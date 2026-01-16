//! Build manifest for tracking builds.
//!
//! Each build directory contains a `build-manifest.json` file with metadata
//! about the build, including project info, commit details, and artifact checksums.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::path::BuildSource;

/// Manifest for a single build.
///
/// Stored as `build-manifest.json` in each commit directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildManifest {
    /// Schema version for future compatibility.
    pub schema_version: u32,

    /// Project name.
    pub project: String,

    /// Version that was built.
    pub version: String,

    /// Full commit hash.
    pub commit: String,

    /// Short commit hash (7 characters).
    pub commit_short: String,

    /// When the build was created.
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

    /// When this source was linked.
    pub linked_at: DateTime<Utc>,
}

/// Entry for a single artifact file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    /// Variant ID (None if single-variant project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant_id: Option<String>,

    /// Filename.
    pub filename: String,

    /// File size in bytes.
    pub size_bytes: u64,

    /// SHA256 checksum.
    pub sha256: String,
}

impl BuildManifest {
    /// Current schema version.
    pub const CURRENT_SCHEMA: u32 = 1;

    /// Manifest filename.
    pub const FILENAME: &'static str = "build-manifest.json";

    /// Create a new manifest.
    pub fn new(project: String, version: String, commit: String) -> Self {
        let commit_short = commit.chars().take(7).collect();

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

    /// Add an artifact to the manifest.
    pub fn add_artifact(&mut self, entry: ArtifactEntry) {
        self.artifacts.push(entry);
    }

    /// Add a source to the manifest.
    pub fn add_source(&mut self, source: BuildSource, branch: Option<String>) {
        // Check if source already exists
        let exists = self.sources.iter().any(|s| s.source == source);
        if !exists {
            self.sources.push(SourceEntry {
                source,
                branch,
                linked_at: Utc::now(),
            });
        }
    }

    /// Get variant IDs of existing artifacts.
    pub fn existing_variants(&self) -> Vec<Option<String>> {
        self.artifacts.iter().map(|a| a.variant_id.clone()).collect()
    }

    /// Check if a variant already exists.
    pub fn has_variant(&self, variant_id: Option<&str>) -> bool {
        self.artifacts
            .iter()
            .any(|a| a.variant_id.as_deref() == variant_id)
    }

    /// Load manifest from a directory.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(Self::FILENAME);

        if !path.exists() {
            return Err(Error::InvalidManifest(format!(
                "Manifest not found at {}",
                path.display()
            )));
        }

        let content = std::fs::read_to_string(&path)?;
        let manifest: Self = serde_json::from_str(&content).map_err(|e| {
            Error::InvalidManifest(format!("Failed to parse manifest: {}", e))
        })?;

        Ok(manifest)
    }

    /// Save manifest to a directory.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(Self::FILENAME);

        // Ensure directory exists
        std::fs::create_dir_all(dir)?;

        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)?;

        Ok(())
    }

    /// Get the manifest file path for a directory.
    pub fn path_in(dir: &Path) -> std::path::PathBuf {
        dir.join(Self::FILENAME)
    }
}
