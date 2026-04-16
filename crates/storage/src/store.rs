//! Artifact storage manager.
//!
//! Manages storing, retrieving, and querying build artifacts with deduplication.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::link::create_dir_link;
use crate::manifest::{ArtifactEntry, BuildManifest};
use crate::path::{BuildSource, PathBuilder};
use crate::query::BuildQuery;

/// Manages artifact storage with deduplication.
///
/// # Directory Structure
///
/// ```text
/// {base_dir}/{project}/{major.minor}/{version}/
/// ├── by-commit/{commit_short}/  - actual files + manifest
/// └── by-source/{source}/        - directory containing commit links
///     ├── {commit1} → ../../by-commit/{commit1}
///     └── {commit2} → ../../by-commit/{commit2}
/// ```
pub struct ArtifactStore {
    paths: PathBuilder,
}

impl ArtifactStore {
    /// Create a new artifact store with the given base directory.
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

    /// Store build artifacts with deduplication.
    ///
    /// If the commit already has builds, only missing variants are added.
    /// A source link is always created/updated.
    ///
    /// # Arguments
    ///
    /// * `source_artifacts` - Artifacts to store
    /// * `metadata` - Build metadata
    ///
    /// # Returns
    ///
    /// Result containing store information including which files were stored
    /// and which were skipped (already existed).
    pub fn store(
        &self,
        source_artifacts: &[SourceArtifact],
        metadata: &BuildMetadata,
    ) -> Result<StoreResult> {
        let commit_dir =
            self.paths
                .commit_dir(&metadata.project, &metadata.version, &metadata.commit_short);

        // Check for existing manifest
        let existing_manifest = if commit_dir.exists() {
            BuildManifest::load(&commit_dir).ok()
        } else {
            None
        };

        // Get existing variants
        let existing_variants: Vec<Option<String>> = existing_manifest
            .as_ref()
            .map(|m| m.existing_variants())
            .unwrap_or_default();

        // Create commit directory
        std::fs::create_dir_all(&commit_dir)?;

        // Load or create manifest
        let mut manifest = existing_manifest.unwrap_or_else(|| {
            BuildManifest::new(
                metadata.project.clone(),
                metadata.version.clone(),
                metadata.commit.clone(),
            )
        });

        // Add source to manifest
        manifest.add_source(metadata.source.clone(), Some(metadata.branch.clone()));

        let mut stored_files = Vec::new();
        let mut skipped_variants = Vec::new();

        // Store each artifact
        for artifact in source_artifacts {
            // Check if variant already exists
            if existing_variants.contains(&artifact.variant_id) {
                skipped_variants.push(artifact.variant_id.clone());
                continue;
            }

            // Verify source file exists
            if !artifact.path.exists() {
                return Err(Error::ArtifactNotFound(artifact.path.clone()));
            }

            let target_path = commit_dir.join(&artifact.target_name);

            // Copy file
            std::fs::copy(&artifact.path, &target_path)?;

            // Calculate checksum and size
            let sha256 = self.calculate_sha256(&target_path)?;
            let size = std::fs::metadata(&target_path)?.len();

            // Add to manifest
            manifest.add_artifact(ArtifactEntry {
                variant_id: artifact.variant_id.clone(),
                filename: artifact.target_name.clone(),
                size_bytes: size,
                sha256,
            });

            stored_files.push(target_path);

            tracing::debug!("Stored artifact: {} ({} bytes)", artifact.target_name, size);
        }

        // Save manifest
        manifest.save(&commit_dir)?;

        // Create source link
        self.create_source_link(metadata)?;

        let was_deduplicated = !skipped_variants.is_empty();

        tracing::info!(
            "Stored build: {} v{} @ {} ({} files, {} skipped)",
            metadata.project,
            metadata.version,
            metadata.commit_short,
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

    /// Create a link from source to commit directory.
    fn create_source_link(&self, metadata: &BuildMetadata) -> Result<()> {
        let link_path = self.paths.source_link(
            &metadata.project,
            &metadata.version,
            &metadata.source,
            &metadata.commit_short,
        );

        // Use relative path on Unix, absolute on Windows (junctions need absolute)
        #[cfg(unix)]
        let target = self.paths.relative_commit_path(&metadata.commit_short);

        #[cfg(windows)]
        let target =
            self.paths
                .commit_dir(&metadata.project, &metadata.version, &metadata.commit_short);

        create_dir_link(&target, &link_path)?;

        tracing::debug!(
            "Created source link: {} -> {}",
            link_path.display(),
            target.display()
        );

        Ok(())
    }

    /// Calculate SHA256 checksum of a file.
    fn calculate_sha256(&self, path: &Path) -> Result<String> {
        let bytes = std::fs::read(path)?;
        let hash = Sha256::digest(&bytes);
        // sha2 0.11 returns an Array newtype; format each byte as two hex digits.
        // Source: https://doc.rust-lang.org/std/fmt/trait.LowerHex.html (u8 impl)
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        Ok(hex)
    }

    /// Find builds by source (PR, tag, branch, commit).
    /// Returns all builds for a source, sorted by date (newest first).
    pub fn find_by_source(
        &self,
        project: &str,
        version: &str,
        source: &BuildSource,
    ) -> Result<Vec<StoredBuild>> {
        let source_dir = self.paths.source_dir(project, version, source);

        if !source_dir.exists() {
            return Ok(Vec::new());
        }

        let mut builds = Vec::new();

        // Iterate through commit links in the source directory
        for entry in std::fs::read_dir(&source_dir)? {
            let entry = entry?;
            let link_path = entry.path();

            // Resolve the link to get the commit directory
            let commit_dir = if crate::link::is_link(&link_path) {
                std::fs::read_link(&link_path)
                    .map(|rel| link_path.parent().unwrap().join(rel))
                    .ok()
            } else if link_path.is_dir() {
                Some(link_path)
            } else {
                None
            };

            if let Some(dir) = commit_dir
                && dir.exists()
                && let Ok(Some(build)) = self.load_stored_build(&dir)
            {
                builds.push(build);
            }
        }

        // Sort by build date (newest first)
        builds.sort_by_key(|b| std::cmp::Reverse(b.manifest.built_at));

        Ok(builds)
    }

    /// Find the latest build for a source.
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

    /// Find a build by commit.
    pub fn find_by_commit(
        &self,
        project: &str,
        version: &str,
        commit_short: &str,
    ) -> Result<Option<StoredBuild>> {
        let commit_dir = self.paths.commit_dir(project, version, commit_short);

        if !commit_dir.exists() {
            return Ok(None);
        }

        self.load_stored_build(&commit_dir)
    }

    /// Load a stored build from a commit directory.
    fn load_stored_build(&self, commit_dir: &Path) -> Result<Option<StoredBuild>> {
        if !commit_dir.exists() {
            return Ok(None);
        }

        let manifest = match BuildManifest::load(commit_dir) {
            Ok(m) => m,
            Err(_) => return Ok(None),
        };

        let files = manifest
            .artifacts
            .iter()
            .map(|a| commit_dir.join(&a.filename))
            .collect();

        Ok(Some(StoredBuild {
            manifest,
            commit_dir: commit_dir.to_path_buf(),
            files,
        }))
    }

    /// Get existing variants for a commit.
    pub fn get_existing_variants(
        &self,
        project: &str,
        version: &str,
        commit_short: &str,
    ) -> Result<Vec<Option<String>>> {
        let commit_dir = self.paths.commit_dir(project, version, commit_short);

        if !commit_dir.exists() {
            return Ok(Vec::new());
        }

        let manifest = BuildManifest::load(&commit_dir)?;
        Ok(manifest.existing_variants())
    }

    /// Check if a specific variant exists for a commit.
    pub fn has_variant(
        &self,
        project: &str,
        version: &str,
        commit_short: &str,
        variant_id: Option<&str>,
    ) -> Result<bool> {
        let commit_dir = self.paths.commit_dir(project, version, commit_short);

        if !commit_dir.exists() {
            return Ok(false);
        }

        let manifest = BuildManifest::load(&commit_dir)?;
        Ok(manifest.has_variant(variant_id))
    }

    /// Create a query builder for finding builds.
    pub fn query(&self) -> BuildQuery<'_> {
        BuildQuery::new(self)
    }

    /// List all projects.
    pub fn list_projects(&self) -> Result<Vec<String>> {
        let base = self.paths.base_dir();
        let mut projects = Vec::new();

        if !base.exists() {
            return Ok(projects);
        }

        for entry in std::fs::read_dir(base)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                // Skip hidden directories
                if !name.starts_with('.') {
                    projects.push(name.to_string());
                }
            }
        }

