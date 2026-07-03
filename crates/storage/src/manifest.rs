//! Build manifest for tracking builds.
//!
//! Each commit directory contains a `build-manifest.json` file with metadata
//! about the build: project info, commit details, the sources that point at
//! it, and per-artifact checksums. The manifest is the source of truth for a
//! commit directory — files not listed in it are ignored (and re-copied on
//! the next store if missing).
//!
//! Manifests are written atomically (temp file + fsync + rename), so readers
//! can never observe a torn manifest. See `fsx.rs` for the model.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Error, IoResultExt, Result};
use crate::fsx;
use crate::path::BuildSource;

/// Number of characters in a short commit hash.
pub(crate) const COMMIT_SHORT_LEN: usize = 7;

/// Compute the short form of a commit hash (lowercased, first 7 chars).
pub(crate) fn short_commit(commit: &str) -> String {
    commit
        .chars()
        .take(COMMIT_SHORT_LEN)
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Load a schema-versioned JSON manifest from `path`.
///
/// Shared by [`BuildManifest`] and [`crate::release::ReleaseManifest`].
/// Checks `schema_version` *before* the typed parse so files written by a
/// newer crate produce a clear [`Error::UnsupportedSchema`] instead of a
/// confusing field-mismatch error.
pub(crate) fn load_json_manifest<T: DeserializeOwned>(path: &Path, supported: u32) -> Result<T> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::ManifestNotFound(path.to_path_buf()));
        }
        Err(e) => return Err(Error::io(format!("reading manifest {}", path.display()), e)),
    };

    let value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| Error::corrupted(path, format!("invalid JSON: {e}")))?;

    let found = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| Error::corrupted(path, "missing or non-numeric 'schema_version'"))?;

    if found > u64::from(supported) {
        return Err(Error::UnsupportedSchema {
            path: path.to_path_buf(),
            found: found.try_into().unwrap_or(u32::MAX),
            supported,
        });
    }

    serde_json::from_value(value)
        .map_err(|e| Error::corrupted(path, format!("schema mismatch: {e}")))
}

/// Serialize `manifest` as pretty JSON and write it atomically to `path`.
pub(crate) fn save_json_manifest<T: Serialize>(path: &Path, manifest: &T) -> Result<()> {
    let content = serde_json::to_vec_pretty(manifest)?;
    fsx::atomic_write(path, &content)
}

/// Manifest for a single build.
///
/// Stored as `build-manifest.json` in each commit directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildManifest {
    /// Schema version for forward compatibility.
    pub schema_version: u32,

    /// Project name.
    pub project: String,

    /// Version that was built.
    pub version: String,

    /// Full commit hash.
    pub commit: String,

    /// Short commit hash (first 7 characters, lowercase).
    pub commit_short: String,

    /// When the build was first stored.
    pub built_at: DateTime<Utc>,

    /// Sources that point to this build.
    /// Multiple sources can reference the same commit.
    #[serde(default)]
    pub sources: Vec<SourceEntry>,

    /// List of artifact files.
    pub artifacts: Vec<ArtifactEntry>,
}

/// Entry for a source that points to this build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEntry {
    /// The source type and identifier.
    pub source: BuildSource,

    /// Branch name (if applicable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    /// When this source last pointed at this build (refreshed on re-store).
    pub linked_at: DateTime<Utc>,
}

/// Entry for a single artifact file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    /// Variant ID (None if single-variant project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant_id: Option<String>,

    /// Filename (bare name inside the commit/release directory).
    pub filename: String,

    /// File size in bytes.
    pub size_bytes: u64,

    /// SHA256 checksum (lowercase hex).
    pub sha256: String,
}

impl BuildManifest {
    /// Current schema version.
    pub const CURRENT_SCHEMA: u32 = 1;

    /// Manifest filename.
    pub const FILENAME: &'static str = "build-manifest.json";

