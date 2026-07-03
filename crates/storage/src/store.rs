//! Artifact storage manager.
//!
//! Manages storing, retrieving, and querying build artifacts with
//! deduplication.
//!
//! # Directory Structure
//!
//! ```text
//! {base_dir}/{project}/{major.minor}/{version}/
//! ├── by-commit/{commit_short}/  - actual files + build-manifest.json
//! └── by-source/{source}/        - directory containing commit links
//!     ├── {commit1} → ../../by-commit/{commit1}
//!     └── {commit2} → ../../by-commit/{commit2}
//! {base_dir}/{project}/releases/{tag}/ - cached release assets (see crate::release)
//! ```
//!
//! # Consistency & concurrency
//!
//! - All caller input that becomes a path component is validated
//!   (`validate.rs`) — nothing can escape `base_dir` or shadow
//!   metadata files.
//! - Manifests and artifacts are written atomically (temp + fsync + rename,
//!   see `fsx.rs`); readers never observe partial files.
//! - Mutations take an exclusive store-wide lock (cross-process, via
//!   `File::lock`); reads are lock-free because atomic renames keep them
//!   consistent.
//! - Reads are integrity-checked (manifest-recorded sizes) and treat
//!   damaged entries as misses; re-storing self-heals them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoResultExt, Result};
use crate::fsx::{self, StoreLock};
use crate::link::create_dir_link;
use crate::manifest::{ArtifactEntry, BuildManifest, short_commit};
use crate::path::{BuildSource, PathBuilder, RELEASES_DIR, compare_versions, major_minor};
use crate::query::BuildQuery;
use crate::validate;
use crate::verify::{VerifyIssue, VerifyMode, verify_entries};

/// Store marker filename at the base directory root.
///
/// Identifies a directory as an APVM store and records the layout schema so
/// a future version can migrate (or refuse to write) rather than silently
/// mixing layouts.
const STORE_MARKER: &str = ".apvm-store.json";

/// Current store layout schema version.
const STORE_SCHEMA: u32 = 1;

/// Content of the store marker file.
#[derive(Debug, Serialize, Deserialize)]
struct StoreMarker {
    /// Layout schema version of this store directory.
    store_schema_version: u32,
}

/// Manages artifact storage with deduplication.
///
/// Cheap to construct and to clone conceptually (it only holds the base
/// path); all state lives on disk. Safe to use from multiple threads and
/// processes concurrently — see the module docs for the model.
pub struct ArtifactStore {
    paths: PathBuilder,
}