        projects.sort();
        Ok(projects)
    }

    /// List all versions for a project.
    pub fn list_versions(&self, project: &str) -> Result<Vec<String>> {
        let project_dir = self.paths.project_dir(project);
        let mut versions = Vec::new();

        if !project_dir.exists() {
            return Ok(versions);
        }

        // Walk major.minor directories
        for mm_entry in std::fs::read_dir(&project_dir)? {
            let mm_entry = mm_entry?;
            if mm_entry.file_type()?.is_dir() {
                // Walk version directories
                for v_entry in std::fs::read_dir(mm_entry.path())? {
                    let v_entry = v_entry?;
                    if v_entry.file_type()?.is_dir()
                        && let Some(name) = v_entry.file_name().to_str()
                    {
                        versions.push(name.to_string());
                    }
                }
            }
        }

        versions.sort();
        Ok(versions)
    }

    /// List all sources for a version.
    pub fn list_sources(&self, project: &str, version: &str) -> Result<Vec<BuildSource>> {
        let source_dir = self.paths.by_source_dir(project, version);
        let mut sources = Vec::new();

        if !source_dir.exists() {
            return Ok(sources);
        }

        for entry in std::fs::read_dir(&source_dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && let Some(source) = BuildSource::from_dir_name(name)
            {
                sources.push(source);
            }
        }

        Ok(sources)
    }

    /// List all commits for a version.
    pub fn list_commits(&self, project: &str, version: &str) -> Result<Vec<String>> {
        let commit_dir = self.paths.by_commit_dir(project, version);
        let mut commits = Vec::new();

        if !commit_dir.exists() {
            return Ok(commits);
        }

        for entry in std::fs::read_dir(&commit_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                commits.push(name.to_string());
            }
        }

        commits.sort();
        Ok(commits)
    }

    /// Delete a build by commit.
    pub fn delete_by_commit(&self, project: &str, version: &str, commit_short: &str) -> Result<()> {
        let commit_dir = self.paths.commit_dir(project, version, commit_short);

        if commit_dir.exists() {
            std::fs::remove_dir_all(&commit_dir)?;
            tracing::info!("Deleted build: {}", commit_dir.display());
        }

        // Remove this commit from all source directories and clean up
        self.cleanup_dangling_links(project, version)?;
        self.cleanup_empty_dirs(project)?;

        Ok(())
    }

    /// Delete a source entirely (all commits for this source).
    /// Also removes the source from commit manifests and cleans up orphan commits.
    pub fn delete_source(
        &self,
        project: &str,
        version: &str,
        source: &BuildSource,
        delete_orphan_commits: bool,
    ) -> Result<DeleteSourceResult> {
        let source_dir = self.paths.source_dir(project, version, source);
        let mut orphan_commits_deleted = Vec::new();

        if !source_dir.exists() {
            return Ok(DeleteSourceResult {
                commits_affected: Vec::new(),
                orphan_commits_deleted,
            });
        }

        // Collect all commits linked from this source
        let mut commits_affected = Vec::new();
        for entry in std::fs::read_dir(&source_dir)? {
            let entry = entry?;
            if let Some(commit_short) = entry.file_name().to_str() {
                commits_affected.push(commit_short.to_string());
            }
        }

        // Remove the source directory entirely
        std::fs::remove_dir_all(&source_dir)?;
        tracing::info!("Deleted source: {}", source_dir.display());

        // Update manifests to remove this source
        for commit_short in &commits_affected {
            let commit_dir = self.paths.commit_dir(project, version, commit_short);
            if commit_dir.exists()
                && let Ok(mut manifest) = BuildManifest::load(&commit_dir)
            {
                // Remove the source from the manifest
                manifest.sources.retain(|s| &s.source != source);
                manifest.save(&commit_dir)?;

                // Check if commit is now orphaned (no sources reference it)
                if delete_orphan_commits && manifest.sources.is_empty() {
                    std::fs::remove_dir_all(&commit_dir)?;
                    orphan_commits_deleted.push(commit_short.clone());
                    tracing::info!("Deleted orphan commit: {}", commit_short);
                }
            }
        }

        self.cleanup_empty_dirs(project)?;

        Ok(DeleteSourceResult {
            commits_affected,
            orphan_commits_deleted,
        })
    }

    /// Delete a specific commit link from a source.
    /// Optionally deletes the commit if it becomes orphaned.
    pub fn delete_source_commit(
        &self,
        project: &str,
        version: &str,
        source: &BuildSource,
        commit_short: &str,
        delete_if_orphan: bool,
    ) -> Result<bool> {
        let link_path = self
            .paths
            .source_link(project, version, source, commit_short);

        if !link_path.exists() && !crate::link::is_link(&link_path) {
            return Ok(false);
        }

        // Remove the link
        crate::link::remove_dir_link(&link_path)?;
        tracing::debug!("Deleted source commit link: {}", link_path.display());

        // Update manifest to remove this source
        let commit_dir = self.paths.commit_dir(project, version, commit_short);
        let mut is_orphan = false;

        if commit_dir.exists()
            && let Ok(mut manifest) = BuildManifest::load(&commit_dir)
        {
            manifest.sources.retain(|s| &s.source != source);
            manifest.save(&commit_dir)?;

            is_orphan = manifest.sources.is_empty();

            if delete_if_orphan && is_orphan {
                std::fs::remove_dir_all(&commit_dir)?;
                tracing::info!("Deleted orphan commit: {}", commit_short);
            }
        }

        // Clean up empty source directory
        let source_dir = self.paths.source_dir(project, version, source);
        if source_dir.exists() && std::fs::read_dir(&source_dir)?.next().is_none() {
            std::fs::remove_dir(&source_dir)?;
        }

        self.cleanup_empty_dirs(project)?;

        Ok(is_orphan)
    }

    /// Remove dangling source links (links pointing to non-existent commits).
    fn cleanup_dangling_links(&self, project: &str, version: &str) -> Result<()> {
        let by_source_dir = self.paths.by_source_dir(project, version);

        if !by_source_dir.exists() {
            return Ok(());
        }

        // Walk through each source directory
        for source_entry in std::fs::read_dir(&by_source_dir)? {
            let source_entry = source_entry?;
            let source_path = source_entry.path();

            if !source_path.is_dir() {
                continue;
            }

            // Walk through commit links in this source
            for commit_entry in std::fs::read_dir(&source_path)? {
                let commit_entry = commit_entry?;
                let link_path = commit_entry.path();

                if crate::link::is_link(&link_path) {
                    let target = std::fs::read_link(&link_path);
                    let resolved = target.map(|t| link_path.parent().unwrap().join(t));

                    if let Ok(path) = resolved
                        && !path.exists()
                    {
                        crate::link::remove_dir_link(&link_path)?;
                        tracing::debug!("Removed dangling link: {}", link_path.display());
                    }
                }
            }

            // Remove source directory if empty
            if std::fs::read_dir(&source_path)?.next().is_none() {
                std::fs::remove_dir(&source_path)?;
            }
        }

        Ok(())
    }

    /// Clean up empty directories.
    pub fn cleanup_empty_dirs(&self, project: &str) -> Result<()> {
        let project_dir = self.paths.project_dir(project);

        if !project_dir.exists() {
            return Ok(());
        }

        // Walk and remove empty directories from bottom up
        self.remove_empty_dirs_recursive(&project_dir)?;

        Ok(())
    }

    fn remove_empty_dirs_recursive(&self, dir: &Path) -> Result<bool> {
        if !dir.is_dir() {
            return Ok(false);
        }

        let mut is_empty = true;

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                if !self.remove_empty_dirs_recursive(&path)? {
                    is_empty = false;
                }
            } else {
                is_empty = false;
            }
        }

        if is_empty {
            std::fs::remove_dir(dir)?;
            tracing::debug!("Removed empty directory: {}", dir.display());
        }

        Ok(is_empty)
    }
}

