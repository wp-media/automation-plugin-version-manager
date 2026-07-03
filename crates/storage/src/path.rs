//! Path building for artifact storage.
//!
//! Centralizes every on-disk location so the directory layout is defined in
//! exactly one place:
//!
//! ```text
//! {base_dir}/{project}/{major.minor}/{version}/by-commit/{commit_short}/
//! {base_dir}/{project}/{major.minor}/{version}/by-source/{source}/{commit_short}
//! {base_dir}/{project}/releases/{tag}/
//! ```
//!
//! `releases/` can never collide with a `{major.minor}` directory because
//! `major_minor()` output always contains a `.` and `releases` does not.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Name of the per-project directory holding cached GitHub Release assets.
///
/// Lives alongside `{major.minor}` directories; guaranteed collision-free
/// because major.minor names always contain a dot.
pub(crate) const RELEASES_DIR: &str = "releases";

/// Maximum length kept from a sanitized name before the disambiguation
/// suffix. Keeps `{prefix}-{name}-{hash}` well under filesystem limits.
const MAX_SANITIZED_LEN: usize = 80;

/// Source of a build (what was built from).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id")]
pub enum BuildSource {
    /// Built from a Pull Request.
    #[serde(rename = "pr")]
    PullRequest(u64),
    /// Built from a tag.
    #[serde(rename = "tag")]
    Tag(String),
    /// Built from a branch.
    #[serde(rename = "branch")]
    Branch(String),
    /// Built from a specific commit.
    #[serde(rename = "commit")]
    Commit(String),
    /// Built from a GitHub Release (pre-built assets downloaded).
    #[serde(rename = "release")]
    Release(String),
}

impl BuildSource {
    /// Convert to a filesystem-safe directory name.
    ///
    /// Names needing sanitization get a deterministic 8-hex-char suffix
    /// derived from the *original* value, so distinct sources that sanitize
    /// to the same text (e.g. branches `a/b` and `a-b`) still get distinct
    /// directories. Unmodified names (the common case: `develop`, `v1.0.0`)
    /// stay clean and human-readable.
    pub fn to_dir_name(&self) -> String {
        match self {
            Self::PullRequest(num) => format!("pr-{num}"),
            Self::Tag(tag) => format!("tag-{}", safe_component(tag)),
            Self::Branch(branch) => format!("branch-{}", safe_component(branch)),
            // Commits are hex in practice, but the enum does not enforce it:
            // sanitize (not slice) so a malformed value can never panic on a
            // char boundary or produce an unsafe name.
            Self::Commit(sha) => {
                let short: String = sha.to_ascii_lowercase().chars().take(7).collect();
                format!("commit-{}", safe_component(&short))
            }
            Self::Release(tag) => format!("release-{}", safe_component(tag)),
        }
    }

    /// Parse from a directory name (best effort).
    ///
    /// **Lossy**: sanitized characters and disambiguation suffixes cannot be
    /// reversed (`branch-feature-test-1a2b3c4d` parses as
    /// `Branch("feature-test-1a2b3c4d")`, not `Branch("feature/test")`).
    /// Exact sources are recorded in build manifests —
    /// [`crate::store::ArtifactStore::list_sources`] prefers those and only
    /// falls back to this parser for directories without readable manifests.
    pub fn from_dir_name(name: &str) -> Option<Self> {
        if let Some(num) = name.strip_prefix("pr-") {
            num.parse().ok().map(Self::PullRequest)
        } else if let Some(tag) = name.strip_prefix("tag-") {
            Some(Self::Tag(tag.to_string()))
        } else if let Some(branch) = name.strip_prefix("branch-") {
            Some(Self::Branch(branch.to_string()))
        } else if let Some(commit) = name.strip_prefix("commit-") {
            Some(Self::Commit(commit.to_string()))
        } else {
            name.strip_prefix("release-")
                .map(|tag| Self::Release(tag.to_string()))
        }
    }