impl ArtifactStore {
    /// Create a new artifact store with the given base directory.
    ///
    /// The directory is created lazily on the first mutation; constructing
    /// the store never touches the filesystem.
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            paths: PathBuilder::new(base_dir),
        }
    }

    /// Get the path builder.
    pub fn paths(&self) -> &PathBuilder {
        &self.paths
    }

    /// Get the base directory.
    pub fn base_dir(&self) -> &Path {
        self.paths.base_dir()
    }

    // =========================================================================
    // Store
    // =========================================================================

    /// Store build artifacts with deduplication.
    ///
    /// Idempotent and self-healing: artifacts whose filename is already
    /// recorded in the commit manifest *and* whose file passes a size check
    /// are skipped; missing or damaged files are re-copied. A source link is
    /// always created/updated, and the source's `linked_at` timestamp is
    /// refreshed.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] for invalid names/versions/commits/filenames
    ///   or an empty artifact list
    /// - [`Error::ArtifactNotFound`] if a source file does not exist
    /// - [`Error::CommitCollision`] if the short-commit directory already
    ///   holds a build for a *different* full commit
    /// - [`Error::UnsupportedSchema`] if the existing manifest was written
    ///   by a newer crate version (never overwritten)
    pub fn store(
        &self,
        source_artifacts: &[SourceArtifact],
        metadata: &BuildMetadata,
    ) -> Result<StoreResult> {
        // ---- Validate everything before touching the filesystem ----
        validate::validate_project(&metadata.project)?;
        validate::validate_version(&metadata.version)?;
        let commit = validate::validate_commit(&metadata.commit)?;
        if source_artifacts.is_empty() {
            return Err(Error::InvalidInput(
                "cannot store a build with no artifacts".to_string(),
            ));
        }
        crate::release::validate_unique_filenames(source_artifacts)?;
        for artifact in source_artifacts {
            validate::validate_artifact_filename(&artifact.target_name)?;
        }
        // Derived here rather than trusted from the metadata struct, so an
        // inconsistent caller-set `commit_short` can never split a commit
        // across two directories.
        let commit_short = short_commit(&commit);

        // ---- Serialize mutations across threads and processes ----
        let _lock = StoreLock::acquire(self.base_dir())?;
        self.ensure_store_marker()?;

        let commit_dir = self
            .paths
            .commit_dir(&metadata.project, &metadata.version, &commit_short);
        std::fs::create_dir_all(&commit_dir)
            .io_ctx(|| format!("creating commit directory {}", commit_dir.display()))?;
        fsx::remove_stale_temps(&commit_dir);

        // Load the existing manifest. Corrupted manifests are rebuilt (this
        // call re-establishes consistency); newer-schema manifests error out.
        let existing = self.load_manifest_for_write(&commit_dir)?;

        // Short-hash collision check: same directory must mean same commit.
        // Prefix comparison accepts the same commit given at different
        // precision (7-char short vs 40-char full).
        if let Some(m) = &existing
            && m.commit != commit
            && !m.commit.starts_with(&commit)
            && !commit.starts_with(&m.commit)
        {
            return Err(Error::CommitCollision {
                dir: commit_dir,
                existing: m.commit.clone(),
                incoming: commit,
            });
        }

        // Whether a valid manifest existed before this call: decides both
        // the collision-free base manifest and whether a failure below may
        // clean up the whole directory (fresh dirs only — never previously
        // valid data).
        let is_fresh = existing.is_none();
        let mut manifest = existing.unwrap_or_else(|| {
            BuildManifest::new(
                metadata.project.clone(),
                metadata.version.clone(),
                commit.clone(),
            )
        });
        // Upgrade to the more precise hash if the caller provided one.
        if commit.len() > manifest.commit.len() {
            manifest.commit = commit.clone();
        }

        let mut stored_files = Vec::new();
        let mut skipped_variants = Vec::new();

        // Copy artifacts and save the manifest inside one fallible block so
        // a failure can roll back a *fresh* directory (a dir without a valid
        // manifest is invisible to readers but would show up as a phantom
        // entry in `list_commits`).
        let copy_result = (|| -> Result<()> {
            for artifact in source_artifacts {
                // Dedupe by filename (the file's physical identity), but
                // only when the stored file is healthy — a deleted or
                // truncated file is re-copied (self-healing).
                if let Some(entry) = manifest.artifact_by_filename(&artifact.target_name) {
                    if verify_entries(&commit_dir, std::slice::from_ref(entry), VerifyMode::Size)?
                        .is_empty()
                    {
                        skipped_variants.push(artifact.variant_id.clone());
                        tracing::debug!(
                            "artifact already stored: {}",
                            commit_dir.join(&entry.filename).display()
                        );
                        continue;
                    }
                    tracing::warn!(
                        "re-storing damaged or missing artifact {}",
                        commit_dir.join(&entry.filename).display()
                    );
                }

                if !artifact.path.exists() {
                    return Err(Error::ArtifactNotFound(artifact.path.clone()));
                }

                let target_path = commit_dir.join(&artifact.target_name);
                // Streamed copy: checksum is computed over the bytes written,
                // so manifest and file can never disagree at store time.
                let (size, sha256) = fsx::copy_file_atomic_hashed(&artifact.path, &target_path)?;

                manifest.upsert_artifact(ArtifactEntry {
                    variant_id: artifact.variant_id.clone(),
                    filename: artifact.target_name.clone(),
                    size_bytes: size,
                    sha256,
                });
                stored_files.push(target_path);

                tracing::debug!("Stored artifact: {} ({} bytes)", artifact.target_name, size);
            }

            // Record/refresh the source *before* saving, so the manifest and
            // the link created below can never disagree about known sources.
            let branch = (!metadata.branch.is_empty()).then(|| metadata.branch.clone());
            manifest.add_source(metadata.source.clone(), branch);
            manifest.save(&commit_dir)
        })();

        if let Err(e) = copy_result {
            if is_fresh {
                // Best-effort rollback: the dir holds only this call's
                // partial output (there was no valid manifest before), so
                // removing it cannot destroy previously stored data.
                if let Err(cleanup_err) = std::fs::remove_dir_all(&commit_dir) {
                    tracing::debug!(
                        "could not roll back partial commit dir {}: {cleanup_err}",
                        commit_dir.display()
                    );
                }
            }
            return Err(e);
        }

        self.create_source_link(metadata, &commit_short)?;

        let was_deduplicated = !skipped_variants.is_empty();
        tracing::info!(
            "Stored build: {} v{} @ {} ({} files, {} skipped)",
            metadata.project,
            metadata.version,
            commit_short,
            stored_files.len(),
            skipped_variants.len()
        );

        Ok(StoreResult {
            commit_dir,
            manifest,
            stored_files,
            skipped_variants,
            was_deduplicated,
        })
    }

    /// Create/refresh the link from a source directory to a commit directory.
    fn create_source_link(&self, metadata: &BuildMetadata, commit_short: &str) -> Result<()> {
        let link_path = self.paths.source_link(
            &metadata.project,
            &metadata.version,
            &metadata.source,
            commit_short,
        );

        // Relative target on Unix keeps the store relocatable; junctions on
        // Windows require absolute targets.
        #[cfg(unix)]
        let target = self.paths.relative_commit_path(commit_short);

        #[cfg(windows)]
        let target = self
            .paths
            .commit_dir(&metadata.project, &metadata.version, commit_short);

        create_dir_link(&target, &link_path)?;

        tracing::debug!(
            "Created source link: {} -> {}",
            link_path.display(),
            target.display()
        );

        Ok(())
    }

    /// Ensure the store marker exists and its schema is supported.
    ///
    /// Called under the store lock by operations that write data in the
    /// current layout ([`Self::store`], [`Self::store_release`]).
    pub(crate) fn ensure_store_marker(&self) -> Result<()> {
        let path = self.base_dir().join(STORE_MARKER);
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<StoreMarker>(&content) {
                Ok(marker) if marker.store_schema_version > STORE_SCHEMA => {
                    Err(Error::UnsupportedSchema {
                        path,
                        found: marker.store_schema_version,
                        supported: STORE_SCHEMA,
                    })
                }
                Ok(_) => Ok(()),
                Err(e) => {
                    // A corrupt marker is repaired: the layout itself is
                    // self-describing enough that this is metadata, not state.
                    tracing::warn!("rewriting corrupt store marker {}: {e}", path.display());
                    self.write_store_marker(&path)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.write_store_marker(&path),
            Err(e) => Err(Error::io(
                format!("reading store marker {}", path.display()),
                e,
            )),
        }
    }

    /// Write the store marker with the current schema version.
    fn write_store_marker(&self, path: &Path) -> Result<()> {
        let content = serde_json::to_vec_pretty(&StoreMarker {
            store_schema_version: STORE_SCHEMA,
        })?;
        fsx::atomic_write(path, &content)
    }

    /// Load a manifest for a write operation.
    ///
    /// Missing → `Ok(None)` (fresh build). Corrupted → warn + `Ok(None)`
    /// (the write re-establishes consistency). Newer schema → error (never
    /// clobber data written by a newer version). IO problems → error.
    fn load_manifest_for_write(&self, commit_dir: &Path) -> Result<Option<BuildManifest>> {
        match BuildManifest::load(commit_dir) {
            Ok(m) => Ok(Some(m)),
            Err(Error::ManifestNotFound(_)) => Ok(None),
            Err(Error::ManifestCorrupted { path, reason }) => {
                tracing::warn!(
                    "rebuilding corrupted manifest {} ({reason})",
                    path.display()
                );
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    // =========================================================================
    // Find
    // =========================================================================

    /// Find builds by source (PR, tag, branch, commit).
    ///
    /// Returns healthy builds only (see [`Self::find_by_commit`] for the
    /// integrity rules), sorted by when this source last pointed at each
    /// build (newest first) — for a branch that was reverted to an older
    /// commit and rebuilt, the older commit correctly sorts first.
    pub fn find_by_source(
        &self,
        project: &str,
        version: &str,
        source: &BuildSource,
    ) -> Result<Vec<StoredBuild>> {
        validate::validate_project(project)?;
        validate::validate_version(version)?;

        let source_dir = self.paths.source_dir(project, version, source);
        let mut builds = Vec::new();

        for commit_name in read_dir_names(&source_dir)? {
            // Skip hidden strays (e.g. macOS `.DS_Store`) — real entries are
            // commit shorts, which never start with a dot.
            if commit_name.starts_with('.') {
                continue;
            }
            // Entry names in a source directory are the commit shorts the
            // links point at; resolving through by-commit (instead of
            // following the links) sidesteps platform link semantics.
            let commit_dir = self.paths.commit_dir(project, version, &commit_name);
            if let Some(build) = self.load_stored_build(&commit_dir, Some(VerifyMode::Size))? {
                builds.push(build);
            }
        }

        // Most recent link first; fall back to build time if (unexpectedly)
        // the manifest lost the source entry.
        builds.sort_by_key(|b| {
            std::cmp::Reverse(
                b.manifest
                    .source_linked_at(source)
                    .unwrap_or(b.manifest.built_at),
            )
        });

        Ok(builds)
    }

    /// Find the latest build for a source (by most recent link time).
    pub fn find_latest_by_source(
        &self,
        project: &str,
        version: &str,
        source: &BuildSource,
    ) -> Result<Option<StoredBuild>> {
        Ok(self
            .find_by_source(project, version, source)?
            .into_iter()
            .next())
    }

    /// Find a build by commit (short or full hash, any case).
    ///
    /// Returns `Ok(None)` when the build is absent, its manifest is
    /// unreadable, its files fail a size check (all logged), or a full hash
    /// was given that does not match the stored build's full hash.
    pub fn find_by_commit(
        &self,
        project: &str,
        version: &str,
        commit: &str,
    ) -> Result<Option<StoredBuild>> {
        Ok(self
            .find_by_commit_raw(project, version, commit)?
            .filter(|build| self.is_healthy(build)))
    }

    /// Like [`Self::find_by_commit`] but without the integrity check.
    ///
    /// Used by lookup logic that needs to distinguish "not cached" from
    /// "cached but damaged".
    pub(crate) fn find_by_commit_raw(
        &self,
        project: &str,
        version: &str,
        commit: &str,
    ) -> Result<Option<StoredBuild>> {
        validate::validate_project(project)?;
        validate::validate_version(version)?;
        let commit = validate::validate_commit(commit)?;
        let commit_short = short_commit(&commit);

        let commit_dir = self.paths.commit_dir(project, version, &commit_short);
        let Some(build) = self.load_stored_build(&commit_dir, None)? else {
            return Ok(None);
        };

        // A 7-char directory hit alone is not proof of identity: the full
        // hash recorded in the manifest must be prefix-compatible with the
        // requested commit (handles both short-vs-full and full-vs-short).
        if !build.manifest.commit.starts_with(&commit)
            && !commit.starts_with(&build.manifest.commit)
        {
            tracing::warn!(
                "commit directory {} holds build for {} which does not match requested {commit}",
                commit_dir.display(),
                build.manifest.commit
            );
            return Ok(None);
        }

        Ok(Some(build))
    }

    /// Find all healthy builds of a commit across every version of a
    /// project, sorted by build time (newest first).
    ///
    /// This is what powers lenient version matching: "is this commit cached
    /// under *any* version?"
    pub fn find_commit_in_versions(&self, project: &str, commit: &str) -> Result<Vec<StoredBuild>> {
        let mut builds = self.find_commit_in_versions_raw(project, commit)?;
        builds.retain(|build| self.is_healthy(build));
        Ok(builds)
    }

    /// Like [`Self::find_commit_in_versions`] but without integrity checks.
    ///
    /// Used by lookup logic that must report a damaged cached build as
    /// [`crate::lookup::MissReason::Incomplete`] rather than "not cached".
    pub(crate) fn find_commit_in_versions_raw(
        &self,
        project: &str,
        commit: &str,
    ) -> Result<Vec<StoredBuild>> {
        validate::validate_project(project)?;
        validate::validate_commit(commit)?;

        let mut builds = Vec::new();
        for version in self.list_versions(project)? {
            if let Some(build) = self.find_by_commit_raw(project, &version, commit)? {
                builds.push(build);
            }
        }
        builds.sort_by_key(|b| std::cmp::Reverse(b.manifest.built_at));
        Ok(builds)
    }

    /// Check a loaded build's files against its manifest (size level),
    /// logging any issues.
    fn is_healthy(&self, build: &StoredBuild) -> bool {
        match verify_entries(
            &build.commit_dir,
            &build.manifest.artifacts,
            VerifyMode::Size,
        ) {
            Ok(issues) if issues.is_empty() => true,
            Ok(issues) => {
                for issue in &issues {
                    tracing::warn!(
                        "stored build {} failed verification: {} ({})",
                        build.commit_dir.display(),
                        issue.filename,
                        issue.reason
                    );
                }
                false
            }
            Err(e) => {
                tracing::warn!("could not verify build {}: {e}", build.commit_dir.display());
                false
            }
        }
    }

    /// Load a stored build from a commit directory.
    ///
    /// Read-lenient: missing, corrupted, or newer-schema manifests yield
    /// `Ok(None)` (with a warning for the latter two) so one bad entry never
    /// breaks listings; genuine IO errors propagate. When `verify` is set,
    /// builds failing that level also yield `Ok(None)`.
    pub(crate) fn load_stored_build(
        &self,
        commit_dir: &Path,
        verify: Option<VerifyMode>,
    ) -> Result<Option<StoredBuild>> {
        let manifest = match BuildManifest::load(commit_dir) {
            Ok(m) => m,
            Err(Error::ManifestNotFound(_)) => return Ok(None),
            Err(e @ (Error::ManifestCorrupted { .. } | Error::UnsupportedSchema { .. })) => {
                tracing::warn!("skipping unreadable build manifest: {e}");
                return Ok(None);
            }
            Err(e) => return Err(e),
        };

        let files = manifest
            .artifacts
            .iter()
            .map(|a| commit_dir.join(&a.filename))
            .collect();

        let build = StoredBuild {
            manifest,
            commit_dir: commit_dir.to_path_buf(),
            files,
        };

        if verify.is_some() && !self.is_healthy(&build) {
            return Ok(None);
        }

        Ok(Some(build))
    }

    // =========================================================================
    // Variants
    // =========================================================================

    /// Get the variants present (and healthy) for a commit.
    ///
    /// A variant only counts as existing when **all** of its files pass a
    /// size check — a half-stored or damaged variant must be rebuilt, not
    /// trusted. Duplicates are removed.
    pub fn get_existing_variants(
        &self,
        project: &str,
        version: &str,
        commit: &str,
    ) -> Result<Vec<Option<String>>> {
        let Some(build) = self.find_by_commit_raw(project, version, commit)? else {
            return Ok(Vec::new());
        };

        let mut variants: Vec<Option<String>> = Vec::new();
        for entry in &build.manifest.artifacts {
            if variants.contains(&entry.variant_id) {
                continue;
            }
            // Collect all files belonging to this variant and verify them
            // together: one damaged file disqualifies the whole variant.
            let variant_files: Vec<ArtifactEntry> = build
                .manifest
                .artifacts
                .iter()
                .filter(|a| a.variant_id == entry.variant_id)
                .cloned()
                .collect();
            if verify_entries(&build.commit_dir, &variant_files, VerifyMode::Size)?.is_empty() {
                variants.push(entry.variant_id.clone());
            }
        }
        Ok(variants)
    }

    /// Check if a specific variant exists (and is healthy) for a commit.
    pub fn has_variant(
        &self,
        project: &str,
        version: &str,
        commit: &str,
        variant_id: Option<&str>,
    ) -> Result<bool> {
        Ok(self
            .get_existing_variants(project, version, commit)?
            .iter()
            .any(|v| v.as_deref() == variant_id))
    }

    // =========================================================================
    // Query & listings
    // =========================================================================

    /// Create a query builder for finding builds.
    pub fn query(&self) -> BuildQuery<'_> {
        BuildQuery::new(self)
    }

    /// List all projects (sorted).
    pub fn list_projects(&self) -> Result<Vec<String>> {
        let mut projects = read_dir_names(self.paths.base_dir())?;
        projects.retain(|name| !name.starts_with('.') && self.paths.base_dir().join(name).is_dir());
        projects.sort();
        Ok(projects)
    }

    /// List all versions for a project, sorted with numeric-aware ordering
    /// (`9.0` < `10.0`).
    ///
    /// The `releases/` cache subtree is excluded. Only *canonical* version
    /// directories are listed — a version whose directory does not sit under
    /// its own `major.minor` (manual tampering / foreign data) is skipped
    /// with a warning, because no store operation could ever address it.
    /// This also guarantees each version appears at most once.
    pub fn list_versions(&self, project: &str) -> Result<Vec<String>> {
        validate::validate_project(project)?;
        let project_dir = self.paths.project_dir(project);
        let mut versions = Vec::new();

        for mm_name in read_dir_names(&project_dir)? {
            // Skip hidden entries and the releases cache subtree.
            if mm_name.starts_with('.') || mm_name == RELEASES_DIR {
                continue;
            }
            let mm_dir = project_dir.join(&mm_name);
            if !mm_dir.is_dir() {
                continue;
            }
            for v_name in read_dir_names(&mm_dir)? {
                if v_name.starts_with('.') || !mm_dir.join(&v_name).is_dir() {
                    continue;
                }
                if major_minor(&v_name) != mm_name {
                    tracing::warn!(
                        "skipping non-canonical version directory {} (expected under {})",
                        mm_dir.join(&v_name).display(),
                        major_minor(&v_name)
                    );
                    continue;
                }
                versions.push(v_name);
            }
        }

        versions.sort_by(|a, b| compare_versions(a, b));
        Ok(versions)
    }

    /// List all sources for a version.
    ///
    /// Source directory names are lossy (sanitized), so the *exact* sources
    /// are recovered from build manifests where possible; directories whose
    /// manifests are gone fall back to best-effort name parsing.
    pub fn list_sources(&self, project: &str, version: &str) -> Result<Vec<BuildSource>> {
        validate::validate_project(project)?;
        validate::validate_version(version)?;

        // Exact sources indexed by their directory name, from manifests.
        let mut by_dir_name = std::collections::BTreeMap::new();
        for commit_name in self.list_commits(project, version)? {
            let commit_dir = self.paths.commit_dir(project, version, &commit_name);
            if let Some(build) = self.load_stored_build(&commit_dir, None)? {
                for entry in build.manifest.sources {
                    by_dir_name
                        .entry(entry.source.to_dir_name())
                        .or_insert(entry.source);
                }
            }
        }

        // The by-source directory decides *which* sources are listed.
        let source_dir = self.paths.by_source_dir(project, version);
        let mut sources = Vec::new();
        for name in read_dir_names(&source_dir)? {
            if name.starts_with('.') {
                continue;
            }
            if let Some(exact) = by_dir_name.remove(&name) {
                sources.push(exact);
            } else if let Some(parsed) = BuildSource::from_dir_name(&name) {
                sources.push(parsed);
            }
        }
        Ok(sources)
    }

    /// List all commit shorts for a version (sorted).
    pub fn list_commits(&self, project: &str, version: &str) -> Result<Vec<String>> {
        validate::validate_project(project)?;
        validate::validate_version(version)?;

        let commit_dir = self.paths.by_commit_dir(project, version);
        let mut commits = read_dir_names(&commit_dir)?;
        commits.retain(|name| !name.starts_with('.') && commit_dir.join(name).is_dir());
        commits.sort();
        Ok(commits)
    }

    // =========================================================================
    // Delete
    // =========================================================================

    /// Delete a build by commit (short or full hash).
    ///
    /// The stored build's full hash must be prefix-compatible with the
    /// requested commit — a full hash that merely shares the 7-char short
    /// with a different stored commit deletes nothing (returns `false`).
    /// Dangling source links pointing at the deleted commit are removed and
    /// empty directories cleaned up. Returns `true` if a build was deleted.
    pub fn delete_by_commit(&self, project: &str, version: &str, commit: &str) -> Result<bool> {
        validate::validate_project(project)?;
        validate::validate_version(version)?;
        let commit = validate::validate_commit(commit)?;
        let commit_short = short_commit(&commit);

        let _lock = StoreLock::acquire(self.base_dir())?;

        let commit_dir = self.paths.commit_dir(project, version, &commit_short);
        let deleted = if commit_dir.exists() {
            // Identity check mirrors find_by_commit: never delete a build
            // whose full hash does not match the request. A directory with
            // an unreadable manifest is garbage and may always be deleted.
            if let Some(build) = self.load_stored_build(&commit_dir, None)?
                && !build.manifest.commit.starts_with(&commit)
                && !commit.starts_with(&build.manifest.commit)
            {
                tracing::warn!(
                    "refusing to delete {}: stored build is for commit {}, requested {commit}",
                    commit_dir.display(),
                    build.manifest.commit
                );
                return Ok(false);
            }
            std::fs::remove_dir_all(&commit_dir)
                .io_ctx(|| format!("deleting build {}", commit_dir.display()))?;
            tracing::info!("Deleted build: {}", commit_dir.display());
            true
        } else {
            false
        };

        self.cleanup_dangling_links(project, version)?;
        self.cleanup_empty_dirs_unlocked(project)?;

        Ok(deleted)
    }

    /// Delete a source entirely (all commit links for this source).
    ///
    /// The source is also removed from affected commit manifests. When
    /// `delete_orphan_commits` is set, commits left with no referencing
    /// source are deleted too.
    pub fn delete_source(
        &self,
        project: &str,
        version: &str,
        source: &BuildSource,
        delete_orphan_commits: bool,
    ) -> Result<DeleteSourceResult> {
        validate::validate_project(project)?;
        validate::validate_version(version)?;

        let _lock = StoreLock::acquire(self.base_dir())?;

        let source_dir = self.paths.source_dir(project, version, source);
        let mut orphan_commits_deleted = Vec::new();

        if !source_dir.exists() {
            return Ok(DeleteSourceResult {
                commits_affected: Vec::new(),
                orphan_commits_deleted,
            });
        }

        // Entry names in the source directory are the linked commit shorts.
        let commits_affected: Vec<String> = read_dir_names(&source_dir)?
            .into_iter()
            .filter(|n| !n.starts_with('.'))
            .collect();

        // Ordered for retryability: manifests are updated FIRST (removing
        // the source is idempotent), the source dir removed second, orphan
        // commits deleted last. A failure at any step leaves the source dir
        // present, so re-running this call resumes instead of returning
        // early with stale manifest entries left behind.
        let mut orphan_commits: Vec<String> = Vec::new();
        for commit_short in &commits_affected {
            let commit_dir = self.paths.commit_dir(project, version, commit_short);
            let Some(mut manifest) = self.load_manifest_for_write(&commit_dir)? else {
                continue;
            };
            manifest.remove_source(source);
            manifest.save(&commit_dir)?;

            if manifest.sources.is_empty() {
                orphan_commits.push(commit_short.clone());
            }
        }

        std::fs::remove_dir_all(&source_dir)
            .io_ctx(|| format!("deleting source directory {}", source_dir.display()))?;
        tracing::info!("Deleted source: {}", source_dir.display());

        if delete_orphan_commits {
            for commit_short in orphan_commits {
                let commit_dir = self.paths.commit_dir(project, version, &commit_short);
                std::fs::remove_dir_all(&commit_dir)
                    .io_ctx(|| format!("deleting orphan commit {}", commit_dir.display()))?;
                orphan_commits_deleted.push(commit_short.clone());
                tracing::info!("Deleted orphan commit: {commit_short}");
            }
        }

        self.cleanup_empty_dirs_unlocked(project)?;

        Ok(DeleteSourceResult {
            commits_affected,
            orphan_commits_deleted,
        })
    }

    /// Delete a specific commit link from a source.
    ///
    /// The stored build's full hash must be prefix-compatible with the
    /// requested commit (see [`Self::delete_by_commit`]); on mismatch
    /// nothing is touched and `false` is returned.
    ///
    /// Returns `true` when the commit became orphaned (no sources reference
    /// it anymore); it is additionally deleted when `delete_if_orphan` is set.
    pub fn delete_source_commit(
        &self,
        project: &str,
        version: &str,
        source: &BuildSource,
        commit: &str,
        delete_if_orphan: bool,
    ) -> Result<bool> {
        validate::validate_project(project)?;
        validate::validate_version(version)?;
        let commit = validate::validate_commit(commit)?;
        let commit_short = short_commit(&commit);

        let _lock = StoreLock::acquire(self.base_dir())?;

        let commit_dir = self.paths.commit_dir(project, version, &commit_short);

        // Identity check before mutating anything: a full hash that only
        // shares the short prefix with the stored build must not remove
        // that build's links or sources.
        if let Some(build) = self.load_stored_build(&commit_dir, None)?
            && !build.manifest.commit.starts_with(&commit)
            && !commit.starts_with(&build.manifest.commit)
        {
            tracing::warn!(
                "refusing to unlink {}: stored build is for commit {}, requested {commit}",
                commit_dir.display(),
                build.manifest.commit
            );
            return Ok(false);
        }

        let link_path = self
            .paths
            .source_link(project, version, source, &commit_short);

        if !link_path.exists() && !crate::link::is_link(&link_path) {
            return Ok(false);
        }

        crate::link::remove_dir_link(&link_path)?;
        tracing::debug!("Deleted source commit link: {}", link_path.display());

        // Update the manifest to drop this source.
        let mut is_orphan = false;

        if let Some(mut manifest) = self.load_manifest_for_write(&commit_dir)? {
            manifest.remove_source(source);
            manifest.save(&commit_dir)?;

            is_orphan = manifest.sources.is_empty();
            if delete_if_orphan && is_orphan {
                std::fs::remove_dir_all(&commit_dir)
                    .io_ctx(|| format!("deleting orphan commit {}", commit_dir.display()))?;
                tracing::info!("Deleted orphan commit: {commit_short}");
            }
        }

        self.cleanup_empty_dirs_unlocked(project)?;

        Ok(is_orphan)
    }

    // =========================================================================
    // Cleanup
    // =========================================================================

    /// Remove dangling source links (links whose target no longer exists)
    /// for one version, then drop empty source directories.
    fn cleanup_dangling_links(&self, project: &str, version: &str) -> Result<()> {
        let by_source_dir = self.paths.by_source_dir(project, version);

        for source_name in read_dir_names(&by_source_dir)? {
            let source_path = by_source_dir.join(&source_name);
            if !source_path.is_dir() {
                continue;
            }

            for commit_name in read_dir_names(&source_path)? {
                let link_path = source_path.join(&commit_name);
                if !crate::link::is_link(&link_path) {
                    continue;
                }
                // fs::metadata follows links: an error means the target is gone.
                if std::fs::metadata(&link_path).is_err() {
                    crate::link::remove_dir_link(&link_path)?;
                    tracing::debug!("Removed dangling link: {}", link_path.display());
                }
            }

            // Drop the source directory once it holds nothing.
            if read_dir_names(&source_path)?.is_empty() {
                std::fs::remove_dir(&source_path)
                    .io_ctx(|| format!("removing empty source dir {}", source_path.display()))?;
            }
        }

        Ok(())
    }

    /// Clean up empty directories under a project (removing the project
    /// directory itself if everything is gone).
    pub fn cleanup_empty_dirs(&self, project: &str) -> Result<()> {
        validate::validate_project(project)?;
        let _lock = StoreLock::acquire(self.base_dir())?;
        self.cleanup_empty_dirs_unlocked(project)
    }

    /// Lock-free inner cleanup, for callers already holding the store lock
    /// (the lock is not reentrant — acquiring it twice would deadlock).
    pub(crate) fn cleanup_empty_dirs_unlocked(&self, project: &str) -> Result<()> {
        let project_dir = self.paths.project_dir(project);
        if project_dir.is_dir() {
            remove_empty_dirs_recursive(&project_dir)?;
        }
        Ok(())
    }
}

/// Recursively remove empty directories, bottom-up. Returns whether `dir`
/// itself was removed.
///
/// Links are treated as content (never followed, never deleted): a source
/// directory holding live links is not empty. Dangling links are removed by
/// `cleanup_dangling_links` before this runs on delete paths.
fn remove_empty_dirs_recursive(dir: &Path) -> Result<bool> {
    let mut is_empty = true;

    let entries =
        std::fs::read_dir(dir).io_ctx(|| format!("reading directory {}", dir.display()))?;
    for entry in entries {
        let entry = entry.io_ctx(|| format!("reading entry in {}", dir.display()))?;
        // file_type() from the DirEntry never follows symlinks, so links
        // count as plain content here.
        let file_type = entry
            .file_type()
            .io_ctx(|| format!("inspecting {}", entry.path().display()))?;

        if file_type.is_dir() {
            if !remove_empty_dirs_recursive(&entry.path())? {
                is_empty = false;
            }
        } else {
            is_empty = false;
        }
    }

    if is_empty {
        std::fs::remove_dir(dir)
            .io_ctx(|| format!("removing empty directory {}", dir.display()))?;
        tracing::debug!("Removed empty directory: {}", dir.display());
    }

    Ok(is_empty)
}

/// Read the entry names of a directory; a missing directory yields an empty
/// list (callers treat absent store subtrees as empty, not as errors).
pub(crate) fn read_dir_names(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::io(format!("reading directory {}", dir.display()), e));
        }
    };

    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.io_ctx(|| format!("reading entry in {}", dir.display()))?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        } else {
            tracing::warn!(
                "skipping non-UTF-8 entry in {}: {:?}",
                dir.display(),
                entry.file_name()
            );
        }
    }
    Ok(names)
}

