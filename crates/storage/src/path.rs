//! Path building for artifact storage.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    pub fn to_dir_name(&self) -> String {
        match self {
            Self::PullRequest(num) => format!("pr-{}", num),
            Self::Tag(tag) => format!("tag-{}", Self::sanitize(tag)),
            Self::Branch(branch) => format!("branch-{}", Self::sanitize(branch)),
            Self::Commit(sha) => format!("commit-{}", &sha[..7.min(sha.len())]),
            Self::Release(tag) => format!("release-{}", Self::sanitize(tag)),
        }
    }

    /// Sanitize a string for use in filesystem paths.
    /// Replaces invalid characters with hyphens.
    fn sanitize(s: &str) -> String {
        s.chars()
            .map(|c| match c {
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' => '-',
                c => c,
            })
            .collect()
    }

    /// Parse from a directory name.
    pub fn from_dir_name(name: &str) -> Option<Self> {
        if let Some(num) = name.strip_prefix("pr-") {
            num.parse().ok().map(Self::PullRequest)
        } else if let Some(tag) = name.strip_prefix("tag-") {
            Some(Self::Tag(tag.to_string()))
        } else if let Some(branch) = name.strip_prefix("branch-") {
            Some(Self::Branch(branch.to_string()))
        } else if let Some(commit) = name.strip_prefix("commit-") {
            Some(Self::Commit(commit.to_string()))
        } else if let Some(tag) = name.strip_prefix("release-") {
            Some(Self::Release(tag.to_string()))
        } else {
            None
        }
    }

    /// Get a human-readable description.
    pub fn description(&self) -> String {
        match self {
            Self::PullRequest(num) => format!("PR #{}", num),
            Self::Tag(tag) => format!("Tag {}", tag),
            Self::Branch(branch) => format!("Branch {}", branch),
            Self::Commit(sha) => format!("Commit {}", &sha[..7.min(sha.len())]),
            Self::Release(tag) => format!("Release {}", tag),
        }
    }
}

impl std::fmt::Display for BuildSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description())
    }
}

/// Extracts major.minor from a version string.
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
        [major, minor, ..] => format!("{}.{}", major, minor),
        [major] => format!("{}.0", major),
        _ => "0.0".to_string(),
    }
}

