//! Releases cache: stores downloaded GitHub Release assets keyed by tag.
//!
//! GitHub Releases resolve to a *tag*, not a commit (the API does not
//! reliably expose the underlying SHA), so cached releases live in their own
//! per-project subtree instead of the commit-keyed build tree:
//!
//! ```text
//! {base_dir}/{project}/releases/{tag}/
//! ├── release-manifest.json
//! └── *.zip
//! ```
//!
//! Lookup flow for the CLI/API: resolve the user's `release:` input to a
//! concrete tag (keywords like `latest-stable` must be resolved against the
//! GitHub API first — they are moving targets and are deliberately never
//! cached), then call [`ArtifactStore::find_release`]; on a miss, download
//! and [`ArtifactStore::store_release`].
//!
//! Same guarantees as the build store: validated inputs, atomic manifest
//! writes, store-wide locking for mutations, integrity-checked reads.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, IoResultExt, Result};
use crate::fsx::{self, StoreLock};
use crate::manifest::{ArtifactEntry, load_json_manifest, save_json_manifest};
use crate::store::{ArtifactStore, SourceArtifact};
use crate::validate;
use crate::verify::{VerifyMode, verify_entries};

/// Manifest for a single cached release.
///
/// Stored as `release-manifest.json` in each release directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// Schema version for forward compatibility.
    pub schema_version: u32,

    /// Project name.
    pub project: String,

    /// Release tag name, verbatim (e.g. `v5.6.0`).
    pub tag: String,

    /// Version derived from the tag (e.g. `5.6.0`).
    pub version: String,

    /// Whether the release is marked as a prerelease on GitHub.
    #[serde(default)]
    pub prerelease: bool,

    /// Whether the release was a draft when cached.
    #[serde(default)]
    pub draft: bool,

    /// When the release was published on GitHub (if known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<DateTime<Utc>>,

    /// The commitish the release targets (if known; GitHub may report a
    /// branch name rather than a SHA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,

    /// When the assets were downloaded into this cache.
    pub cached_at: DateTime<Utc>,

    /// Cached asset files with sizes and checksums.
    pub assets: Vec<ArtifactEntry>,
}

impl ReleaseManifest {
    /// Current schema version.
    pub const CURRENT_SCHEMA: u32 = 1;

    /// Manifest filename.
    pub const FILENAME: &'static str = "release-manifest.json";

    /// Create a new manifest from caller metadata.
    fn new(meta: &ReleaseMetadata) -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA,
            project: meta.project.clone(),
            tag: meta.tag.clone(),
            version: meta.version.clone(),
            prerelease: meta.prerelease,
            draft: meta.draft,
            published_at: meta.published_at,
            target_commitish: meta.target_commitish.clone(),
            cached_at: Utc::now(),
            assets: Vec::new(),
        }
    }

    /// Add an asset, replacing any existing entry with the same filename.
    fn upsert_asset(&mut self, entry: ArtifactEntry) {
        if let Some(existing) = self
            .assets
            .iter_mut()
            .find(|a| a.filename == entry.filename)
        {
            *existing = entry;
        } else {
            self.assets.push(entry);
        }
    }

    /// Load a release manifest from a directory.
    ///
    /// # Errors
    ///
    /// Same contract as [`crate::manifest::BuildManifest::load`]:
    /// `ManifestNotFound`, `ManifestCorrupted`, or `UnsupportedSchema`.
    pub fn load(dir: &std::path::Path) -> Result<Self> {
        let path = dir.join(Self::FILENAME);
        let manifest: Self = load_json_manifest(&path, Self::CURRENT_SCHEMA)?;
        if manifest.project.is_empty() || manifest.tag.is_empty() {
            return Err(Error::corrupted(&path, "empty 'project' or 'tag'"));
        }
        // Same hardening as build manifests: asset filenames from disk must
        // be safe bare filenames, unique within the manifest.
        crate::manifest::validate_entry_filenames(&manifest.assets, &path)?;
        Ok(manifest)
    }

    /// Save the manifest atomically to a directory (created if missing).
    pub fn save(&self, dir: &std::path::Path) -> Result<()> {
        std::fs::create_dir_all(dir)
            .io_ctx(|| format!("creating release directory {}", dir.display()))?;
        save_json_manifest(&dir.join(Self::FILENAME), self)
    }
}