// =============================================================================
// Value types
// =============================================================================

/// Metadata for storing a build.
#[derive(Debug, Clone)]
pub struct BuildMetadata {
    /// Project name.
    pub project: String,
    /// Version being built.
    pub version: String,
    /// Build source (PR, tag, branch, commit).
    pub source: BuildSource,
    /// Full commit hash (lowercase hex; short hashes of ≥7 chars accepted).
    pub commit: String,
    /// Short commit hash (first 7 characters). Derived from `commit`;
    /// [`ArtifactStore::store`] re-derives it and ignores manual edits.
    pub commit_short: String,
    /// Branch name (may be empty when not applicable).
    pub branch: String,
}

impl BuildMetadata {
    /// Create new build metadata. The commit is normalized to lowercase and
    /// `commit_short` derived from it.
    pub fn new(
        project: String,
        version: String,
        source: BuildSource,
        commit: String,
        branch: String,
    ) -> Self {
        let commit = commit.to_ascii_lowercase();
        let commit_short = short_commit(&commit);
        Self {
            project,
            version,
            source,
            commit,
            commit_short,
            branch,
        }
    }
}

/// A source artifact to be stored.
#[derive(Debug, Clone)]
pub struct SourceArtifact {
    /// Variant ID (None if single-variant project).
    pub variant_id: Option<String>,
    /// Path to the source file.
    pub path: PathBuf,
    /// Target filename in storage (bare filename, validated).
    pub target_name: String,
}