/// Builds storage paths for artifacts.
///
/// # Directory Structure
///
/// ```text
/// {base_dir}/{project}/{major.minor}/{version}/by-commit/{commit}/
/// {base_dir}/{project}/{major.minor}/{version}/by-source/{source}/
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

    /// Path to a source directory (contains commit links).
    pub fn source_dir(&self, project: &str, version: &str, source: &BuildSource) -> PathBuf {
        self.by_source_dir(project, version).join(source.to_dir_name())
    }

    /// Get the relative path from source link to commit directory.
    /// Used for creating relative symlinks on Unix.
    pub fn relative_commit_path(&self, commit_short: &str) -> PathBuf {
        PathBuf::from("../..").join("by-commit").join(commit_short)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // BuildSource Tests
    // =========================================================================

    #[test]
    fn test_build_source_to_dir_name_pr() {
        let source = BuildSource::PullRequest(123);
        assert_eq!(source.to_dir_name(), "pr-123");
    }

    #[test]
    fn test_build_source_to_dir_name_tag() {
        let source = BuildSource::Tag("v1.0.0".to_string());
        assert_eq!(source.to_dir_name(), "tag-v1.0.0");
    }

    #[test]
    fn test_build_source_to_dir_name_branch() {
        let source = BuildSource::Branch("feature/test".to_string());
        assert_eq!(source.to_dir_name(), "branch-feature-test"); // / is sanitized
    }

    #[test]
    fn test_build_source_to_dir_name_commit() {
        let source = BuildSource::Commit("abc1234567890".to_string());
        assert_eq!(source.to_dir_name(), "commit-abc1234");
    }

    #[test]
    fn test_build_source_to_dir_name_commit_short() {
        let source = BuildSource::Commit("abc12".to_string());
        assert_eq!(source.to_dir_name(), "commit-abc12");
    }

    #[test]
    fn test_build_source_sanitize_special_chars() {
        let source = BuildSource::Branch("my:branch*name?".to_string());
        assert_eq!(source.to_dir_name(), "branch-my-branch-name-");
    }

    #[test]
    fn test_build_source_from_dir_name_pr() {
        let source = BuildSource::from_dir_name("pr-456");
        assert_eq!(source, Some(BuildSource::PullRequest(456)));
    }

    #[test]
    fn test_build_source_from_dir_name_tag() {
        let source = BuildSource::from_dir_name("tag-v2.0.0");
        assert_eq!(source, Some(BuildSource::Tag("v2.0.0".to_string())));
    }

    #[test]
    fn test_build_source_from_dir_name_branch() {
        let source = BuildSource::from_dir_name("branch-develop");
        assert_eq!(source, Some(BuildSource::Branch("develop".to_string())));
    }

    #[test]
    fn test_build_source_from_dir_name_commit() {
        let source = BuildSource::from_dir_name("commit-abc1234");
        assert_eq!(source, Some(BuildSource::Commit("abc1234".to_string())));
    }

    #[test]
    fn test_build_source_from_dir_name_invalid() {
        let source = BuildSource::from_dir_name("invalid-format");
        assert_eq!(source, None);
    }

    #[test]
    fn test_build_source_description() {
        assert_eq!(BuildSource::PullRequest(99).description(), "PR #99");
        assert_eq!(BuildSource::Tag("v1.0".to_string()).description(), "Tag v1.0");
        assert_eq!(BuildSource::Branch("main".to_string()).description(), "Branch main");
        assert_eq!(BuildSource::Commit("abcdef1234567".to_string()).description(), "Commit abcdef1");
    }

    #[test]
    fn test_build_source_display() {
        let source = BuildSource::PullRequest(42);
        assert_eq!(format!("{}", source), "PR #42");
    }

    // =========================================================================
    // major_minor() Tests
    // =========================================================================

    #[test]
    fn test_major_minor_three_parts() {
        assert_eq!(major_minor("5.6.0"), "5.6");
        assert_eq!(major_minor("10.20.30"), "10.20");
    }

    #[test]
    fn test_major_minor_two_parts() {
        assert_eq!(major_minor("5.6"), "5.6");
    }

    #[test]
    fn test_major_minor_one_part() {
        assert_eq!(major_minor("5"), "5.0");
    }

    #[test]
    fn test_major_minor_empty() {
        // Empty string results in ".0" because split on "" returns [""]
        // which matches the single-element case [major] => "{major}.0"
        assert_eq!(major_minor(""), ".0");
    }

    #[test]
    fn test_major_minor_many_parts() {
        assert_eq!(major_minor("1.2.3.4.5"), "1.2");
    }

    // =========================================================================
    // PathBuilder Tests
    // =========================================================================

    #[test]
    fn test_path_builder_project_dir() {
        let builder = PathBuilder::new(PathBuf::from("/builds"));
        assert_eq!(builder.project_dir("backwpup"), PathBuf::from("/builds/backwpup"));
    }

    #[test]
    fn test_path_builder_major_minor_dir() {
        let builder = PathBuilder::new(PathBuf::from("/builds"));
        assert_eq!(
            builder.major_minor_dir("backwpup", "5.6.0"),
            PathBuf::from("/builds/backwpup/5.6")
        );
    }

    #[test]
    fn test_path_builder_version_dir() {
        let builder = PathBuilder::new(PathBuf::from("/builds"));
        assert_eq!(
            builder.version_dir("backwpup", "5.6.0"),
            PathBuf::from("/builds/backwpup/5.6/5.6.0")
        );
    }

    #[test]
    fn test_path_builder_commit_dir() {
        let builder = PathBuilder::new(PathBuf::from("/builds"));
        assert_eq!(
            builder.commit_dir("backwpup", "5.6.0", "abc1234"),
            PathBuf::from("/builds/backwpup/5.6/5.6.0/by-commit/abc1234")
        );
    }

    #[test]
    fn test_path_builder_source_dir() {
        let builder = PathBuilder::new(PathBuf::from("/builds"));
        let source = BuildSource::PullRequest(123);
        assert_eq!(
            builder.source_dir("backwpup", "5.6.0", &source),
            PathBuf::from("/builds/backwpup/5.6/5.6.0/by-source/pr-123")
        );
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
