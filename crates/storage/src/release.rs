//! Release asset caching: [`ArtifactStore`] methods for GitHub Releases.
//!
//! Releases are keyed by **tag**, not commit — that is what GitHub resolves
//! a release to. The tag is stored verbatim in the database and matched
//! exactly on lookup; the on-disk directory name is a sanitized derivation
//! (see [`crate::paths::sanitize_tag_dir`]), so arbitrary tags like
//! `release/5.3` cache safely on every platform.
//!
//! Keyword tags (`latest-stable`, ...) are moving targets and must be
//! resolved against the GitHub API *before* consulting this cache — they
//! are deliberately never cached here.

use chrono::Utc;
use rusqlite::Connection;

use crate::db;
use crate::error::{Error, IoContext, Result};
use crate::fsx;
use crate::lock::StoreLock;
use crate::paths;
use crate::store::{ArtifactStore, validate_artifact_inputs};
use crate::types::{
    ReleaseMetadata, SourceArtifact, StoreReleaseResult, StoredArtifact, StoredRelease,
};

impl ArtifactStore {
    /// Cache a release's assets and record (or refresh) its metadata,
    /// idempotently.
    ///
    /// Assets already present with their recorded size are reused; damaged
    /// or missing ones are re-copied. The GitHub-reported flags (`draft`,
    /// `prerelease`, `published_at`, ...) are refreshed on every call, so a
    /// release that gets un-drafted updates its cache entry.
    ///
    /// `variant_id` on the input artifacts is ignored — release assets have
    /// no variants.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`], [`Error::SourceFileMissing`],
    /// [`Error::Io`] and [`Error::Database`], with the same semantics as
    /// [`ArtifactStore::store`].
    pub fn store_release(
        &self,
        metadata: &ReleaseMetadata,
        assets: &[SourceArtifact],
    ) -> Result<StoreReleaseResult> {
        paths::validate_project(&metadata.project)?;
        paths::validate_tag(&metadata.tag)?;
        if let Some(version) = &metadata.version {
            paths::validate_version(version)?;
        }
        validate_artifact_inputs(assets)?;

        let _lock = StoreLock::acquire(&self.base_dir)?;
        let now = Utc::now();

        // Reuse the existing directory when the release is already cached;
        // otherwise derive one from the tag (short borrow).
        let (known_assets, dir_rel) = {
            let conn = self.conn();
            match db::releases::by_tag(&conn, &metadata.project, &metadata.tag)? {
                Some(row) => (db::releases::assets_for(&conn, row.id)?, row.dir_rel),
                None => {
                    let tag_dir = paths::sanitize_tag_dir(&metadata.tag);
                    (
                        Vec::new(),
                        paths::release_dir_rel(&metadata.project, &tag_dir),
                    )
                }
            }
        };
        let dir_abs = paths::rel_to_abs(&self.base_dir, &dir_rel);

        // Copy phase (no database lock held).
        std::fs::create_dir_all(&dir_abs)
            .io_ctx(|| format!("failed to create release directory {}", dir_abs.display()))?;
        let mut newly_stored = Vec::new();
        let mut reused = Vec::new();
        let mut records = Vec::with_capacity(assets.len());
        for asset in assets {
            let dest = dir_abs.join(&asset.target_name);
            let intact = known_assets
                .iter()
                .find(|row| row.filename == asset.target_name)
                .and_then(|row| {
                    let expected = u64::try_from(row.size_bytes).ok()?;
                    (fsx::file_size(&dest) == Some(expected)).then_some(fsx::FileDigest {
                        size_bytes: expected,
                        sha256: row.sha256.clone(),
                    })
                });
            match intact {
                Some(digest) => {
                    reused.push(asset.target_name.clone());
                    records.push((asset.target_name.clone(), digest));
                }
                None => {
                    let digest = fsx::copy_file_hashed(&asset.path, &dest)?;
                    newly_stored.push(asset.target_name.clone());
                    records.push((asset.target_name.clone(), digest));
                }
            }
        }

        // Metadata phase — one transaction.
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let (release_id, _authoritative_dir) = db::releases::upsert(
            &tx,
            &metadata.project,
            &metadata.tag,
            metadata.version.as_deref(),
            metadata.prerelease,
            metadata.draft,
            metadata.published_at.map(db::to_ms),
            metadata.target_commitish.as_deref(),
            &dir_rel,
            db::to_ms(now),
        )?;
        for (filename, digest) in &records {
            db::releases::upsert_asset(
                &tx,
                release_id,
                filename,
                db::size_to_db(digest.size_bytes)?,
                &digest.sha256,
            )?;
        }
        tx.commit()?;

        let release = self.assemble_release(&conn, release_id)?;
        Ok(StoreReleaseResult {
            release,
            newly_stored,
            reused,
        })
    }