    /// Create a new manifest.
    pub fn new(project: String, version: String, commit: String) -> Self {
        let commit_short = short_commit(&commit);

        Self {
            schema_version: Self::CURRENT_SCHEMA,
            project,
            version,
            commit,
            commit_short,
            built_at: Utc::now(),
            sources: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    /// Add an artifact, replacing any existing entry with the same filename.
    ///
    /// Filename is the *physical* identity: it maps to exactly one file in
    /// the commit directory, so the manifest must hold at most one entry per
    /// filename to stay in sync with what is on disk. Re-storing the same
    /// file therefore refreshes its size/checksum in place instead of
    /// appending a duplicate entry. (Each variant is a single artifact file,
    /// so `filename` and `variant_id` are one-to-one in practice; keying on
    /// filename additionally keeps `None`-variant single-artifact builds
    /// correct.)
    pub fn upsert_artifact(&mut self, entry: ArtifactEntry) {
        if let Some(existing) = self
            .artifacts
            .iter_mut()
            .find(|a| a.filename == entry.filename)
        {
            *existing = entry;
        } else {
            self.artifacts.push(entry);
        }
    }

    /// Add or refresh a source pointing at this build.
    ///
    /// If the source already exists, its `linked_at` timestamp and branch
    /// are refreshed — `linked_at` is what "latest build for source" queries
    /// sort by, so it must track the most recent link, not the first.
    pub fn add_source(&mut self, source: BuildSource, branch: Option<String>) {
        if let Some(existing) = self.sources.iter_mut().find(|s| s.source == source) {
            existing.linked_at = Utc::now();
            if branch.is_some() {
                existing.branch = branch;
            }
        } else {
            self.sources.push(SourceEntry {
                source,
                branch,
                linked_at: Utc::now(),
            });
        }
    }

    /// Remove all sources whose directory name matches `source`'s.
    ///
    /// Matching by *directory name* rather than value equality makes removal
    /// work even when the caller's `BuildSource` came from the lossy
    /// [`BuildSource::from_dir_name`] parser (e.g. `Branch("feature-test")`
    /// for an on-disk `Branch("feature/test")` entry).
    pub fn remove_source(&mut self, source: &BuildSource) {
        let dir_name = source.to_dir_name();
        self.sources
            .retain(|s| s.source != *source && s.source.to_dir_name() != dir_name);
    }

    /// When `source` last pointed at this build, if it does.
    pub fn source_linked_at(&self, source: &BuildSource) -> Option<DateTime<Utc>> {
        let dir_name = source.to_dir_name();
        self.sources
            .iter()
            .find(|s| s.source == *source || s.source.to_dir_name() == dir_name)
            .map(|s| s.linked_at)
    }

    /// Get variant IDs of existing artifacts (may contain duplicates when a
    /// variant has multiple files).
    pub fn existing_variants(&self) -> Vec<Option<String>> {
        self.artifacts
            .iter()
            .map(|a| a.variant_id.clone())
            .collect()
    }

    /// Check if a variant already exists.
    pub fn has_variant(&self, variant_id: Option<&str>) -> bool {
        self.artifacts
            .iter()
            .any(|a| a.variant_id.as_deref() == variant_id)
    }

    /// Find an artifact entry by filename.
    pub fn artifact_by_filename(&self, filename: &str) -> Option<&ArtifactEntry> {
        self.artifacts.iter().find(|a| a.filename == filename)
    }

    /// Load manifest from a directory.
    ///
    /// # Errors
    ///
    /// - [`Error::ManifestNotFound`] if no manifest file exists
    /// - [`Error::ManifestCorrupted`] if it cannot be parsed or violates invariants
    /// - [`Error::UnsupportedSchema`] if written by a newer crate version
    pub fn load(dir: &Path) -> Result<Self> {
        let path = Self::path_in(dir);
        let manifest: Self = load_json_manifest(&path, Self::CURRENT_SCHEMA)?;
        manifest.validate(&path)?;
        Ok(manifest)
    }

    /// Save manifest atomically to a directory (created if missing).
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)
            .io_ctx(|| format!("creating manifest directory {}", dir.display()))?;
        save_json_manifest(&Self::path_in(dir), self)
    }

    /// Get the manifest file path for a directory.
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(Self::FILENAME)
    }

    /// Check internal invariants after a successful parse.
    ///
    /// Guards against hand-edited or otherwise inconsistent manifests being
    /// silently trusted: a wrong `commit` would defeat collision detection,
    /// and a filename with path separators would make `StoredBuild::files`
    /// point outside the commit directory.
    fn validate(&self, path: &Path) -> Result<()> {
        if self.project.is_empty() {
            return Err(Error::corrupted(path, "empty 'project'"));
        }
        if self.version.is_empty() {
            return Err(Error::corrupted(path, "empty 'version'"));
        }
        if self.commit.is_empty() || !self.commit.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::corrupted(path, "'commit' is not a hex hash"));
        }
        if self.commit_short != short_commit(&self.commit) {
            return Err(Error::corrupted(
                path,
                format!(
                    "'commit_short' ({}) does not match 'commit' ({})",
                    self.commit_short, self.commit
                ),
            ));
        }
        validate_entry_filenames(&self.artifacts, path)
    }
}

