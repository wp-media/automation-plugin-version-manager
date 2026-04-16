//! Build context providing path information to builders.
//!
//! The [`BuildContext`] struct gives builders access to both the repository
//! directory and the surrounding workspace directory, enabling build processes
//! that need to operate outside the repository root (e.g., creating staging
//! directories for packaging).

use std::path::{Path, PathBuf};

/// Paths and context available to builders during the build process.
///
/// Provides two key directories that cover all builder needs:
///
/// - [`repo_dir`](Self::repo_dir) — The cloned repository root (contains `.git/`).
///   Shell commands run here by default. Most builders only need this.
///
/// - [`workspace_dir`](Self::workspace_dir) — The parent directory that contains
///   the repository. Builders that need scratch space outside the repo (e.g.,
///   for staging or packaging) can use this.
///
/// # Directory Layout
///
/// ```text
/// workspace_dir/          ← Isolated temp directory
/// └── repo_dir/           ← Cloned repository (commands run here)
///     ├── .git/
///     ├── wp-rocket.php
///     └── ...
/// ```
///
/// # Usage in Builders
///
/// ```ignore
/// // BackWPup: only needs the repo directory
/// fn pre_build_hook(&self, context: &BuildContext, ...) -> Result<()> {
///     let pattern = context.repo_dir().join("backwpup-*.zip");
///     // ...
/// }
///
/// // WP Rocket: needs workspace for staging
/// fn build_commands(&self, context: &BuildContext, ...) -> Vec<String> {
///     let staging = context.workspace_dir().join("wp-rocket-tmp");
///     let repo = context.repo_dir();
///     // rsync from repo to staging, zip from staging, output to repo
/// }
/// ```
#[derive(Debug, Clone)]
pub struct BuildContext {
    /// The cloned repository root directory.
    repo_dir: PathBuf,
    /// The workspace root directory (parent of repo_dir).
    workspace_dir: PathBuf,
}

impl BuildContext {
    /// Create a new build context from repository and workspace paths.
    ///
    /// # Arguments
    ///
    /// * `repo_dir` - Path to the cloned repository root (where `.git/` lives)
    /// * `workspace_dir` - Path to the parent workspace directory
    pub fn new(repo_dir: PathBuf, workspace_dir: PathBuf) -> Self {
        Self {
            repo_dir,
            workspace_dir,
        }
    }

    /// The cloned repository root directory.
    ///
    /// This is where `.git/` lives and where shell commands run by default.
    /// Most builder operations (glob patterns, artifact lookups) should use
    /// this path.
    pub fn repo_dir(&self) -> &Path {
        &self.repo_dir
    }

    /// The workspace root directory (parent of the repository).
    ///
    /// Use this for operations that need space outside the repository, such
    /// as creating temporary staging directories for packaging.
    ///
    /// This directory is isolated (typically a temp dir) and will be cleaned
    /// up after the build completes.
    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    /// Update the repo directory path.
    ///
    /// Used internally by the build runner when a builder specifies a
    /// [`build_subdirectory`](super::plugins::Builder::build_subdirectory).
    pub(crate) fn set_repo_dir(&mut self, path: PathBuf) {
        self.repo_dir = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_context_accessors() {
        let context = BuildContext::new(
            PathBuf::from("/tmp/build-abc123/my-plugin"),
            PathBuf::from("/tmp/build-abc123"),
        );

        assert_eq!(context.repo_dir(), Path::new("/tmp/build-abc123/my-plugin"));
        assert_eq!(context.workspace_dir(), Path::new("/tmp/build-abc123"));
    }

    #[test]
    fn test_set_repo_dir() {
        let mut context = BuildContext::new(
            PathBuf::from("/tmp/build/repo"),
            PathBuf::from("/tmp/build"),
        );

        context.set_repo_dir(PathBuf::from("/tmp/build/repo/subdir"));
        assert_eq!(context.repo_dir(), Path::new("/tmp/build/repo/subdir"));
        // workspace_dir unchanged
        assert_eq!(context.workspace_dir(), Path::new("/tmp/build"));
    }

    #[test]
    fn test_build_context_clone() {
        let original = BuildContext::new(
            PathBuf::from("/tmp/build/repo"),
            PathBuf::from("/tmp/build"),
        );
        let cloned = original.clone();

        assert_eq!(original.repo_dir(), cloned.repo_dir());
        assert_eq!(original.workspace_dir(), cloned.workspace_dir());
    }
}