    /// Find a cached release by its exact tag, health-checked: `None` when
    /// the tag is not cached or any asset file is missing/resized on disk
    /// (the caller re-downloads and [`ArtifactStore::store_release`] heals
    /// the entry).
    pub fn find_release(&self, project: &str, tag: &str) -> Result<Option<StoredRelease>> {
        paths::validate_project(project)?;
        paths::validate_tag(tag)?;
        let conn = self.conn();
        let Some(row) = db::releases::by_tag(&conn, project, tag)? else {
            return Ok(None);
        };
        let release = self.release_from_row(&conn, &row)?;
        if !assets_healthy(&release) {
            tracing::warn!(
                project,
                tag,
                "cached release failed health check; treating as miss"
            );
            return Ok(None);
        }
        if let Err(err) = db::releases::touch(&conn, row.id, db::to_ms(Utc::now())) {
            tracing::warn!(release_id = row.id, error = %err, "failed to update last-used timestamp");
        }
        Ok(Some(release))
    }

    /// Whether a healthy cached copy of the release exists. Unlike
    /// [`ArtifactStore::find_release`] this does not count as usage.
    pub fn has_release(&self, project: &str, tag: &str) -> Result<bool> {
        paths::validate_project(project)?;
        paths::validate_tag(tag)?;
        let conn = self.conn();
        let Some(row) = db::releases::by_tag(&conn, project, tag)? else {
            return Ok(false);
        };
        let release = self.release_from_row(&conn, &row)?;
        Ok(assets_healthy(&release))
    }

    /// Inventory of cached releases for a project, most recently cached
    /// first (no health filtering, no usage touching). Unreadable rows are
    /// skipped with a warning.
    pub fn list_releases(&self, project: &str) -> Result<Vec<StoredRelease>> {
        paths::validate_project(project)?;
        let conn = self.conn();
        let rows = db::releases::list_for_project(&conn, project)?;
        let mut releases = Vec::with_capacity(rows.len());
        for row in rows {
            match self.release_from_row(&conn, &row) {
                Ok(release) => releases.push(release),
                Err(err) => tracing::warn!(error = %err, "skipping unreadable release row"),
            }
        }
        Ok(releases)
    }

    /// Delete one cached release (metadata first, then files). Returns
    /// `false` when the tag is not cached.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the asset directory could not be removed — the
    /// metadata is already gone, so the files are orphans and the next
    /// [`ArtifactStore::gc`](crate::ArtifactStore::gc) reclaims them.
    pub fn delete_release(&self, project: &str, tag: &str) -> Result<bool> {
        paths::validate_project(project)?;
        paths::validate_tag(tag)?;

        let _lock = StoreLock::acquire(&self.base_dir)?;
        let dir_rel = {
            let conn = self.conn();
            let Some(row) = db::releases::by_tag(&conn, project, tag)? else {
                return Ok(false);
            };
            db::releases::delete(&conn, row.id)?;
            row.dir_rel
        };
        // Only remove a directory genuinely inside the store; a tampered
        // `dir_path` is dropped from the index but never followed into a
        // destructive removal (see `paths::resolve_within_base`).
        match paths::resolve_within_base(&self.base_dir, &dir_rel) {
            Some(dir_abs) => {
                fsx::remove_dir_all_if_exists(&dir_abs)?;
                if let Some(parent) = dir_abs.parent() {
                    fsx::remove_empty_parents(parent, &self.base_dir);
                }
            }
            None => tracing::warn!(
                dir_rel = %dir_rel,
                "release record had an out-of-store directory; dropped record without removing files"
            ),
        }
        Ok(true)
    }

    /// Load a full [`StoredRelease`] by row id.
    fn assemble_release(&self, conn: &Connection, id: i64) -> Result<StoredRelease> {
        let row = db::releases::get(conn, id)?.ok_or_else(|| Error::Data {
            details: format!("release row {id} disappeared mid-operation"),
        })?;
        self.release_from_row(conn, &row)
    }

    /// Convert a database row (plus its assets) into the public
    /// [`StoredRelease`] with absolute paths and UTC timestamps.
    ///
    /// Takes the already-held connection — never locks internally, so it is
    /// safe to call while a [`MutexGuard`](std::sync::MutexGuard) is live.
    pub(crate) fn release_from_row(
        &self,
        conn: &Connection,
        row: &db::ReleaseRow,
    ) -> Result<StoredRelease> {
        let conn_assets = db::releases::assets_for(conn, row.id)?;
        let dir = paths::rel_to_abs(&self.base_dir, &row.dir_rel);
        let mut assets = Vec::with_capacity(conn_assets.len());
        for asset in conn_assets {
            assets.push(StoredArtifact {
                path: dir.join(&asset.filename),
                variant_id: None,
                size_bytes: db::size_from_db(asset.size_bytes)?,
                sha256: asset.sha256,
                filename: asset.filename,
            });
        }
        Ok(StoredRelease {
            id: row.id,
            project: row.project.clone(),
            tag: row.tag.clone(),
            version: row.version.clone(),
            prerelease: row.prerelease,
            draft: row.draft,
            published_at: row.published_at_ms.map(db::from_ms).transpose()?,
            target_commitish: row.target_commitish.clone(),
            cached_at: db::from_ms(row.cached_at_ms)?,
            last_used_at: db::from_ms(row.last_used_at_ms)?,
            dir,
            assets,
        })
    }
}

/// Whether every recorded asset exists on disk with its recorded size.
fn assets_healthy(release: &StoredRelease) -> bool {
    !release.assets.is_empty()
        && release
            .assets
            .iter()
            .all(|asset| fsx::file_size(&asset.path) == Some(asset.size_bytes))
}