/// Result of storing artifacts.
#[derive(Debug)]
pub struct StoreResult {
    /// Directory where artifacts are stored.
    pub commit_dir: PathBuf,
    /// Updated manifest.
    pub manifest: BuildManifest,
    /// Files that were copied in by this call.
    pub stored_files: Vec<PathBuf>,
    /// Variant IDs of artifacts that were skipped (already stored, healthy).
    pub skipped_variants: Vec<Option<String>>,
    /// Whether any artifacts were deduplicated.
    pub was_deduplicated: bool,
}

/// A stored build with its metadata.
#[derive(Debug)]
pub struct StoredBuild {
    /// Build manifest.
    pub manifest: BuildManifest,
    /// Directory where the build is stored.
    pub commit_dir: PathBuf,
    /// Paths to artifact files (aligned with `manifest.artifacts`).
    pub files: Vec<PathBuf>,
}

impl StoredBuild {
    /// Verify stored files against the manifest at the given level.
    pub fn verify(&self, mode: VerifyMode) -> Result<Vec<VerifyIssue>> {
        verify_entries(&self.commit_dir, &self.manifest.artifacts, mode)
    }

    /// Paths of the files belonging to a variant (empty if absent).
    pub fn files_for_variant(&self, variant_id: Option<&str>) -> Vec<PathBuf> {
        self.manifest
            .artifacts
            .iter()
            .filter(|a| a.variant_id.as_deref() == variant_id)
            .map(|a| self.commit_dir.join(&a.filename))
            .collect()
    }