/// Caller-provided metadata describing a release being cached.
///
/// `published_at`, `target_commitish`, `prerelease`, and `draft` come from
/// the GitHub API when available; only `project`, `tag`, and `version` are
/// required.
#[derive(Debug, Clone)]
pub struct ReleaseMetadata {
    /// Project name.
    pub project: String,
    /// Release tag, verbatim.
    pub tag: String,
    /// Version derived from the tag (caller strips `v`/`release-` prefixes).
    pub version: String,
    /// GitHub prerelease flag.
    pub prerelease: bool,
    /// GitHub draft flag.
    pub draft: bool,
    /// GitHub publish timestamp, if known.
    pub published_at: Option<DateTime<Utc>>,
    /// Release target commitish, if known.
    pub target_commitish: Option<String>,
}

impl ReleaseMetadata {
    /// Create metadata with the required fields; optional GitHub metadata
    /// defaults to absent/false and can be set on the struct directly.
    pub fn new(project: String, tag: String, version: String) -> Self {
        Self {
            project,
            tag,
            version,
            prerelease: false,
            draft: false,
            published_at: None,
            target_commitish: None,
        }
    }
}

/// A cached release with its metadata and file paths.
#[derive(Debug)]
pub struct StoredRelease {
    /// Release manifest.
    pub manifest: ReleaseManifest,
    /// Directory where the release assets are cached.
    pub release_dir: PathBuf,
    /// Paths to asset files (aligned with `manifest.assets`).
    pub files: Vec<PathBuf>,
}

impl StoredRelease {
    /// Verify cached files against the manifest at the given level.
    pub fn verify(&self, mode: VerifyMode) -> Result<Vec<crate::verify::VerifyIssue>> {
        verify_entries(&self.release_dir, &self.manifest.assets, mode)
    }

    /// Path to the asset with the given filename, if recorded.
    pub fn file_by_name(&self, filename: &str) -> Option<PathBuf> {
        self.manifest
            .assets
            .iter()
            .find(|a| a.filename == filename)
            .map(|a| self.release_dir.join(&a.filename))
    }
}

/// Result of caching a release.
#[derive(Debug)]
pub struct StoreReleaseResult {
    /// Directory where the assets are cached.
    pub release_dir: PathBuf,
    /// Updated manifest.
    pub manifest: ReleaseManifest,
    /// Files that were copied in by this call.
    pub stored_files: Vec<PathBuf>,
    /// Asset filenames that already existed and were skipped.
    pub skipped_files: Vec<String>,
    /// Whether anything was skipped (already cached).
    pub was_deduplicated: bool,
}