/// Validate artifact/asset entries loaded from disk: every filename must be
/// a safe bare filename (same rules as at store time) and unique within the
/// manifest, otherwise upsert/verify invariants no longer hold.
///
/// Shared by [`BuildManifest`] and [`crate::release::ReleaseManifest`].
pub(crate) fn validate_entry_filenames(entries: &[ArtifactEntry], path: &Path) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        if let Err(e) = crate::validate::validate_artifact_filename(&entry.filename) {
            return Err(Error::corrupted(
                path,
                format!("unsafe artifact filename '{}': {e}", entry.filename),
            ));
        }
        if !seen.insert(entry.filename.as_str()) {
            return Err(Error::corrupted(
                path,
                format!("duplicate artifact filename '{}'", entry.filename),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn manifest() -> BuildManifest {
        BuildManifest::new(
            "backwpup".to_string(),
            "5.6.0".to_string(),
            "abc1234567890".to_string(),
        )
    }

    #[test]
    fn test_manifest_new() {
        let m = manifest();
        assert_eq!(m.project, "backwpup");
        assert_eq!(m.version, "5.6.0");
        assert_eq!(m.commit, "abc1234567890");
        assert_eq!(m.commit_short, "abc1234");
        assert_eq!(m.schema_version, BuildManifest::CURRENT_SCHEMA);
        assert!(m.sources.is_empty());
        assert!(m.artifacts.is_empty());
    }

    #[test]
    fn test_short_commit_truncates_and_lowercases() {
        assert_eq!(short_commit("ABCDEFGHIJKLMNOP"), "abcdefg");
        assert_eq!(short_commit("abc12"), "abc12");
    }

    #[test]
    fn test_upsert_artifact_adds_and_replaces() {
        let mut m = manifest();
        m.upsert_artifact(ArtifactEntry {
            variant_id: Some("free".to_string()),
            filename: "plugin-free.zip".to_string(),
            size_bytes: 1024,
            sha256: "aaa".to_string(),
        });
        assert_eq!(m.artifacts.len(), 1);

        // Same filename replaces (refreshed checksum), not duplicates.
        m.upsert_artifact(ArtifactEntry {
            variant_id: Some("free".to_string()),
            filename: "plugin-free.zip".to_string(),
            size_bytes: 2048,
            sha256: "bbb".to_string(),
        });
        assert_eq!(m.artifacts.len(), 1);
        assert_eq!(m.artifacts[0].size_bytes, 2048);

        // A different variant (its own file) is a separate entry.
        m.upsert_artifact(ArtifactEntry {
            variant_id: Some("pro-en".to_string()),
            filename: "plugin-pro-en.zip".to_string(),
            size_bytes: 4096,
            sha256: "ccc".to_string(),
        });
        assert_eq!(m.artifacts.len(), 2);
    }

    #[test]
    fn test_add_source_no_duplicates_refreshes_linked_at() {
        let mut m = manifest();
        let source = BuildSource::PullRequest(123);
        m.add_source(source.clone(), Some("feature/test".to_string()));
        let first_linked = m.sources[0].linked_at;

        std::thread::sleep(std::time::Duration::from_millis(5));
        m.add_source(source.clone(), Some("feature/test".to_string()));

        assert_eq!(m.sources.len(), 1);
        assert!(m.sources[0].linked_at > first_linked);
    }

    #[test]
    fn test_remove_source_matches_by_dir_name() {
        let mut m = manifest();
        m.add_source(BuildSource::Branch("feature/test".to_string()), None);

        // A lossy round-tripped value (as returned by from_dir_name) still
        // must NOT match a different source...
        m.remove_source(&BuildSource::Branch("other".to_string()));
        assert_eq!(m.sources.len(), 1);

        // ...but the exact source removes it.
        m.remove_source(&BuildSource::Branch("feature/test".to_string()));
        assert!(m.sources.is_empty());
    }

    #[test]
    fn test_source_linked_at() {
        let mut m = manifest();
        let source = BuildSource::Branch("develop".to_string());
        assert!(m.source_linked_at(&source).is_none());
        m.add_source(source.clone(), None);
        assert!(m.source_linked_at(&source).is_some());
    }

    #[test]
    fn test_existing_variants_and_has_variant() {
        let mut m = manifest();
        m.upsert_artifact(ArtifactEntry {
            variant_id: Some("free".to_string()),
            filename: "free.zip".to_string(),
            size_bytes: 1,
            sha256: "a".to_string(),
        });
        m.upsert_artifact(ArtifactEntry {
            variant_id: None,
            filename: "plugin.zip".to_string(),
            size_bytes: 2,
            sha256: "b".to_string(),
        });

        let variants = m.existing_variants();
        assert!(variants.contains(&Some("free".to_string())));
        assert!(variants.contains(&None));
        assert!(m.has_variant(Some("free")));
        assert!(m.has_variant(None));
        assert!(!m.has_variant(Some("pro")));
    }

    #[test]
    fn test_manifest_save_and_load_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        let mut m = manifest();
        m.add_source(BuildSource::PullRequest(42), Some("develop".to_string()));
        m.upsert_artifact(ArtifactEntry {
            variant_id: Some("free".to_string()),
            filename: "backwpup-free.zip".to_string(),
            size_bytes: 12345,
            sha256: "sha256hash".to_string(),
        });

        m.save(dir).unwrap();
        assert!(BuildManifest::path_in(dir).exists());

        let loaded = BuildManifest::load(dir).unwrap();
        assert_eq!(loaded.project, "backwpup");
        assert_eq!(loaded.version, "5.6.0");
        assert_eq!(loaded.commit_short, "abc1234");
        assert_eq!(loaded.sources.len(), 1);
        assert_eq!(loaded.artifacts.len(), 1);
    }

    #[test]
    fn test_manifest_load_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let err = BuildManifest::load(temp_dir.path()).unwrap_err();
        assert!(matches!(err, Error::ManifestNotFound(_)));
    }

    #[test]
    fn test_manifest_load_corrupted_json() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(BuildManifest::path_in(temp_dir.path()), "{not json").unwrap();
        let err = BuildManifest::load(temp_dir.path()).unwrap_err();
        assert!(matches!(err, Error::ManifestCorrupted { .. }));
    }

    #[test]
    fn test_manifest_load_unsupported_schema() {
        let temp_dir = TempDir::new().unwrap();
        let mut m = manifest();
        m.schema_version = 999;
        m.save(temp_dir.path()).unwrap();

        let err = BuildManifest::load(temp_dir.path()).unwrap_err();
        assert!(matches!(err, Error::UnsupportedSchema { found: 999, .. }));
    }

    #[test]
    fn test_manifest_load_rejects_inconsistent_commit_short() {
        let temp_dir = TempDir::new().unwrap();
        let mut m = manifest();
        m.commit_short = "zzzzzzz".to_string();
        // Bypass save-side invariants by writing raw JSON.
        std::fs::write(
            BuildManifest::path_in(temp_dir.path()),
            serde_json::to_vec_pretty(&m).unwrap(),
        )
        .unwrap();

        let err = BuildManifest::load(temp_dir.path()).unwrap_err();
        assert!(matches!(err, Error::ManifestCorrupted { .. }));
    }

    #[test]
    fn test_manifest_load_rejects_duplicate_filenames() {
        let temp_dir = TempDir::new().unwrap();
        let mut m = manifest();
        // Bypass upsert to simulate a hand-tampered manifest.
        for _ in 0..2 {
            m.artifacts.push(ArtifactEntry {
                variant_id: None,
                filename: "plugin.zip".to_string(),
                size_bytes: 1,
                sha256: "a".to_string(),
            });
        }
        std::fs::write(
            BuildManifest::path_in(temp_dir.path()),
            serde_json::to_vec_pretty(&m).unwrap(),
        )
        .unwrap();

        let err = BuildManifest::load(temp_dir.path()).unwrap_err();
        assert!(err.to_string().contains("duplicate artifact filename"));
    }

    #[test]
    fn test_manifest_load_rejects_traversal_filename() {
        let temp_dir = TempDir::new().unwrap();
        let mut m = manifest();
        m.artifacts.push(ArtifactEntry {
            variant_id: None,
            filename: "../../escape.zip".to_string(),
            size_bytes: 1,
            sha256: "a".to_string(),
        });
        std::fs::write(
            BuildManifest::path_in(temp_dir.path()),
            serde_json::to_vec_pretty(&m).unwrap(),
        )
        .unwrap();

        // A tampered filename with separators must never surface as a
        // readable manifest (it would leak paths outside the commit dir).
        let err = BuildManifest::load(temp_dir.path()).unwrap_err();
        assert!(matches!(err, Error::ManifestCorrupted { .. }));
    }

    #[test]
    fn test_manifest_path_in() {
        let dir = Path::new("/some/dir");
        assert_eq!(
            BuildManifest::path_in(dir),
            PathBuf::from("/some/dir/build-manifest.json")
        );
    }
}