    /// Path to the artifact with the given filename, if recorded.
    pub fn file_by_name(&self, filename: &str) -> Option<PathBuf> {
        self.manifest
            .artifact_by_filename(filename)
            .map(|a| self.commit_dir.join(&a.filename))
    }
}

/// Result of deleting a source.
#[derive(Debug)]
pub struct DeleteSourceResult {
    /// Commits that were affected (had the source removed from their manifest).
    pub commits_affected: Vec<String>,
    /// Commits that were deleted because they became orphaned.
    pub orphan_commits_deleted: Vec<String>,
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

    /// Helper: create a dummy source artifact file.
    fn make_artifact(dir: &Path, name: &str, variant: Option<&str>) -> SourceArtifact {
        let path = dir.join(name);
        std::fs::write(&path, format!("dummy content for {name}")).unwrap();
        SourceArtifact {
            variant_id: variant.map(|s| s.to_string()),
            path,
            target_name: name.to_string(),
        }
    }

    /// Helper: create metadata.
    fn make_metadata(
        project: &str,
        version: &str,
        source: BuildSource,
        commit: &str,
        branch: &str,
    ) -> BuildMetadata {
        BuildMetadata::new(
            project.to_string(),
            version.to_string(),
            source,
            commit.to_string(),
            branch.to_string(),
        )
    }

    // =========================================================================
    // Store basics
    // =========================================================================

    #[test]
    fn test_store_basic() {
        let (dir, store) = temp_store();
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890abcdef",
            "develop",
        );

        let result = store.store(&[artifact], &meta).unwrap();

        assert!(result.commit_dir.exists());
        assert_eq!(result.stored_files.len(), 1);
        assert!(result.skipped_variants.is_empty());
        assert!(!result.was_deduplicated);
        assert_eq!(result.manifest.project, "wp-rocket");
        assert_eq!(result.manifest.version, "3.17.4");
        assert_eq!(result.manifest.artifacts.len(), 1);
        assert_eq!(result.manifest.artifacts[0].sha256.len(), 64);