impl ArtifactStore {
    /// Cache release assets for a project, keyed by tag.
    ///
    /// Idempotent: assets whose filename is already recorded *and* whose
    /// file passes a size check are skipped; missing or size-mismatched
    /// files are re-copied (self-healing). Release-level metadata
    /// (prerelease/draft/published_at) is refreshed from `metadata` on every
    /// call, so a re-published release updates its flags.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] for bad names/tags/filenames or an empty asset list
    /// - [`Error::ArtifactNotFound`] if a source file is missing
    /// - IO errors with context
    pub fn store_release(
        &self,
        assets: &[SourceArtifact],
        metadata: &ReleaseMetadata,
    ) -> Result<StoreReleaseResult> {
        validate::validate_project(&metadata.project)?;
        validate::validate_tag(&metadata.tag)?;
        validate::validate_version(&metadata.version)?;
        if assets.is_empty() {
            return Err(Error::InvalidInput(
                "cannot cache a release with no assets".to_string(),
            ));
        }
        validate_unique_filenames(assets)?;
        for asset in assets {
            validate::validate_artifact_filename(&asset.target_name)?;
        }

        // Serialize all store mutations (cross-thread and cross-process).
        let _lock = StoreLock::acquire(self.base_dir())?;
        self.ensure_store_marker()?;

        let release_dir = self.paths().release_dir(&metadata.project, &metadata.tag);
        std::fs::create_dir_all(&release_dir)
            .io_ctx(|| format!("creating release directory {}", release_dir.display()))?;
        fsx::remove_stale_temps(&release_dir);

        // Load the existing manifest. A corrupted one is rebuilt from
        // scratch (this store call re-establishes a consistent state), but a
        // newer-schema manifest is never clobbered.
        let mut manifest = match ReleaseManifest::load(&release_dir) {
            Ok(m) => m,
            Err(Error::ManifestNotFound(_)) => ReleaseManifest::new(metadata),
            Err(Error::ManifestCorrupted { path, reason }) => {
                tracing::warn!(
                    "rebuilding corrupted release manifest {} ({reason})",
                    path.display()
                );
                ReleaseManifest::new(metadata)
            }
            Err(e) => return Err(e),
        };

        // Refresh release-level metadata (a re-published release may have
        // flipped prerelease/draft or gained a publish date).
        manifest.version = metadata.version.clone();
        manifest.prerelease = metadata.prerelease;
        manifest.draft = metadata.draft;
        manifest.published_at = metadata.published_at;
        manifest.target_commitish = metadata.target_commitish.clone();

        let mut stored_files = Vec::new();
        let mut skipped_files = Vec::new();

        for asset in assets {
            // Skip only when the manifest entry exists AND the file on disk
            // passes a size check — otherwise re-copy (self-healing).
            if let Some(entry) = manifest
                .assets
                .iter()
                .find(|a| a.filename == asset.target_name)
            {
                let existing = release_dir.join(&entry.filename);
                if verify_entries(&release_dir, std::slice::from_ref(entry), VerifyMode::Size)?
                    .is_empty()
                {
                    skipped_files.push(asset.target_name.clone());
                    tracing::debug!("release asset already cached: {}", existing.display());
                    continue;
                }
                tracing::warn!(
                    "re-caching damaged or missing release asset {}",
                    existing.display()
                );
            }

            if !asset.path.exists() {
                return Err(Error::ArtifactNotFound(asset.path.clone()));
            }

            let target_path = release_dir.join(&asset.target_name);
            let (size, sha256) = fsx::copy_file_atomic_hashed(&asset.path, &target_path)?;

            manifest.upsert_asset(ArtifactEntry {
                variant_id: asset.variant_id.clone(),
                filename: asset.target_name.clone(),
                size_bytes: size,
                sha256,
            });
            stored_files.push(target_path);
        }

        manifest.save(&release_dir)?;

        let was_deduplicated = !skipped_files.is_empty();
        tracing::info!(
            "Cached release: {} {} ({} files stored, {} skipped)",
            metadata.project,
            metadata.tag,
            stored_files.len(),
            skipped_files.len()
        );

        Ok(StoreReleaseResult {
            release_dir,
            manifest,
            stored_files,
            skipped_files,
            was_deduplicated,
        })
    }

    /// Find a cached release by exact tag.
    ///
    /// Returns `Ok(None)` when the release is not cached, when its manifest
    /// is unreadable (logged), or when any cached file fails a size check —
    /// an unhealthy cache entry is treated as a miss so callers re-download,
    /// and [`ArtifactStore::store_release`] self-heals the entry.
    pub fn find_release(&self, project: &str, tag: &str) -> Result<Option<StoredRelease>> {
        validate::validate_project(project)?;
        validate::validate_tag(tag)?;

        let release_dir = self.paths().release_dir(project, tag);
        let manifest = match ReleaseManifest::load(&release_dir) {
            Ok(m) => m,
            Err(Error::ManifestNotFound(_)) => return Ok(None),
            Err(e) => {
                tracing::warn!("ignoring unreadable release manifest: {e}");
                return Ok(None);
            }
        };

        // Defense-in-depth: the directory is keyed by sanitized tag, the
        // manifest records the exact tag — they must agree.
        if manifest.tag != tag {
            tracing::warn!(
                "release directory {} contains manifest for tag '{}' (requested '{tag}'); \
                 treating as miss",
                release_dir.display(),
                manifest.tag
            );
            return Ok(None);
        }

        let issues = verify_entries(&release_dir, &manifest.assets, VerifyMode::Size)?;
        if !issues.is_empty() {
            for issue in &issues {
                tracing::warn!(
                    "cached release {project} {tag} failed verification: {} ({})",
                    issue.filename,
                    issue.reason
                );
            }
            return Ok(None);
        }

        let files = manifest
            .assets
            .iter()
            .map(|a| release_dir.join(&a.filename))
            .collect();

        Ok(Some(StoredRelease {
            manifest,
            release_dir,
            files,
        }))
    }

    /// Check whether a healthy cached release exists for `tag`.
    pub fn has_release(&self, project: &str, tag: &str) -> Result<bool> {
        Ok(self.find_release(project, tag)?.is_some())
    }