    /// Get a human-readable description.
    pub fn description(&self) -> String {
        match self {
            Self::PullRequest(num) => format!("PR #{num}"),
            Self::Tag(tag) => format!("Tag {tag}"),
            Self::Branch(branch) => format!("Branch {branch}"),
            // chars() (not byte slicing) so a non-hex value with multibyte
            // characters can never panic on a char boundary.
            Self::Commit(sha) => {
                format!("Commit {}", sha.chars().take(7).collect::<String>())
            }
            Self::Release(tag) => format!("Release {tag}"),
        }
    }
}

impl std::fmt::Display for BuildSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description())
    }
}

/// Sanitize an arbitrary string into a filesystem-safe path component.
///
/// Allowed characters (`A-Z a-z 0-9 . _ +`) pass through; everything else
/// (separators, spaces, control chars, Windows-forbidden punctuation, and
/// `-` handled implicitly as the replacement char) maps to `-`. The result
/// is truncated to [`MAX_SANITIZED_LEN`] characters and trailing dots are
/// stripped (Windows silently drops them, which would desync paths).
///
/// If the result differs from the input in any way, an 8-hex-char SHA-256
/// suffix of the *original* string is appended so distinct inputs always
/// yield distinct outputs.
pub(crate) fn safe_component(original: &str) -> String {
    let mut sanitized: String = original
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(MAX_SANITIZED_LEN)
        .collect();

    while sanitized.ends_with('.') {
        sanitized.pop();
    }
    // Leading dots would create hidden directories, which store listing
    // code skips as internal entries.
    while sanitized.starts_with('.') {
        sanitized.remove(0);
    }

    // Windows reserved device names (CON, NUL, COM1, …) cannot be created
    // as directories there — force the suffix path for them. This mostly
    // matters for release tags, which become directory names without a
    // disambiguating prefix.
    if sanitized == original && !crate::validate::is_windows_reserved(&sanitized) {
        return sanitized;
    }

    // Deterministic disambiguation: two different originals that sanitize to
    // the same text still produce different directory names.
    let digest = Sha256::digest(original.as_bytes());
    let suffix: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();

    if sanitized.is_empty() {
        suffix
    } else {
        format!("{sanitized}-{suffix}")
    }
}

/// Extracts major.minor from a version string.
///
/// The result always contains a `.`, which is what keeps `{major.minor}`
/// directories disjoint from the reserved `releases` directory.
///
/// # Examples
///
/// - `"5.6.0"` → `"5.6"`
/// - `"5.6.1"` → `"5.6"`
/// - `"10.20.30"` → `"10.20"`
/// - `"5"` → `"5.0"`
pub fn major_minor(version: &str) -> String {
    let parts: Vec<&str> = version.split('.').collect();
    match parts.as_slice() {
        [major, minor, ..] => format!("{major}.{minor}"),
        [major] => format!("{major}.0"),
        _ => "0.0".to_string(),
    }
}

