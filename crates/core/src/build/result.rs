//! Build result types.
//!
//! This module defines the output types from a build operation,
//! providing a bridge between the build runner and artifact storage.

use std::path::PathBuf;

/// Result of a successful build operation.
///
/// Contains all information needed to store the build artifacts,
/// including the produced files, version info, and build context.
///
/// # Example
///
/// ```rust,ignore
/// let result = runner.execute_build(&builder, "3.17.4", &[]).await?;
///
/// println!("Built {} artifacts:", result.artifacts.len());
/// for artifact in &result.artifacts {
///     println!("  - {} ({} bytes)", artifact.filename, artifact.size);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct BuildResult {
    /// The artifacts produced by the build.
    pub artifacts: Vec<ProducedArtifact>,
    /// The directory where the build was performed.
    pub build_dir: PathBuf,
    /// The version that was built.
    pub version: String,
    /// The variants that were built (empty if no variants).
    pub variants_built: Vec<String>,
}

impl BuildResult {
    /// Create a new build result.
    pub fn new(
        artifacts: Vec<ProducedArtifact>,
        build_dir: PathBuf,
        version: String,
        variants_built: Vec<String>,
    ) -> Self {
        Self {
            artifacts,
            build_dir,
            version,
            variants_built,
        }
    }

    /// Check if the build produced any artifacts.
    pub fn has_artifacts(&self) -> bool {
        !self.artifacts.is_empty()
    }

    /// Get total size of all artifacts in bytes.
    pub fn total_size(&self) -> u64 {
        self.artifacts.iter().map(|a| a.size).sum()
    }

    /// Get artifacts for a specific variant.
    pub fn artifacts_for_variant(&self, variant_id: &str) -> Vec<&ProducedArtifact> {
        self.artifacts
            .iter()
            .filter(|a| a.variant_id.as_deref() == Some(variant_id))
            .collect()
    }

    /// Get artifacts without a variant (single-variant projects).
    pub fn unvariant_artifacts(&self) -> Vec<&ProducedArtifact> {
        self.artifacts
            .iter()
            .filter(|a| a.variant_id.is_none())
            .collect()
    }
}

/// Where an artifact delivered to the output directory came from.
///
/// Lets consumers report provenance per artifact — including the mixed case
/// of a partial build where some variants are reused from the cache and
/// others are freshly built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactOrigin {
    /// Copied from the artifact cache (a prior build's stored output).
    Cache,
    /// Freshly produced by the builder in this run.
    Built,
    /// Downloaded from a GitHub Release's pre-built assets.
    Downloaded,
}

impl std::fmt::Display for ArtifactOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cache => write!(f, "cache"),
            Self::Built => write!(f, "built"),
            Self::Downloaded => write!(f, "downloaded"),
        }
    }
}

/// A single artifact produced by the build.
///
/// Represents a built file (typically a zip or other archive)
/// that can be stored in the artifact storage system.
#[derive(Debug, Clone)]
pub struct ProducedArtifact {
    /// Variant ID this artifact belongs to (None for single-variant projects).
    pub variant_id: Option<String>,
    /// Full path to the artifact file.
    pub path: PathBuf,
    /// Target filename (how it should be named in storage).
    pub filename: String,
    /// File size in bytes.
    pub size: u64,
    /// Where this artifact came from (built, downloaded, or cache).
    pub origin: ArtifactOrigin,
}

impl ProducedArtifact {
    /// Create a new produced artifact.
    ///
    /// The [`origin`](Self::origin) defaults to [`ArtifactOrigin::Built`] — the
    /// builder is the primary producer. Downloaded or cache-sourced artifacts
    /// override it with [`Self::with_origin`].
    ///
    /// # Arguments
    ///
    /// * `variant_id` - Optional variant identifier
    /// * `path` - Full path to the artifact file
    /// * `filename` - Target filename for storage
    /// * `size` - File size in bytes
    pub fn new(variant_id: Option<String>, path: PathBuf, filename: String, size: u64) -> Self {
        Self {
            variant_id,
            path,
            filename,
            size,
            origin: ArtifactOrigin::Built,
        }
    }

    /// Set the artifact's [`origin`](Self::origin) (builder-style).
    #[must_use]
    pub fn with_origin(mut self, origin: ArtifactOrigin) -> Self {
        self.origin = origin;
        self
    }

    /// Check if this artifact belongs to a specific variant.
    pub fn is_variant(&self, variant_id: &str) -> bool {
        self.variant_id.as_deref() == Some(variant_id)
    }