    /// List all cached releases for a project, newest cached first.
    ///
    /// Unreadable entries are skipped with a warning; files are not
    /// integrity-checked here (this is an inventory listing — use
    /// [`StoredRelease::verify`] for that).
    pub fn list_releases(&self, project: &str) -> Result<Vec<StoredRelease>> {
        validate::validate_project(project)?;

        let releases_dir = self.paths().releases_dir(project);
        let mut releases = Vec::new();

        let entries = match std::fs::read_dir(&releases_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(releases),
            Err(e) => {
                return Err(Error::io(
                    format!("reading releases directory {}", releases_dir.display()),
                    e,
                ));
            }
        };

        for entry in entries {
            let entry = entry.io_ctx(|| format!("reading entry in {}", releases_dir.display()))?;
            if !entry
                .file_type()
                .io_ctx(|| format!("inspecting {}", entry.path().display()))?
                .is_dir()
            {
                continue;
            }
            let dir = entry.path();
            match ReleaseManifest::load(&dir) {
                Ok(manifest) => {
                    let files = manifest
                        .assets
                        .iter()
                        .map(|a| dir.join(&a.filename))
                        .collect();
                    releases.push(StoredRelease {
                        manifest,
                        release_dir: dir,
                        files,
                    });
                }
                Err(e) => {
                    tracing::warn!("skipping unreadable release entry {}: {e}", dir.display());
                }
            }
        }

        releases.sort_by_key(|r| std::cmp::Reverse(r.manifest.cached_at));
        Ok(releases)
    }

    /// Delete a cached release. Returns `true` if something was deleted.
    pub fn delete_release(&self, project: &str, tag: &str) -> Result<bool> {
        validate::validate_project(project)?;
        validate::validate_tag(tag)?;

        let _lock = StoreLock::acquire(self.base_dir())?;

        let release_dir = self.paths().release_dir(project, tag);
        if !release_dir.exists() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&release_dir)
            .io_ctx(|| format!("deleting cached release {}", release_dir.display()))?;
        tracing::info!("Deleted cached release: {}", release_dir.display());

        // Remove the releases/ dir (and empty parents) if nothing is left.
        // Unlocked variant: this call already holds the (non-reentrant)
        // store lock.
        self.cleanup_empty_dirs_unlocked(project)?;
        Ok(true)
    }
}