/// Compare two version strings with numeric-aware ordering.
///
/// Dot-separated segments are compared pairwise: by their leading numeric
/// value first (`10.0` > `9.0`, unlike lexicographic order), then by the
/// non-numeric remainder (`5.6.0-beta1` < `5.6.0-beta2`). A version that is
/// a prefix of another sorts first (`5.6` < `5.6.0`).
///
/// Intended for *sorting* human-oriented version lists; it deliberately does
/// not implement full semver precedence (plugin versions are frequently not
/// semver).
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    /// Split a segment into its leading numeric value and the remainder
    /// (`"6rc1"` → `(Some(6), "rc1")`, `"beta"` → `(None, "beta")`).
    fn split_segment(seg: &str) -> (Option<u64>, &str) {
        let digits_end = seg
            .char_indices()
            .find(|(_, c)| !c.is_ascii_digit())
            .map(|(i, _)| i)
            .unwrap_or(seg.len());
        let num = seg[..digits_end].parse::<u64>().ok();
        (num, &seg[digits_end..])
    }

    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(l), Some(r)) => {
                let (ln, lrest) = split_segment(l);
                let (rn, rrest) = split_segment(r);
                // Numeric segments sort before purely textual ones.
                let ord = match (ln, rn) {
                    (Some(x), Some(y)) => x.cmp(&y).then_with(|| lrest.cmp(rrest)),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => lrest.cmp(rrest),
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// Builds storage paths for artifacts.
///
/// # Directory Structure
///
/// ```text
/// {base_dir}/{project}/{major.minor}/{version}/by-commit/{commit_short}/
/// {base_dir}/{project}/{major.minor}/{version}/by-source/{source}/
/// {base_dir}/{project}/releases/{tag}/
/// ```
#[derive(Debug, Clone)]
pub struct PathBuilder {
    base_dir: PathBuf,
}

impl PathBuilder {
    /// Create a new path builder with the given base directory.
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Get the base directory.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Path to a project directory.
    pub fn project_dir(&self, project: &str) -> PathBuf {
        self.base_dir.join(project)
    }

    /// Path to a major.minor directory.
    pub fn major_minor_dir(&self, project: &str, version: &str) -> PathBuf {
        self.project_dir(project).join(major_minor(version))
    }

    /// Path to a version directory.
    pub fn version_dir(&self, project: &str, version: &str) -> PathBuf {
        self.major_minor_dir(project, version).join(version)
    }

    /// Path to the by-commit directory (contains all commit directories).
    pub fn by_commit_dir(&self, project: &str, version: &str) -> PathBuf {
        self.version_dir(project, version).join("by-commit")
    }

    /// Path to a specific commit's build directory.
    pub fn commit_dir(&self, project: &str, version: &str, commit_short: &str) -> PathBuf {
        self.by_commit_dir(project, version).join(commit_short)
    }

    /// Path to the by-source directory (contains all source links).
    pub fn by_source_dir(&self, project: &str, version: &str) -> PathBuf {
        self.version_dir(project, version).join("by-source")
    }

    /// Path to a source directory (contains commit links).
    pub fn source_dir(&self, project: &str, version: &str, source: &BuildSource) -> PathBuf {
        self.by_source_dir(project, version)
            .join(source.to_dir_name())
    }

    /// Path to a source link.
    pub fn source_link(
        &self,
        project: &str,
        version: &str,
        source: &BuildSource,
        commit_short: &str,
    ) -> PathBuf {
        self.source_dir(project, version, source).join(commit_short)
    }

    /// Path to a project's releases cache directory.
    pub fn releases_dir(&self, project: &str) -> PathBuf {
        self.project_dir(project).join(RELEASES_DIR)
    }

    /// Path to a specific cached release directory.
    ///
    /// The tag is sanitized with the same collision-proof scheme as source
    /// directories, so tags like `release/5.6` are safe.
    pub fn release_dir(&self, project: &str, tag: &str) -> PathBuf {
        self.releases_dir(project).join(safe_component(tag))
    }

    /// Get the relative path from a source link to its commit directory.
    /// Used for creating relative symlinks on Unix (keeps stores relocatable).
    pub fn relative_commit_path(&self, commit_short: &str) -> PathBuf {
        // Link lives at {version}/by-source/{source}/{commit}; ../../
        // climbs to {version}, then descends into by-commit.
        PathBuf::from("../..").join("by-commit").join(commit_short)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    // =========================================================================
    // BuildSource → dir name
    // =========================================================================

    #[test]
    fn test_build_source_to_dir_name_pr() {
        assert_eq!(BuildSource::PullRequest(123).to_dir_name(), "pr-123");
    }

    #[test]
    fn test_build_source_to_dir_name_tag_clean() {
        // Clean names stay human-readable with no suffix.
        assert_eq!(
            BuildSource::Tag("v1.0.0".to_string()).to_dir_name(),
            "tag-v1.0.0"
        );
    }

    #[test]
    fn test_build_source_to_dir_name_branch_sanitized_gets_suffix() {
        let name = BuildSource::Branch("feature/test".to_string()).to_dir_name();
        assert!(name.starts_with("branch-feature-test-"));
        // 8 hex chars of disambiguation suffix.
        let suffix = name.rsplit('-').next().unwrap();
        assert_eq!(suffix.len(), 8);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_build_source_dir_names_distinct_for_colliding_sanitizations() {
        // "a/b" and "a-b" sanitize to the same text but must get distinct dirs.
        let a = BuildSource::Branch("a/b".to_string()).to_dir_name();
        let b = BuildSource::Branch("a-b".to_string()).to_dir_name();
        assert_ne!(a, b);
    }

    #[test]
    fn test_build_source_to_dir_name_commit_normalizes() {
        assert_eq!(
            BuildSource::Commit("ABC1234567890".to_string()).to_dir_name(),
            "commit-abc1234"
        );
        assert_eq!(
            BuildSource::Commit("abc12".to_string()).to_dir_name(),
            "commit-abc12"
        );
    }

    #[test]
    fn test_build_source_commit_multibyte_does_not_panic() {
        // The enum's inner String is unvalidated; a multibyte value landing
        // on byte index 7 used to panic with byte slicing. Both methods must
        // stay total.
        for weird in ["abcde日", "日本語のブランチ", "é", ""] {
            let source = BuildSource::Commit(weird.to_string());
            let dir = source.to_dir_name();
            assert!(dir.starts_with("commit-"));
            let _ = source.description();
        }
    }

    #[test]
    fn test_safe_component_windows_reserved_gets_suffix() {
        // A bare reserved device name would be uncreatable as a directory
        // on Windows; the suffix path must kick in even though the name
        // contains only allowed characters.
        for reserved in ["CON", "con", "NUL", "com1", "PRN.txt"] {
            let out = safe_component(reserved);
            assert_ne!(out, reserved, "{reserved:?} must not pass through bare");
        }
        // Normal names remain untouched.
        assert_eq!(safe_component("v1.0.0"), "v1.0.0");
        assert_eq!(safe_component("develop"), "develop");
    }

    #[test]
    fn test_safe_component_long_multibyte_no_panic() {
        // Truncation operates on chars, so multibyte input of any length is
        // safe and stays within component limits.
        let long = "日本語".repeat(100);
        let out = safe_component(&long);
        assert!(out.len() <= MAX_SANITIZED_LEN + 9);
        assert!(out.is_ascii());
    }

    #[test]
    fn test_compare_versions_extreme_segments() {
        use std::cmp::Ordering;
        // Numeric overflow (> u64::MAX) falls back to textual comparison
        // without panicking; empty segments compare consistently.
        let huge = "99999999999999999999999999.0";
        assert_ne!(compare_versions(huge, "1.0"), Ordering::Equal);
        assert_eq!(compare_versions("5..6", "5..6"), Ordering::Equal);
        assert_eq!(compare_versions("5..6", "5.0.6"), Ordering::Greater);
    }

    #[test]
    fn test_build_source_to_dir_name_deterministic() {
        let s = BuildSource::Branch("feature/test one".to_string());
        assert_eq!(s.to_dir_name(), s.to_dir_name());
    }

    #[test]
    fn test_safe_component_truncates_long_names() {
        let long = "x".repeat(500);
        let out = safe_component(&long);
        assert!(out.len() <= MAX_SANITIZED_LEN + 9); // truncated + "-" + 8 hex
    }

    #[test]
    fn test_safe_component_strips_trailing_dots() {
        let out = safe_component("v1.");
        assert!(!out.contains('.') || !out.ends_with('.'));
        assert!(out.starts_with("v1"));
        assert_ne!(out, "v1."); // suffix appended because input changed
    }

    #[test]
    fn test_safe_component_only_invalid_chars() {
        let out = safe_component("///");
        // Sanitizes to "---" + suffix (never empty, never ambiguous).
        assert!(!out.is_empty());
        assert_ne!(safe_component("///"), safe_component("\\\\\\"));
    }

    // =========================================================================
    // BuildSource ← dir name (best-effort)
    // =========================================================================

    #[test]
    fn test_build_source_from_dir_name() {
        assert_eq!(
            BuildSource::from_dir_name("pr-456"),
            Some(BuildSource::PullRequest(456))
        );
        assert_eq!(
            BuildSource::from_dir_name("tag-v2.0.0"),
            Some(BuildSource::Tag("v2.0.0".to_string()))
        );
        assert_eq!(
            BuildSource::from_dir_name("branch-develop"),
            Some(BuildSource::Branch("develop".to_string()))
        );
        assert_eq!(
            BuildSource::from_dir_name("commit-abc1234"),
            Some(BuildSource::Commit("abc1234".to_string()))
        );
        assert_eq!(
            BuildSource::from_dir_name("release-v5.6.0"),
            Some(BuildSource::Release("v5.6.0".to_string()))
        );
        assert_eq!(BuildSource::from_dir_name("invalid-format"), None);
    }

    #[test]
    fn test_build_source_description_and_display() {
        assert_eq!(BuildSource::PullRequest(99).description(), "PR #99");
        assert_eq!(BuildSource::Tag("v1.0".into()).description(), "Tag v1.0");
        assert_eq!(
            BuildSource::Branch("main".into()).description(),
            "Branch main"
        );
        assert_eq!(
            BuildSource::Commit("abcdef1234567".into()).description(),
            "Commit abcdef1"
        );
        assert_eq!(format!("{}", BuildSource::PullRequest(42)), "PR #42");
    }

    // =========================================================================
    // major_minor()
    // =========================================================================

    #[test]
    fn test_major_minor() {
        assert_eq!(major_minor("5.6.0"), "5.6");
        assert_eq!(major_minor("10.20.30"), "10.20");
        assert_eq!(major_minor("5.6"), "5.6");
        assert_eq!(major_minor("5"), "5.0");
        assert_eq!(major_minor("1.2.3.4.5"), "1.2");
    }

    // =========================================================================
    // compare_versions()
    // =========================================================================

    #[test]
    fn test_compare_versions_numeric_aware() {
        assert_eq!(compare_versions("10.0", "9.0"), Ordering::Greater);
        assert_eq!(compare_versions("5.6.0", "5.6.1"), Ordering::Less);
        assert_eq!(compare_versions("5.6.0", "5.6.0"), Ordering::Equal);
        assert_eq!(compare_versions("5.6", "5.6.0"), Ordering::Less);
        assert_eq!(compare_versions("5.10.0", "5.9.9"), Ordering::Greater);
    }

    #[test]
    fn test_compare_versions_textual_segments() {
        assert_eq!(
            compare_versions("5.6.0-beta1", "5.6.0-beta2"),
            Ordering::Less
        );
        // Numeric-leading segment sorts before purely textual segment.
        assert_eq!(compare_versions("5.6.1", "5.6.beta"), Ordering::Less);
    }

    // =========================================================================
    // PathBuilder
    // =========================================================================

    #[test]
    fn test_path_builder_layout() {
        let builder = PathBuilder::new(PathBuf::from("/builds"));
        assert_eq!(
            builder.project_dir("backwpup"),
            PathBuf::from("/builds/backwpup")
        );
        assert_eq!(
            builder.major_minor_dir("backwpup", "5.6.0"),
            PathBuf::from("/builds/backwpup/5.6")
        );
        assert_eq!(
            builder.version_dir("backwpup", "5.6.0"),
            PathBuf::from("/builds/backwpup/5.6/5.6.0")
        );
        assert_eq!(
            builder.commit_dir("backwpup", "5.6.0", "abc1234"),
            PathBuf::from("/builds/backwpup/5.6/5.6.0/by-commit/abc1234")
        );
        assert_eq!(
            builder.source_dir("backwpup", "5.6.0", &BuildSource::PullRequest(123)),
            PathBuf::from("/builds/backwpup/5.6/5.6.0/by-source/pr-123")
        );
    }

    #[test]
    fn test_path_builder_release_dirs() {
        let builder = PathBuilder::new(PathBuf::from("/builds"));
        assert_eq!(
            builder.releases_dir("backwpup"),
            PathBuf::from("/builds/backwpup/releases")
        );
        assert_eq!(
            builder.release_dir("backwpup", "v5.6.0"),
            PathBuf::from("/builds/backwpup/releases/v5.6.0")
        );
        // Tags with separators are sanitized (with suffix), staying inside
        // the releases dir.
        let sneaky = builder.release_dir("backwpup", "../../escape");
        assert!(sneaky.starts_with("/builds/backwpup/releases"));
    }

    #[test]
    fn test_path_builder_relative_commit_path() {
        let builder = PathBuilder::new(PathBuf::from("/builds"));
        assert_eq!(
            builder.relative_commit_path("abc1234"),
            PathBuf::from("../..").join("by-commit").join("abc1234")
        );
    }
}