/// Metadata for storing a build.
#[derive(Debug, Clone)]
pub struct BuildMetadata {
    /// Project name.
    pub project: String,
    /// Version being built.
    pub version: String,
    /// Build source (PR, tag, branch, commit).
    pub source: BuildSource,
    /// Full commit hash.
    pub commit: String,
    /// Short commit hash (7 characters).
    pub commit_short: String,
    /// Branch name.
    pub branch: String,
}

impl BuildMetadata {
    /// Create new build metadata.
    pub fn new(
        project: String,
        version: String,
        source: BuildSource,
        commit: String,
        branch: String,
    ) -> Self {
        let commit_short = commit.chars().take(7).collect();
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
    /// Target filename in storage.
    pub target_name: String,
}

/// Result of storing artifacts.
#[derive(Debug)]
pub struct StoreResult {
    /// Directory where artifacts are stored.
    pub commit_dir: PathBuf,
    /// Updated manifest.
    pub manifest: BuildManifest,
    /// Files that were stored.
    pub stored_files: Vec<PathBuf>,
    /// Variants that were skipped (already existed).
    pub skipped_variants: Vec<Option<String>>,
    /// Whether any variants were deduplicated.
    pub was_deduplicated: bool,
}

/// A stored build with its metadata.
#[derive(Debug)]
pub struct StoredBuild {
    /// Build manifest.
    pub manifest: BuildManifest,
    /// Directory where build is stored.
    pub commit_dir: PathBuf,
    /// Paths to artifact files.
    pub files: Vec<PathBuf>,
}

/// Result of deleting a source.
#[derive(Debug)]
pub struct DeleteSourceResult {
    /// Commits that were affected (had source removed from manifest).
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
        let store = ArtifactStore::new(dir.path().to_path_buf());
        (dir, store)
    }

