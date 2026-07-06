//! Public data types: inputs handed to the store and records read back out.
//!
//! Naming convention: `*Metadata` / [`SourceArtifact`] describe what a caller
//! wants to store; `Stored*` describe what the store holds. All timestamps
//! are UTC.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

// ============================================================================
// Build sources
// ============================================================================

/// Where a build came from: the git reference that was resolved and built.
///
/// A single build (identified by project + version + commit) can be reached
/// from several sources over time — e.g. a PR and its underlying branch — so
/// sources are stored as links to a build, not as part of its identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BuildSource {
    /// Pull request number.
    PullRequest(u64),
    /// Branch name.
    Branch(String),
    /// Tag name.
    Tag(String),
    /// Commit SHA (as provided by the resolver; may be short or full).
    Commit(String),
    /// GitHub Release tag (pre-built assets).
    Release(String),
}

impl BuildSource {
    /// Stable discriminant used as the `kind` column in the database.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::PullRequest(_) => "pr",
            Self::Branch(_) => "branch",
            Self::Tag(_) => "tag",
            Self::Commit(_) => "commit",
            Self::Release(_) => "release",
        }
    }

    /// The reference value paired with [`Self::kind`] (PR number, branch
    /// name, tag, ...). Together they uniquely identify a source.
    pub fn reference(&self) -> String {
        match self {
            Self::PullRequest(number) => number.to_string(),
            Self::Branch(name) | Self::Tag(name) | Self::Commit(name) | Self::Release(name) => {
                name.clone()
            }
        }
    }

    /// Rebuild a `BuildSource` from its stored `(kind, reference)` pair.
    ///
    /// Returns `None` when the pair is not representable (unknown kind, or a
    /// PR reference that is not a number) — callers treat such rows as
    /// corrupt and skip them.
    pub(crate) fn from_kind_reference(kind: &str, reference: &str) -> Option<Self> {
        match kind {
            "pr" => reference.parse().ok().map(Self::PullRequest),
            "branch" => Some(Self::Branch(reference.to_string())),
            "tag" => Some(Self::Tag(reference.to_string())),
            "commit" => Some(Self::Commit(reference.to_string())),
            "release" => Some(Self::Release(reference.to_string())),
            _ => None,
        }
    }

    /// Human-readable description, e.g. `PR #123` or `branch 'develop'`.
    pub fn description(&self) -> String {
        match self {
            Self::PullRequest(number) => format!("PR #{number}"),
            Self::Branch(name) => format!("branch '{name}'"),
            Self::Tag(name) => format!("tag '{name}'"),
            Self::Commit(sha) => {
                // chars() (not byte slicing) so non-hex multibyte input can
                // never panic on a char boundary.
                format!("commit {}", sha.chars().take(7).collect::<String>())
            }
            Self::Release(tag) => format!("release '{tag}'"),
        }
    }
}

impl std::fmt::Display for BuildSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description())
    }
}

// ============================================================================
// Store inputs
// ============================================================================

/// Metadata describing a build about to be stored.
#[derive(Debug, Clone)]
pub struct BuildMetadata {
    /// Project identifier (lowercase, e.g. `backwpup`).
    pub project: String,
    /// Version the build was produced for (e.g. `5.6.0`).
    pub version: String,
    /// The git reference that produced this build.
    pub source: BuildSource,
    /// Commit SHA that was built — full 40-char SHA preferred, 7+ hex chars
    /// accepted. Case-insensitive; normalized to lowercase on store.
    pub commit: String,
    /// Branch that was checked out (kept even for PR sources, so the backing
    /// branch of a PR build remains known).
    pub branch: Option<String>,
    /// When the build was produced. `None` means "now" at store time.
    pub built_at: Option<DateTime<Utc>>,
}

impl BuildMetadata {
    /// Create metadata for a build.
    ///
    /// `branch` accepts both `String` and `Option<String>`; `built_at`
    /// defaults to the store time (override with [`Self::with_built_at`]).
    pub fn new(
        project: impl Into<String>,
        version: impl Into<String>,
        source: BuildSource,
        commit: impl Into<String>,
        branch: impl Into<Option<String>>,
    ) -> Self {
        Self {
            project: project.into(),
            version: version.into(),
            source,
            commit: commit.into(),
            branch: branch.into(),
            built_at: None,
        }
    }

    /// Set an explicit build timestamp (useful for tests and backfills).
    #[must_use]
    pub fn with_built_at(mut self, built_at: DateTime<Utc>) -> Self {
        self.built_at = Some(built_at);
        self
    }
}

/// A produced file to copy into the store.
#[derive(Debug, Clone)]
pub struct SourceArtifact {
    /// Build variant this artifact belongs to (e.g. `pro-en`), or `None`
    /// for single-output plugins. Ignored for release assets.
    pub variant_id: Option<String>,
    /// Path of the file to copy from.
    pub path: PathBuf,
    /// Filename the artifact should have inside the store.
    pub target_name: String,
}

/// Metadata describing a GitHub Release whose assets are about to be cached.
///
/// Releases are keyed by tag (not commit): GitHub resolves a release to a
/// tag, and the mutable flags (`draft`, `prerelease`, ...) are refreshed on
/// every re-store so an un-drafted release updates its cached metadata.
#[derive(Debug, Clone)]
pub struct ReleaseMetadata {
    /// Project identifier (lowercase, e.g. `backwpup`).
    pub project: String,
    /// The release tag, verbatim (stored exactly; the directory name on disk
    /// is a sanitized derivation).
    pub tag: String,
    /// Version string parsed from the tag, if known.
    pub version: Option<String>,
    /// Whether GitHub marks the release as a prerelease.
    pub prerelease: bool,
    /// Whether GitHub marks the release as a draft.
    pub draft: bool,
    /// Publish timestamp reported by GitHub.
    pub published_at: Option<DateTime<Utc>>,
    /// The commitish the release targets, as reported by GitHub.
    pub target_commitish: Option<String>,
}

