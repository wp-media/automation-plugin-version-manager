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
}

impl BuildSource {
    /// Convert to a filesystem-safe directory name.
    pub fn to_dir_name(&self) -> String {
        match self {
            Self::PullRequest(num) => format!("pr-{}", num),
            Self::Tag(tag) => format!("tag-{}", Self::sanitize(tag)),
            Self::Branch(branch) => format!("branch-{}", Self::sanitize(branch)),
            Self::Commit(sha) => format!("commit-{}", &sha[..7.min(sha.len())]),
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