        // Source link created.
        let source_dir = store.paths().source_dir(
            "wp-rocket",
            "3.17.4",
            &BuildSource::Branch("develop".into()),
        );
        assert!(source_dir.exists());
        // Store marker written.
        assert!(store.base_dir().join(STORE_MARKER).exists());
    }

    #[test]
    fn test_store_deduplication() {
        let (dir, store) = temp_store();
        let a1 = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );

        store.store(&[a1], &meta).unwrap();

        let a2 = make_artifact(dir.path(), "plugin.zip", None);
        let result = store.store(&[a2], &meta).unwrap();

        assert!(result.was_deduplicated);
        assert_eq!(result.skipped_variants.len(), 1);
        assert!(result.stored_files.is_empty());
    }

    #[test]
    fn test_store_different_variants() {
        let (dir, store) = temp_store();
        let meta = make_metadata(
            "backwpup",
            "5.1.0",
            BuildSource::PullRequest(42),
            "abc1234567890",
            "develop",
        );

        let a1 = make_artifact(dir.path(), "free.zip", Some("free"));
        store.store(&[a1], &meta).unwrap();

        let a2 = make_artifact(dir.path(), "pro.zip", Some("pro"));
        let result = store.store(&[a2], &meta).unwrap();

        assert!(!result.was_deduplicated);
        assert_eq!(result.stored_files.len(), 1);
        assert!(result.skipped_variants.is_empty());
        assert_eq!(result.manifest.artifacts.len(), 2);
    }

    #[test]
    fn test_store_self_heals_deleted_file() {
        let (dir, store) = temp_store();
        let a1 = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );
        let result = store.store(&[a1], &meta).unwrap();

        // Delete the stored file behind the manifest's back.
        std::fs::remove_file(&result.stored_files[0]).unwrap();

        // Re-store must copy the file again instead of "deduplicating" it.
        let a2 = make_artifact(dir.path(), "plugin.zip", None);
        let result = store.store(&[a2], &meta).unwrap();
        assert!(!result.was_deduplicated);
        assert_eq!(result.stored_files.len(), 1);
        assert!(result.stored_files[0].exists());
    }

    #[test]
    fn test_store_artifact_not_found() {
        let (_dir, store) = temp_store();
        let bad_artifact = SourceArtifact {
            variant_id: None,
            path: PathBuf::from("/nonexistent/path/plugin.zip"),
            target_name: "plugin.zip".into(),
        };
        let meta = make_metadata(
            "wp-rocket",
            "1.0.0",
            BuildSource::Branch("main".into()),
            "abc1234567890",
            "main",
        );

        let err = store.store(&[bad_artifact], &meta).unwrap_err();
        assert!(matches!(err, Error::ArtifactNotFound(_)));
    }

    #[test]
    fn test_store_rejects_invalid_input() {
        let (dir, store) = temp_store();
        let artifact = make_artifact(dir.path(), "plugin.zip", None);

        // Empty artifact list.
        let meta = make_metadata(
            "wp-rocket",
            "1.0.0",
            BuildSource::Branch("main".into()),
            "abc1234567890",
            "main",
        );
        assert!(store.store(&[], &meta).is_err());

        // Path traversal in project.
        let meta_bad_project = make_metadata(
            "../evil",
            "1.0.0",
            BuildSource::Branch("main".into()),
            "abc1234567890",
            "main",
        );
        assert!(
            store
                .store(std::slice::from_ref(&artifact), &meta_bad_project)
                .is_err()
        );

        // Invalid commit.
        let meta_bad_commit = make_metadata(
            "wp-rocket",
            "1.0.0",
            BuildSource::Branch("main".into()),
            "nothex",
            "main",
        );
        assert!(
            store
                .store(std::slice::from_ref(&artifact), &meta_bad_commit)
                .is_err()
        );

        // Artifact shadowing the manifest.
        let evil = SourceArtifact {
            variant_id: None,
            path: artifact.path.clone(),
            target_name: BuildManifest::FILENAME.to_string(),
        };
        assert!(store.store(&[evil], &meta).is_err());

        // Nothing was created by the rejected calls.
        assert!(!store.paths().project_dir("wp-rocket").exists());
    }

    #[test]
    fn test_store_detects_commit_collision() {
        let (dir, store) = temp_store();
        let a1 = make_artifact(dir.path(), "p1.zip", None);
        // Two different full hashes sharing the first 7 chars.
        let m1 = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234000000000000",
            "develop",
        );
        store.store(&[a1], &m1).unwrap();

        let a2 = make_artifact(dir.path(), "p2.zip", None);
        let m2 = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234ffffffffffff",
            "develop",
        );
        let err = store.store(&[a2], &m2).unwrap_err();
        assert!(matches!(err, Error::CommitCollision { .. }));
    }

    #[test]
    fn test_store_short_then_full_commit_upgrades_manifest() {
        let (dir, store) = temp_store();
        let a1 = make_artifact(dir.path(), "p1.zip", None);
        let m1 = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234",
            "develop",
        );
        store.store(&[a1], &m1).unwrap();

        // Same commit, now with full precision: no collision, hash upgraded.
        let a2 = make_artifact(dir.path(), "p1.zip", None);
        let m2 = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890abcdef",
            "develop",
        );
        let result = store.store(&[a2], &m2).unwrap();
        assert_eq!(result.manifest.commit, "abc1234567890abcdef");
    }

    #[test]
    fn test_store_never_writes_over_newer_schema() {
        let (dir, store) = temp_store();
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );
        // Simulate a manifest written by a future crate version.
        let commit_dir = store.paths().commit_dir("wp-rocket", "3.17.4", "abc1234");
        std::fs::create_dir_all(&commit_dir).unwrap();
        std::fs::write(
            BuildManifest::path_in(&commit_dir),
            r#"{"schema_version": 999}"#,
        )
        .unwrap();

        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let err = store.store(&[artifact], &meta).unwrap_err();
        assert!(matches!(err, Error::UnsupportedSchema { found: 999, .. }));
    }

    #[test]
    fn test_store_rebuilds_corrupted_manifest() {
        let (dir, store) = temp_store();
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );
        let commit_dir = store.paths().commit_dir("wp-rocket", "3.17.4", "abc1234");
        std::fs::create_dir_all(&commit_dir).unwrap();
        std::fs::write(BuildManifest::path_in(&commit_dir), "{garbage").unwrap();

        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let result = store.store(&[artifact], &meta).unwrap();
        assert_eq!(result.manifest.artifacts.len(), 1);
        // The rebuilt manifest is valid again.
        assert!(BuildManifest::load(&commit_dir).is_ok());
    }

    // =========================================================================
    // Find by source
    // =========================================================================

    #[test]
    fn test_find_by_source() {
        let (dir, store) = temp_store();
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let source = BuildSource::PullRequest(99);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "abc1234567890",
            "develop",
        );

        store.store(&[artifact], &meta).unwrap();

        let builds = store
            .find_by_source("wp-rocket", "3.17.4", &source)
            .unwrap();
        assert_eq!(builds.len(), 1);
        assert_eq!(builds[0].manifest.commit_short, "abc1234");
    }

    #[test]
    fn test_find_by_source_not_found() {
        let (_dir, store) = temp_store();
        let source = BuildSource::PullRequest(999);
        let builds = store.find_by_source("wp-rocket", "1.0.0", &source).unwrap();
        assert!(builds.is_empty());
    }

    #[test]
    fn test_find_by_source_sanitized_branch_roundtrip() {
        let (dir, store) = temp_store();
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        // Branch with a separator: dir name is sanitized + suffixed.
        let source = BuildSource::Branch("feature/cool-thing".into());
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "abc1234567890",
            "feature/cool-thing",
        );

        store.store(&[artifact], &meta).unwrap();

        let builds = store
            .find_by_source("wp-rocket", "3.17.4", &source)
            .unwrap();
        assert_eq!(builds.len(), 1);
        // And list_sources recovers the EXACT original source from the manifest.
        let sources = store.list_sources("wp-rocket", "3.17.4").unwrap();
        assert_eq!(sources, vec![source]);
    }

    #[test]
    fn test_find_latest_by_source_follows_relink() {
        let (dir, store) = temp_store();
        let source = BuildSource::Branch("develop".into());

        // Build commit A, then commit B.
        let a1 = make_artifact(dir.path(), "p1.zip", None);
        let m1 = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "aaa1111111111",
            "develop",
        );
        store.store(&[a1], &m1).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(5));
        let a2 = make_artifact(dir.path(), "p2.zip", None);
        let m2 = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "bbb2222222222",
            "develop",
        );
        store.store(&[a2], &m2).unwrap();

        let latest = store
            .find_latest_by_source("wp-rocket", "3.17.4", &source)
            .unwrap()
            .unwrap();
        assert_eq!(latest.manifest.commit_short, "bbb2222");

        // Branch reverts to commit A and is rebuilt: dedupe skips the copy,
        // but linked_at refreshes — A must now be the latest for this source.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let a3 = make_artifact(dir.path(), "p1.zip", None);
        store.store(&[a3], &m1).unwrap();

        let latest = store
            .find_latest_by_source("wp-rocket", "3.17.4", &source)
            .unwrap()
            .unwrap();
        assert_eq!(latest.manifest.commit_short, "aaa1111");
    }

    // =========================================================================
    // Find by commit
    // =========================================================================

    #[test]
    fn test_find_by_commit_short_and_full() {
        let (dir, store) = temp_store();
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );

        store.store(&[artifact], &meta).unwrap();

        // Short, full, and uppercase all resolve.
        for query in ["abc1234", "abc1234567890", "ABC1234"] {
            let build = store.find_by_commit("wp-rocket", "3.17.4", query).unwrap();
            assert!(build.is_some(), "query '{query}' should hit");
        }

        // A full hash agreeing only on the short prefix must NOT hit.
        let build = store
            .find_by_commit("wp-rocket", "3.17.4", "abc1234999999")
            .unwrap();
        assert!(build.is_none());
    }

    #[test]
    fn test_find_by_commit_not_found() {
        let (_dir, store) = temp_store();
        let build = store
            .find_by_commit("wp-rocket", "1.0.0", "9999999")
            .unwrap();
        assert!(build.is_none());
    }

    #[test]
    fn test_find_by_commit_damaged_is_miss() {
        let (dir, store) = temp_store();
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );
        let result = store.store(&[artifact], &meta).unwrap();

        std::fs::remove_file(&result.stored_files[0]).unwrap();

        let build = store
            .find_by_commit("wp-rocket", "3.17.4", "abc1234")
            .unwrap();
        assert!(build.is_none(), "damaged build must read as a miss");
    }

    #[test]
    fn test_find_commit_in_versions() {
        let (dir, store) = temp_store();
        for version in ["3.17.4", "3.18.0"] {
            let a = make_artifact(dir.path(), &format!("p-{version}.zip"), None);
            let m = make_metadata(
                "wp-rocket",
                version,
                BuildSource::Branch("develop".into()),
                "abc1234567890",
                "develop",
            );
            store.store(&[a], &m).unwrap();
        }

        let builds = store
            .find_commit_in_versions("wp-rocket", "abc1234")
            .unwrap();
        assert_eq!(builds.len(), 2);
    }

    // =========================================================================
    // Listings
    // =========================================================================

    #[test]
    fn test_list_projects() {
        let (dir, store) = temp_store();
        for (project, commit) in [
            ("wp-rocket", "aaa1111111111"),
            ("backwpup", "bbb2222222222"),
        ] {
            let a = make_artifact(dir.path(), &format!("{project}.zip"), None);
            let m = make_metadata(
                project,
                "1.0.0",
                BuildSource::Branch("main".into()),
                commit,
                "main",
            );
            store.store(&[a], &m).unwrap();
        }

        // Sorted, and internal entries (lock/marker) are not listed.
        assert_eq!(
            store.list_projects().unwrap(),
            vec!["backwpup".to_string(), "wp-rocket".to_string()]
        );
    }

    #[test]
    fn test_list_projects_empty() {
        let (_dir, store) = temp_store();
        assert!(store.list_projects().unwrap().is_empty());
    }

    #[test]
    fn test_list_versions_numeric_sort() {
        let (dir, store) = temp_store();
        for (ver, commit) in [
            ("10.0.0", "aaa1111111111"),
            ("9.0.0", "bbb2222222222"),
            ("9.1.0", "ccc3333333333"),
        ] {
            let a = make_artifact(dir.path(), &format!("p-{ver}.zip"), None);
            let m = make_metadata(
                "wp-rocket",
                ver,
                BuildSource::Branch("main".into()),
                commit,
                "main",
            );
            store.store(&[a], &m).unwrap();
        }

        // Numeric-aware: 9.x before 10.x (lexicographic would put "10" first).
        assert_eq!(
            store.list_versions("wp-rocket").unwrap(),
            vec![
                "9.0.0".to_string(),
                "9.1.0".to_string(),
                "10.0.0".to_string()
            ]
        );
    }

    #[test]
    fn test_list_sources_exact_and_fallback() {
        let (dir, store) = temp_store();
        let a1 = make_artifact(dir.path(), "p1.zip", None);
        let m1 = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::PullRequest(42),
            "aaa1111111111",
            "develop",
        );
        store.store(&[a1], &m1).unwrap();

        let a2 = make_artifact(dir.path(), "p2.zip", None);
        let m2 = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "bbb2222222222",
            "develop",
        );
        store.store(&[a2], &m2).unwrap();

        let sources = store.list_sources("wp-rocket", "3.17.4").unwrap();
        assert_eq!(sources.len(), 2);
        assert!(sources.contains(&BuildSource::PullRequest(42)));
        assert!(sources.contains(&BuildSource::Branch("develop".into())));
    }

    #[test]
    fn test_list_commits() {
        let (dir, store) = temp_store();
        for (commit, name) in [("aaa1111111111", "p1.zip"), ("bbb2222222222", "p2.zip")] {
            let a = make_artifact(dir.path(), name, None);
            let m = make_metadata(
                "wp-rocket",
                "3.17.4",
                BuildSource::Branch("develop".into()),
                commit,
                "develop",
            );
            store.store(&[a], &m).unwrap();
        }

        assert_eq!(
            store.list_commits("wp-rocket", "3.17.4").unwrap(),
            vec!["aaa1111".to_string(), "bbb2222".to_string()]
        );
    }

    // =========================================================================
    // Variants
    // =========================================================================

    #[test]
    fn test_get_existing_variants() {
        let (dir, store) = temp_store();
        let a1 = make_artifact(dir.path(), "free.zip", Some("free"));
        let a2 = make_artifact(dir.path(), "pro.zip", Some("pro"));
        let meta = make_metadata(
            "backwpup",
            "5.1.0",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );

        store.store(&[a1, a2], &meta).unwrap();

        let variants = store
            .get_existing_variants("backwpup", "5.1.0", "abc1234")
            .unwrap();
        assert_eq!(variants.len(), 2);
        assert!(variants.contains(&Some("free".to_string())));
        assert!(variants.contains(&Some("pro".to_string())));
    }

    #[test]
    fn test_variant_with_missing_file_does_not_count() {
        let (dir, store) = temp_store();
        let a1 = make_artifact(dir.path(), "free.zip", Some("free"));
        let meta = make_metadata(
            "backwpup",
            "5.1.0",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );
        let result = store.store(&[a1], &meta).unwrap();

        assert!(
            store
                .has_variant("backwpup", "5.1.0", "abc1234", Some("free"))
                .unwrap()
        );

        // Delete the file: the variant must stop counting as existing.
        std::fs::remove_file(&result.stored_files[0]).unwrap();
        assert!(
            !store
                .has_variant("backwpup", "5.1.0", "abc1234", Some("free"))
                .unwrap()
        );
    }

    #[test]
    fn test_has_variant() {
        let (dir, store) = temp_store();
        let artifact = make_artifact(dir.path(), "free.zip", Some("free"));
        let meta = make_metadata(
            "backwpup",
            "5.1.0",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );

        store.store(&[artifact], &meta).unwrap();

        assert!(
            store
                .has_variant("backwpup", "5.1.0", "abc1234", Some("free"))
                .unwrap()
        );
        assert!(
            !store
                .has_variant("backwpup", "5.1.0", "abc1234", Some("pro"))
                .unwrap()
        );
        assert!(
            !store
                .has_variant("backwpup", "5.1.0", "abc1234", None)
                .unwrap()
        );
    }

    // =========================================================================
    // Delete
    // =========================================================================

    #[test]
    fn test_delete_by_commit() {
        let (dir, store) = temp_store();
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );

        store.store(&[artifact], &meta).unwrap();
        let commit_dir = store.paths().commit_dir("wp-rocket", "3.17.4", "abc1234");
        assert!(commit_dir.exists());

        // Full hash accepted; returns true on actual deletion.
        assert!(
            store
                .delete_by_commit("wp-rocket", "3.17.4", "abc1234567890")
                .unwrap()
        );
        assert!(!commit_dir.exists());
        // Second delete: nothing left, returns false.
        assert!(
            !store
                .delete_by_commit("wp-rocket", "3.17.4", "abc1234")
                .unwrap()
        );
    }

    #[test]
    fn test_delete_by_commit_cleans_dangling_links() {
        let (dir, store) = temp_store();
        let source = BuildSource::Branch("develop".into());
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "abc1234567890",
            "develop",
        );

        store.store(&[artifact], &meta).unwrap();
        store
            .delete_by_commit("wp-rocket", "3.17.4", "abc1234")
            .unwrap();

        // The dangling source link is removed (and empty dirs swept).
        let source_dir = store.paths().source_dir("wp-rocket", "3.17.4", &source);
        assert!(!source_dir.exists());
        assert!(!store.paths().project_dir("wp-rocket").exists());
    }

    #[test]
    fn test_delete_source_with_orphan_cleanup() {
        let (dir, store) = temp_store();
        let source = BuildSource::PullRequest(42);
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "abc1234567890",
            "develop",
        );

        store.store(&[artifact], &meta).unwrap();

        let result = store
            .delete_source("wp-rocket", "3.17.4", &source, true)
            .unwrap();

        assert_eq!(result.commits_affected, vec!["abc1234".to_string()]);
        assert_eq!(result.orphan_commits_deleted, vec!["abc1234".to_string()]);
        let commit_dir = store.paths().commit_dir("wp-rocket", "3.17.4", "abc1234");
        assert!(!commit_dir.exists());
    }

    #[test]
    fn test_delete_source_without_orphan_cleanup() {
        let (dir, store) = temp_store();
        let source = BuildSource::PullRequest(42);
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "abc1234567890",
            "develop",
        );

        store.store(&[artifact], &meta).unwrap();

        let result = store
            .delete_source("wp-rocket", "3.17.4", &source, false)
            .unwrap();

        assert!(result.orphan_commits_deleted.is_empty());
        let commit_dir = store.paths().commit_dir("wp-rocket", "3.17.4", "abc1234");
        assert!(commit_dir.exists());
        // The manifest no longer references the deleted source.
        let manifest = BuildManifest::load(&commit_dir).unwrap();
        assert!(manifest.sources.is_empty());
    }

    #[test]
    fn test_delete_source_missing_is_noop() {
        let (_dir, store) = temp_store();
        let result = store
            .delete_source("wp-rocket", "1.0.0", &BuildSource::PullRequest(1), true)
            .unwrap();
        assert!(result.commits_affected.is_empty());
        assert!(result.orphan_commits_deleted.is_empty());
    }

    #[test]
    fn test_delete_source_commit_orphan_detection() {
        let (dir, store) = temp_store();
        let source = BuildSource::Branch("develop".into());
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "abc1234567890",
            "develop",
        );

        store.store(&[artifact], &meta).unwrap();

        let is_orphan = store
            .delete_source_commit("wp-rocket", "3.17.4", &source, "abc1234", false)
            .unwrap();
        assert!(is_orphan); // last source removed → orphan
    }

    #[test]
    fn test_cleanup_empty_dirs_after_full_delete() {
        let (dir, store) = temp_store();
        let source = BuildSource::PullRequest(42);
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "abc1234567890",
            "develop",
        );

        store.store(&[artifact], &meta).unwrap();
        store
            .delete_source("wp-rocket", "3.17.4", &source, true)
            .unwrap();

        assert!(!store.paths().project_dir("wp-rocket").exists());
        // The store itself (marker, lock) survives.
        assert!(store.base_dir().exists());
    }

    // =========================================================================
    // Identity checks & rollback
    // =========================================================================

    #[test]
    fn test_delete_by_commit_refuses_wrong_full_hash() {
        let (dir, store) = temp_store();
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234000000000000",
            "develop",
        );
        store.store(&[artifact], &meta).unwrap();

        // Same 7-char short, different full hash: nothing may be deleted.
        let deleted = store
            .delete_by_commit("wp-rocket", "3.17.4", "abc1234fffffffffff")
            .unwrap();
        assert!(!deleted);
        assert!(
            store
                .find_by_commit("wp-rocket", "3.17.4", "abc1234")
                .unwrap()
                .is_some(),
            "the stored build must survive a mismatched delete request"
        );
    }

    #[test]
    fn test_delete_source_commit_refuses_wrong_full_hash() {
        let (dir, store) = temp_store();
        let source = BuildSource::Branch("develop".into());
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "abc1234000000000000",
            "develop",
        );
        store.store(&[artifact], &meta).unwrap();

        let touched = store
            .delete_source_commit("wp-rocket", "3.17.4", &source, "abc1234fffffffffff", true)
            .unwrap();
        assert!(!touched);
        // Link and manifest source entry are both intact.
        let builds = store
            .find_by_source("wp-rocket", "3.17.4", &source)
            .unwrap();
        assert_eq!(builds.len(), 1);
        assert_eq!(builds[0].manifest.sources.len(), 1);
    }

    #[test]
    fn test_store_partial_failure_rolls_back_fresh_dir() {
        let (dir, store) = temp_store();
        let good = make_artifact(dir.path(), "a.zip", Some("a"));
        let missing = SourceArtifact {
            variant_id: Some("b".to_string()),
            path: dir.path().join("nonexistent.zip"),
            target_name: "b.zip".to_string(),
        };
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );

        // Second artifact missing → the whole store call fails...
        assert!(store.store(&[good, missing], &meta).is_err());

        // ...and no phantom commit directory is left behind.
        let commit_dir = store.paths().commit_dir("wp-rocket", "3.17.4", "abc1234");
        assert!(!commit_dir.exists());
        assert!(
            store
                .list_commits("wp-rocket", "3.17.4")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_store_partial_failure_preserves_existing_build() {
        let (dir, store) = temp_store();
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );

        // A valid build exists first.
        let a1 = make_artifact(dir.path(), "a.zip", Some("a"));
        store.store(&[a1], &meta).unwrap();

        // A later call fails midway: the pre-existing build must survive.
        let missing = SourceArtifact {
            variant_id: Some("b".to_string()),
            path: dir.path().join("nonexistent.zip"),
            target_name: "b.zip".to_string(),
        };
        assert!(store.store(&[missing], &meta).is_err());

        let build = store
            .find_by_commit("wp-rocket", "3.17.4", "abc1234")
            .unwrap();
        assert!(build.is_some(), "existing build must not be rolled back");
    }

    #[test]
    fn test_list_versions_skips_non_canonical_dirs() {
        let (dir, store) = temp_store();
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );
        store.store(&[artifact], &meta).unwrap();

        // Tampered layout: version 9.9.9 planted under major.minor 3.17.
        std::fs::create_dir_all(
            store
                .paths()
                .project_dir("wp-rocket")
                .join("3.17")
                .join("9.9.9"),
        )
        .unwrap();

        // Only the canonical version is listed (9.9.9 could never be
        // addressed by commit_dir(), which derives 9.9 from the version).
        assert_eq!(
            store.list_versions("wp-rocket").unwrap(),
            vec!["3.17.4".to_string()]
        );
    }

    #[test]
    fn test_version_equal_to_its_major_minor_coexists() {
        let (dir, store) = temp_store();
        // "5.6" and "5.6.0" are distinct versions sharing major.minor 5.6:
        // they must land in distinct directories and both list.
        for (version, commit) in [("5.6", "aaa1111111111"), ("5.6.0", "bbb2222222222")] {
            let a = make_artifact(dir.path(), &format!("p-{version}.zip"), None);
            let m = make_metadata(
                "wp-rocket",
                version,
                BuildSource::Branch("main".into()),
                commit,
                "main",
            );
            store.store(&[a], &m).unwrap();
        }

        assert_eq!(
            store.list_versions("wp-rocket").unwrap(),
            vec!["5.6".to_string(), "5.6.0".to_string()]
        );
        assert!(
            store
                .find_by_commit("wp-rocket", "5.6", "aaa1111")
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .find_by_commit("wp-rocket", "5.6.0", "bbb2222")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_find_by_source_ignores_hidden_strays() {
        let (dir, store) = temp_store();
        let source = BuildSource::Branch("develop".into());
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "abc1234567890",
            "develop",
        );
        store.store(&[artifact], &meta).unwrap();

        // macOS Finder droppings inside the source directory.
        let source_dir = store.paths().source_dir("wp-rocket", "3.17.4", &source);
        std::fs::write(source_dir.join(".DS_Store"), b"junk").unwrap();

        let builds = store
            .find_by_source("wp-rocket", "3.17.4", &source)
            .unwrap();
        assert_eq!(builds.len(), 1);
    }

    // =========================================================================
    // Concurrency
    // =========================================================================

    #[test]
    fn test_concurrent_stores_to_same_commit() {
        let (dir, store) = temp_store();
        let base = store.base_dir().to_path_buf();

        // Several threads store different variants of the same commit at
        // once; locking must serialize the manifest read-modify-write so no
        // variant entry is lost.
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let dir = dir.path().to_path_buf();
                let base = base.clone();
                std::thread::spawn(move || {
                    let store = ArtifactStore::new(base);
                    let name = format!("variant-{i}.zip");
                    let path = dir.join(&name);
                    std::fs::write(&path, format!("content {i}")).unwrap();
                    let artifact = SourceArtifact {
                        variant_id: Some(format!("v{i}")),
                        path,
                        target_name: name,
                    };
                    let meta = BuildMetadata::new(
                        "wp-rocket".to_string(),
                        "3.17.4".to_string(),
                        BuildSource::Branch("develop".into()),
                        "abc1234567890".to_string(),
                        "develop".to_string(),
                    );
                    store.store(&[artifact], &meta).unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let variants = store
            .get_existing_variants("wp-rocket", "3.17.4", "abc1234")
            .unwrap();
        assert_eq!(variants.len(), 4, "all concurrent variants must survive");
    }
}