/// Reject duplicate target filenames within a single store call — the second
/// copy would silently overwrite the first.
pub(crate) fn validate_unique_filenames(artifacts: &[SourceArtifact]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for artifact in artifacts {
        if !seen.insert(artifact.target_name.as_str()) {
            return Err(Error::InvalidInput(format!(
                "duplicate target filename '{}' in one store call",
                artifact.target_name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper: create a store backed by a temp directory.
    fn temp_store() -> (TempDir, ArtifactStore) {
        let dir = TempDir::new().unwrap();
        let store = ArtifactStore::new(dir.path().join("store"));
        (dir, store)
    }

    /// Helper: create a dummy asset file.
    fn make_asset(dir: &std::path::Path, name: &str, content: &str) -> SourceArtifact {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        SourceArtifact {
            variant_id: None,
            path,
            target_name: name.to_string(),
        }
    }

    fn meta(project: &str, tag: &str, version: &str) -> ReleaseMetadata {
        ReleaseMetadata::new(project.to_string(), tag.to_string(), version.to_string())
    }

    #[test]
    fn test_store_and_find_release() {
        let (dir, store) = temp_store();
        let asset = make_asset(dir.path(), "backwpup-5.6.0.zip", "zip bytes");

        let result = store
            .store_release(&[asset], &meta("backwpup", "v5.6.0", "5.6.0"))
            .unwrap();
        assert_eq!(result.stored_files.len(), 1);
        assert!(!result.was_deduplicated);
        assert_eq!(result.manifest.assets.len(), 1);
        assert_eq!(result.manifest.assets[0].sha256.len(), 64);

        let found = store.find_release("backwpup", "v5.6.0").unwrap().unwrap();
        assert_eq!(found.manifest.tag, "v5.6.0");
        assert_eq!(found.manifest.version, "5.6.0");
        assert_eq!(found.files.len(), 1);
        assert!(found.files[0].exists());
    }

    #[test]
    fn test_find_release_miss() {
        let (_dir, store) = temp_store();
        assert!(store.find_release("backwpup", "v9.9.9").unwrap().is_none());
        assert!(!store.has_release("backwpup", "v9.9.9").unwrap());
    }

    #[test]
    fn test_store_release_deduplicates() {
        let (dir, store) = temp_store();
        let a1 = make_asset(dir.path(), "plugin.zip", "bytes");
        store
            .store_release(&[a1], &meta("backwpup", "v5.6.0", "5.6.0"))
            .unwrap();

        let a2 = make_asset(dir.path(), "plugin.zip", "bytes");
        let result = store
            .store_release(&[a2], &meta("backwpup", "v5.6.0", "5.6.0"))
            .unwrap();
        assert!(result.was_deduplicated);
        assert_eq!(result.skipped_files, vec!["plugin.zip".to_string()]);
        assert!(result.stored_files.is_empty());
    }

    #[test]
    fn test_store_release_self_heals_deleted_asset() {
        let (dir, store) = temp_store();
        let a1 = make_asset(dir.path(), "plugin.zip", "bytes");
        let result = store
            .store_release(&[a1], &meta("backwpup", "v5.6.0", "5.6.0"))
            .unwrap();

        // Damage the cache: delete the stored file behind the manifest.
        std::fs::remove_file(&result.stored_files[0]).unwrap();
        // find_release treats it as a miss...
        assert!(store.find_release("backwpup", "v5.6.0").unwrap().is_none());

        // ...and re-storing repairs it instead of skipping.
        let a2 = make_asset(dir.path(), "plugin.zip", "bytes");
        let result = store
            .store_release(&[a2], &meta("backwpup", "v5.6.0", "5.6.0"))
            .unwrap();
        assert_eq!(result.stored_files.len(), 1);
        assert!(store.find_release("backwpup", "v5.6.0").unwrap().is_some());
    }

    #[test]
    fn test_store_release_updates_metadata_flags() {
        let (dir, store) = temp_store();
        let a1 = make_asset(dir.path(), "plugin.zip", "bytes");
        let mut m = meta("backwpup", "v5.6.0", "5.6.0");
        m.prerelease = true;
        store.store_release(&[a1], &m).unwrap();

        // Release later marked stable: flags refresh on re-store.
        let a2 = make_asset(dir.path(), "plugin.zip", "bytes");
        let mut m = meta("backwpup", "v5.6.0", "5.6.0");
        m.prerelease = false;
        let result = store.store_release(&[a2], &m).unwrap();
        assert!(!result.manifest.prerelease);
    }

    #[test]
    fn test_store_release_rejects_bad_input() {
        let (dir, store) = temp_store();
        let asset = make_asset(dir.path(), "plugin.zip", "bytes");

        // Empty asset list.
        assert!(
            store
                .store_release(&[], &meta("backwpup", "v1.0.0", "1.0.0"))
                .is_err()
        );
        // Bad project.
        assert!(
            store
                .store_release(
                    std::slice::from_ref(&asset),
                    &meta("../evil", "v1.0.0", "1.0.0")
                )
                .is_err()
        );
        // Bad tag.
        assert!(
            store
                .store_release(std::slice::from_ref(&asset), &meta("backwpup", "", "1.0.0"))
                .is_err()
        );
        // Manifest-shadowing filename.
        let evil = SourceArtifact {
            variant_id: None,
            path: asset.path.clone(),
            target_name: ReleaseManifest::FILENAME.to_string(),
        };
        assert!(
            store
                .store_release(&[evil], &meta("backwpup", "v1.0.0", "1.0.0"))
                .is_err()
        );
    }

    #[test]
    fn test_store_release_rejects_duplicate_filenames() {
        let (dir, store) = temp_store();
        let a1 = make_asset(dir.path(), "plugin.zip", "one");
        let mut a2 = make_asset(dir.path(), "other.zip", "two");
        a2.target_name = "plugin.zip".to_string();

        let err = store
            .store_release(&[a1, a2], &meta("backwpup", "v1.0.0", "1.0.0"))
            .unwrap_err();
        assert!(err.to_string().contains("duplicate target filename"));
    }

    #[test]
    fn test_release_tag_with_separator_is_sanitized() {
        let (dir, store) = temp_store();
        let asset = make_asset(dir.path(), "plugin.zip", "bytes");

        store
            .store_release(&[asset], &meta("backwpup", "release/5.6", "5.6"))
            .unwrap();

        // Cached under a sanitized dir inside releases/, retrievable by the
        // exact original tag.
        let found = store.find_release("backwpup", "release/5.6").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().manifest.tag, "release/5.6");
        // And a similarly-sanitizing but different tag is NOT confused with it.
        assert!(
            store
                .find_release("backwpup", "release-5.6")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_list_releases_sorted_and_skips_broken() {
        let (dir, store) = temp_store();
        let a1 = make_asset(dir.path(), "p1.zip", "one");
        store
            .store_release(&[a1], &meta("backwpup", "v1.0.0", "1.0.0"))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let a2 = make_asset(dir.path(), "p2.zip", "two");
        store
            .store_release(&[a2], &meta("backwpup", "v2.0.0", "2.0.0"))
            .unwrap();

        // A broken (manifest-less) directory is skipped, not fatal.
        std::fs::create_dir_all(store.paths().releases_dir("backwpup").join("junk")).unwrap();

        let releases = store.list_releases("backwpup").unwrap();
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].manifest.tag, "v2.0.0"); // newest cached first
    }

    #[test]
    fn test_list_releases_empty_project() {
        let (_dir, store) = temp_store();
        assert!(store.list_releases("backwpup").unwrap().is_empty());
    }

    #[test]
    fn test_delete_release() {
        let (dir, store) = temp_store();
        let asset = make_asset(dir.path(), "plugin.zip", "bytes");
        store
            .store_release(&[asset], &meta("backwpup", "v1.0.0", "1.0.0"))
            .unwrap();

        assert!(store.delete_release("backwpup", "v1.0.0").unwrap());
        assert!(store.find_release("backwpup", "v1.0.0").unwrap().is_none());
        // Second delete is a clean no-op.
        assert!(!store.delete_release("backwpup", "v1.0.0").unwrap());
        // Empty project tree is cleaned up.
        assert!(!store.paths().project_dir("backwpup").exists());
    }

    #[test]
    fn test_stored_release_file_by_name_and_verify() {
        let (dir, store) = temp_store();
        let asset = make_asset(dir.path(), "plugin.zip", "bytes");
        store
            .store_release(&[asset], &meta("backwpup", "v1.0.0", "1.0.0"))
            .unwrap();

        let release = store.find_release("backwpup", "v1.0.0").unwrap().unwrap();
        assert!(release.file_by_name("plugin.zip").is_some());
        assert!(release.file_by_name("nope.zip").is_none());
        assert!(release.verify(VerifyMode::Checksum).unwrap().is_empty());
    }

    #[test]
    fn test_release_manifest_load_rejects_tampered_asset_names() {
        let (dir, store) = temp_store();
        let asset = make_asset(dir.path(), "plugin.zip", "bytes");
        let result = store
            .store_release(&[asset], &meta("backwpup", "v1.0.0", "1.0.0"))
            .unwrap();

        // Tamper: rewrite the manifest with a traversal filename.
        let manifest_path = result.release_dir.join(ReleaseManifest::FILENAME);
        let tampered = std::fs::read_to_string(&manifest_path)
            .unwrap()
            .replace("plugin.zip", "../../escape.zip");
        std::fs::write(&manifest_path, tampered).unwrap();

        // The tampered entry is treated as corrupted → cache miss, and the
        // next store rebuilds the manifest cleanly.
        assert!(store.find_release("backwpup", "v1.0.0").unwrap().is_none());
        let asset = make_asset(dir.path(), "plugin.zip", "bytes");
        store
            .store_release(&[asset], &meta("backwpup", "v1.0.0", "1.0.0"))
            .unwrap();
        assert!(store.find_release("backwpup", "v1.0.0").unwrap().is_some());
    }

    #[test]
    fn test_releases_do_not_pollute_version_listing() {
        let (dir, store) = temp_store();
        let asset = make_asset(dir.path(), "plugin.zip", "bytes");
        store
            .store_release(&[asset], &meta("backwpup", "v1.0.0", "1.0.0"))
            .unwrap();

        // The releases/ subtree must not appear as versions.
        assert!(store.list_versions("backwpup").unwrap().is_empty());
        assert_eq!(store.list_projects().unwrap(), vec!["backwpup".to_string()]);
    }
}