impl ReleaseMetadata {
    /// Create release metadata with all optional fields empty/false.
    pub fn new(project: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            tag: tag.into(),
            version: None,
            prerelease: false,
            draft: false,
            published_at: None,
            target_commitish: None,
        }
    }
}

// ============================================================================
// Stored records
// ============================================================================

/// A file held by the store, with its recorded integrity data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredArtifact {
    /// Build variant, or `None` for single-output builds and release assets.
    pub variant_id: Option<String>,
    /// Filename inside the store directory.
    pub filename: String,
    /// Size recorded when the file was stored.
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 recorded while the file was copied in.
    pub sha256: String,
    /// Absolute path of the stored file.
    pub path: PathBuf,
}

/// A source link attached to a stored build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLink {
    /// The source that produced (or re-resolved to) the build.
    pub source: BuildSource,
    /// Branch that backed the source at link time, if known.
    pub branch: Option<String>,
    /// When this source was last linked to the build.
    pub linked_at: DateTime<Utc>,
}

/// A build held by the store.
#[derive(Debug, Clone)]
pub struct StoredBuild {
    /// Database row id (stable for the lifetime of the record; useful for
    /// logging and targeted deletes).
    pub id: i64,
    /// Project identifier.
    pub project: String,
    /// Version the build was produced for.
    pub version: String,
    /// Commit SHA as stored (lowercase hex; full length when known).
    pub commit: String,
    /// When the build was produced.
    pub built_at: DateTime<Utc>,
    /// Last time a lookup returned this build (drives age-based cleaning).
    pub last_used_at: DateTime<Utc>,
    /// Absolute directory holding the artifact files.
    pub dir: PathBuf,
    /// The artifacts recorded for this build.
    pub artifacts: Vec<StoredArtifact>,
    /// Every source that has pointed at this build, newest link first.
    pub sources: Vec<SourceLink>,
}

impl StoredBuild {
    /// Whether an artifact for `variant` is recorded (`None` = the
    /// variant-less artifact of single-output plugins).
    pub fn has_variant(&self, variant: Option<&str>) -> bool {
        self.artifacts
            .iter()
            .any(|artifact| artifact.variant_id.as_deref() == variant)
    }

    /// The artifact recorded for `variant`, if any.
    pub fn artifact(&self, variant: Option<&str>) -> Option<&StoredArtifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.variant_id.as_deref() == variant)
    }
}

/// A cached GitHub Release held by the store.
#[derive(Debug, Clone)]
pub struct StoredRelease {
    /// Database row id.
    pub id: i64,
    /// Project identifier.
    pub project: String,
    /// The release tag, exactly as it was stored.
    pub tag: String,
    /// Version string recorded for the release, if any.
    pub version: Option<String>,
    /// Whether the release was marked prerelease at the last store.
    pub prerelease: bool,
    /// Whether the release was marked draft at the last store.
    pub draft: bool,
    /// Publish timestamp reported by GitHub, if any.
    pub published_at: Option<DateTime<Utc>>,
    /// The commitish the release targets, if reported.
    pub target_commitish: Option<String>,
    /// When the assets were (last) cached.
    pub cached_at: DateTime<Utc>,
    /// Last time a lookup returned this release.
    pub last_used_at: DateTime<Utc>,
    /// Absolute directory holding the asset files.
    pub dir: PathBuf,
    /// The assets recorded for this release (`variant_id` is always `None`).
    pub assets: Vec<StoredArtifact>,
}

// ============================================================================
// Operation results
// ============================================================================

/// Outcome of [`crate::ArtifactStore::store`].
#[derive(Debug, Clone)]
pub struct StoreResult {
    /// The stored build as it now exists (all artifacts, all source links).
    pub build: StoredBuild,
    /// Filenames that were copied in by this call.
    pub newly_stored: Vec<String>,
    /// Filenames that were already present and healthy, so no copy happened.
    pub reused: Vec<String>,
}

/// Outcome of [`crate::ArtifactStore::store_release`].
#[derive(Debug, Clone)]
pub struct StoreReleaseResult {
    /// The cached release as it now exists.
    pub release: StoredRelease,
    /// Asset filenames that were copied in by this call.
    pub newly_stored: Vec<String>,
    /// Asset filenames that were already present and healthy.
    pub reused: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_source_kind_reference_round_trip() {
        let sources = [
            BuildSource::PullRequest(123),
            BuildSource::Branch("feature/x".into()),
            BuildSource::Tag("v1.0.0".into()),
            BuildSource::Commit("a1b2c3d".into()),
            BuildSource::Release("v5.3.2".into()),
        ];
        for source in sources {
            let restored =
                BuildSource::from_kind_reference(source.kind(), &source.reference()).unwrap();
            assert_eq!(source, restored);
        }
    }

    #[test]
    fn build_source_rejects_unknown_kind_and_bad_pr() {
        assert!(BuildSource::from_kind_reference("unknown", "x").is_none());
        assert!(BuildSource::from_kind_reference("pr", "not-a-number").is_none());
    }

    #[test]
    fn metadata_new_accepts_string_and_option_branch() {
        let with_string = BuildMetadata::new(
            "backwpup",
            "5.6.0",
            BuildSource::PullRequest(1),
            "a1b2c3d",
            "develop".to_string(),
        );
        assert_eq!(with_string.branch.as_deref(), Some("develop"));

        let with_none = BuildMetadata::new(
            "backwpup",
            "5.6.0",
            BuildSource::PullRequest(1),
            "a1b2c3d",
            None,
        );
        assert!(with_none.branch.is_none());
    }
}