    /// Helper: create a dummy source artifact file. Returns SourceArtifact.
    fn make_artifact(dir: &std::path::Path, name: &str, variant: Option<&str>) -> SourceArtifact {
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
    // 2.1 – Basic Store
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

        // Source link created
        let source_dir = store.paths().source_dir(
            "wp-rocket",
            "3.17.4",
            &BuildSource::Branch("develop".into()),
        );
        assert!(source_dir.exists());
    }

    // =========================================================================
    // 2.2 – Deduplication
    // =========================================================================

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

        // First store
        store.store(&[a1], &meta).unwrap();

        // Second store with same commit, same variant
        let a2 = make_artifact(dir.path(), "plugin.zip", None);
        let result = store.store(&[a2], &meta).unwrap();

        assert!(result.was_deduplicated);
        assert_eq!(result.skipped_variants.len(), 1);
        assert!(result.stored_files.is_empty());
    }

    // =========================================================================
    // 2.3 – Different Variants
    // =========================================================================

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

        // Store "free"
        let a1 = make_artifact(dir.path(), "free.zip", Some("free"));
        store.store(&[a1], &meta).unwrap();

        // Store "pro" to same commit
        let a2 = make_artifact(dir.path(), "pro.zip", Some("pro"));
        let result = store.store(&[a2], &meta).unwrap();

        // "pro" is a new variant — not deduplicated
        assert!(!result.was_deduplicated);
        assert_eq!(result.stored_files.len(), 1);
        assert!(result.skipped_variants.is_empty());
        assert_eq!(result.manifest.artifacts.len(), 2);
    }

    // =========================================================================
    // 2.4 – Artifact Not Found
    // =========================================================================

    #[test]
    fn test_store_artifact_not_found() {
        let (_dir, store) = temp_store();
        let bad_artifact = SourceArtifact {
            variant_id: None,
            path: std::path::PathBuf::from("/nonexistent/path/plugin.zip"),
            target_name: "plugin.zip".into(),
        };
        let meta = make_metadata(
            "wp-rocket",
            "1.0.0",
            BuildSource::Branch("main".into()),
            "abc1234567890",
            "main",
        );

        let result = store.store(&[bad_artifact], &meta);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Artifact not found"));
    }

    // =========================================================================
    // 2.5 – Creates Dirs
    // =========================================================================

    #[test]
    fn test_store_creates_dirs() {
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

        // Verify nested directory structure
        let commit_dir = store.paths().commit_dir("wp-rocket", "3.17.4", "abc1234");
        assert!(commit_dir.exists());

        let by_source_dir = store.paths().by_source_dir("wp-rocket", "3.17.4");
        assert!(by_source_dir.exists());
    }

    // =========================================================================
    // 2.6 – SHA256 Checksum
    // =========================================================================

    #[test]
    fn test_store_sha256_checksum() {
        let (dir, store) = temp_store();
        let artifact = make_artifact(dir.path(), "plugin.zip", None);
        let meta = make_metadata(
            "wp-rocket",
            "1.0.0",
            BuildSource::Branch("main".into()),
            "abc1234567890",
            "main",
        );

        let result = store.store(&[artifact], &meta).unwrap();

        // Manifest has SHA256
        let sha = &result.manifest.artifacts[0].sha256;
        assert!(!sha.is_empty());
        assert_eq!(sha.len(), 64); // SHA256 = 64 hex chars

        // Verify it matches actual file hash
        let stored_file = result.commit_dir.join("plugin.zip");
        let bytes = std::fs::read(&stored_file).unwrap();
        let hash = sha2::Sha256::digest(&bytes);
        let expected: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(sha, &expected);
    }

    // =========================================================================
    // 2.7–2.8 – Find by Source
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

    // =========================================================================
    // 2.9 – Find Latest by Source
    // =========================================================================

    #[test]
    fn test_find_latest_by_source() {
        let (dir, store) = temp_store();
        let source = BuildSource::Branch("develop".into());

        // Store two commits for same source
        let a1 = make_artifact(dir.path(), "p1.zip", None);
        let m1 = make_metadata(
            "wp-rocket",
            "3.17.4",
            source.clone(),
            "aaa1111111111",
            "develop",
        );
        store.store(&[a1], &m1).unwrap();

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
            .unwrap();
        assert!(latest.is_some());
        // Latest should be the second one (newest built_at)
        assert_eq!(latest.unwrap().manifest.commit_short, "bbb2222");
    }

    // =========================================================================
    // 2.10–2.11 – Find by Commit
    // =========================================================================

    #[test]
    fn test_find_by_commit() {
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

        let build = store
            .find_by_commit("wp-rocket", "3.17.4", "abc1234")
            .unwrap();
        assert!(build.is_some());
        assert_eq!(build.unwrap().manifest.version, "3.17.4");
    }

    #[test]
    fn test_find_by_commit_not_found() {
        let (_dir, store) = temp_store();
        let build = store
            .find_by_commit("wp-rocket", "1.0.0", "zzz9999")
            .unwrap();
        assert!(build.is_none());
    }

    // =========================================================================
    // 2.12–2.13 – List Projects
    // =========================================================================

    #[test]
    fn test_list_projects() {
        let (dir, store) = temp_store();

        // Store in two different projects
        let a1 = make_artifact(dir.path(), "p1.zip", None);
        let m1 = make_metadata(
            "wp-rocket",
            "1.0.0",
            BuildSource::Branch("main".into()),
            "aaa1111111111",
            "main",
        );
        store.store(&[a1], &m1).unwrap();

        let a2 = make_artifact(dir.path(), "p2.zip", None);
        let m2 = make_metadata(
            "backwpup",
            "5.0.0",
            BuildSource::Branch("main".into()),
            "bbb2222222222",
            "main",
        );
        store.store(&[a2], &m2).unwrap();

        let projects = store.list_projects().unwrap();
        assert_eq!(projects.len(), 2);
        assert!(projects.contains(&"wp-rocket".to_string()));
        assert!(projects.contains(&"backwpup".to_string()));
    }

    #[test]
    fn test_list_projects_empty() {
        let (_dir, store) = temp_store();
        let projects = store.list_projects().unwrap();
        assert!(projects.is_empty());
    }

    // =========================================================================
    // 2.14 – List Versions
    // =========================================================================

    #[test]
    fn test_list_versions() {
        let (dir, store) = temp_store();

        for (ver, commit) in [("3.17.4", "aaa1111111111"), ("3.18.0", "bbb2222222222")] {
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

        let versions = store.list_versions("wp-rocket").unwrap();
        assert_eq!(versions.len(), 2);
        assert!(versions.contains(&"3.17.4".to_string()));
        assert!(versions.contains(&"3.18.0".to_string()));
    }

    // =========================================================================
    // 2.15 – List Sources
    // =========================================================================

    #[test]
    fn test_list_sources() {
        let (dir, store) = temp_store();

        // Store with PR source
        let a1 = make_artifact(dir.path(), "p1.zip", None);
        let m1 = make_metadata(
            "wp-rocket",
            "3.17.4",
            BuildSource::PullRequest(42),
            "aaa1111111111",
            "develop",
        );
        store.store(&[a1], &m1).unwrap();

        // Store with branch source
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
    }

    // =========================================================================
    // 2.16 – List Commits
    // =========================================================================

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

        let commits = store.list_commits("wp-rocket", "3.17.4").unwrap();
        assert_eq!(commits.len(), 2);
    }

    // =========================================================================
    // 2.17–2.18 – Delete by Commit
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

        store
            .delete_by_commit("wp-rocket", "3.17.4", "abc1234")
            .unwrap();
        assert!(!commit_dir.exists());
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

        // Source link should be cleaned up since commit is gone
        let source_dir = store.paths().source_dir("wp-rocket", "3.17.4", &source);
        // Source dir may be entirely removed by cleanup, or its link is dangling-removed
        if source_dir.exists() {
            let entries: Vec<_> = std::fs::read_dir(&source_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert!(entries.is_empty());
        }
    }

    // =========================================================================
    // 2.19–2.21 – Delete Source
    // =========================================================================

    #[test]
    fn test_delete_source() {
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
        assert!(!result.commits_affected.is_empty());

        let source_dir = store.paths().source_dir("wp-rocket", "3.17.4", &source);
        assert!(!source_dir.exists());
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

        // Commit had only this source, so it's now orphaned and should be deleted
        assert!(!result.orphan_commits_deleted.is_empty());
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

        // Commit preserved (orphan cleanup off)
        assert!(result.orphan_commits_deleted.is_empty());
        let commit_dir = store.paths().commit_dir("wp-rocket", "3.17.4", "abc1234");
        assert!(commit_dir.exists());
    }

    // =========================================================================
    // 2.22 – Delete Source Commit
    // =========================================================================

    #[test]
    fn test_delete_source_commit() {
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

        assert!(is_orphan); // Last source removed → orphan
    }

    // =========================================================================
    // 2.23 – Cleanup Empty Dirs
    // =========================================================================

    #[test]
    fn test_cleanup_empty_dirs() {
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

        // Delete everything
        store
            .delete_source("wp-rocket", "3.17.4", &source, true)
            .unwrap();

        // Project dir should be cleaned up
        let project_dir = store.paths().project_dir("wp-rocket");
        assert!(!project_dir.exists());
    }

    // =========================================================================
    // 2.24–2.25 – Get Existing Variants / Has Variant
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
}