    /// Check if this is an unvariant (single-variant project) artifact.
    pub fn is_unvariant(&self) -> bool {
        self.variant_id.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_result_total_size() {
        let result = BuildResult::new(
            vec![
                ProducedArtifact::new(None, PathBuf::from("/a.zip"), "a.zip".into(), 100),
                ProducedArtifact::new(None, PathBuf::from("/b.zip"), "b.zip".into(), 200),
            ],
            PathBuf::from("/build"),
            "1.0.0".into(),
            vec![],
        );

        assert_eq!(result.total_size(), 300);
    }

    #[test]
    fn test_artifacts_for_variant() {
        let result = BuildResult::new(
            vec![
                ProducedArtifact::new(
                    Some("pro".into()),
                    PathBuf::from("/pro.zip"),
                    "pro.zip".into(),
                    100,
                ),
                ProducedArtifact::new(
                    Some("free".into()),
                    PathBuf::from("/free.zip"),
                    "free.zip".into(),
                    50,
                ),
            ],
            PathBuf::from("/build"),
            "1.0.0".into(),
            vec!["pro".into(), "free".into()],
        );

        assert_eq!(result.artifacts_for_variant("pro").len(), 1);
        assert_eq!(result.artifacts_for_variant("free").len(), 1);
        assert_eq!(result.artifacts_for_variant("unknown").len(), 0);
    }

    // =========================================================================
    // 6.1 – has_artifacts
    // =========================================================================

    #[test]
    fn test_has_artifacts_true() {
        let result = BuildResult::new(
            vec![ProducedArtifact::new(
                None,
                PathBuf::from("/a.zip"),
                "a.zip".into(),
                100,
            )],
            PathBuf::from("/build"),
            "1.0.0".into(),
            vec![],
        );
        assert!(result.has_artifacts());
    }

    #[test]
    fn test_has_artifacts_false() {
        let result = BuildResult::new(vec![], PathBuf::from("/build"), "1.0.0".into(), vec![]);
        assert!(!result.has_artifacts());
    }

    // =========================================================================
    // 6.2 – unvariant_artifacts
    // =========================================================================

    #[test]
    fn test_unvariant_artifacts() {
        let result = BuildResult::new(
            vec![
                ProducedArtifact::new(None, PathBuf::from("/a.zip"), "a.zip".into(), 100),
                ProducedArtifact::new(
                    Some("pro".into()),
                    PathBuf::from("/pro.zip"),
                    "pro.zip".into(),
                    200,
                ),
                ProducedArtifact::new(None, PathBuf::from("/b.zip"), "b.zip".into(), 50),
            ],
            PathBuf::from("/build"),
            "1.0.0".into(),
            vec![],
        );

        let unvariant = result.unvariant_artifacts();
        assert_eq!(unvariant.len(), 2);
        assert!(unvariant.iter().all(|a| a.variant_id.is_none()));
    }

    // =========================================================================
    // 6.3 – is_variant / is_unvariant
    // =========================================================================

    #[test]
    fn test_is_variant() {
        let artifact = ProducedArtifact::new(
            Some("pro".into()),
            PathBuf::from("/pro.zip"),
            "pro.zip".into(),
            100,
        );
        assert!(artifact.is_variant("pro"));
        assert!(!artifact.is_variant("free"));
        assert!(!artifact.is_unvariant());
    }

    #[test]
    fn test_is_unvariant() {
        let artifact = ProducedArtifact::new(None, PathBuf::from("/a.zip"), "a.zip".into(), 100);
        assert!(artifact.is_unvariant());
        assert!(!artifact.is_variant("pro"));
    }

    // =========================================================================
    // 6.5 – artifact origin
    // =========================================================================

    #[test]
    fn test_new_defaults_origin_to_built() {
        let artifact = ProducedArtifact::new(None, PathBuf::from("/a.zip"), "a.zip".into(), 100);
        assert_eq!(artifact.origin, ArtifactOrigin::Built);
    }

    #[test]
    fn test_with_origin_overrides() {
        let downloaded = ProducedArtifact::new(None, PathBuf::from("/a.zip"), "a.zip".into(), 100)
            .with_origin(ArtifactOrigin::Downloaded);
        assert_eq!(downloaded.origin, ArtifactOrigin::Downloaded);

        let cached = ProducedArtifact::new(None, PathBuf::from("/a.zip"), "a.zip".into(), 100)
            .with_origin(ArtifactOrigin::Cache);
        assert_eq!(cached.origin, ArtifactOrigin::Cache);
    }

    #[test]
    fn test_origin_display() {
        assert_eq!(ArtifactOrigin::Cache.to_string(), "cache");
        assert_eq!(ArtifactOrigin::Built.to_string(), "built");
        assert_eq!(ArtifactOrigin::Downloaded.to_string(), "downloaded");
    }

    // =========================================================================
    // 6.4 – total_size with empty artifacts
    // =========================================================================

    #[test]
    fn test_total_size_empty() {
        let result = BuildResult::new(vec![], PathBuf::from("/build"), "1.0.0".into(), vec![]);
        assert_eq!(result.total_size(), 0);
    }
}
