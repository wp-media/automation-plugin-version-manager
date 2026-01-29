//! Build workspace management using temporary directories.
//!
//! Provides isolated workspaces for builds using temp directories that are
//! automatically cleaned up when the workspace is dropped.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tempfile::{Builder, TempDir};

use crate::error::{Error, Result};

use super::repository::Repository;

/// An isolated workspace for a single build operation.
///
/// Uses a temporary directory that is automatically cleaned up when dropped.
/// This ensures no leftover files even if the build fails or panics.
///
/// # Lifecycle
///
/// ```text
/// 1. new()           → Creates temp dir with consumer-provided namespace prefix
/// 2. clone_repo()    → Clones repository into workspace
/// 3. [build runs]    → Artifacts created in workspace
/// 4. collect_to()    → Moves artifacts to output directory
/// 5. Drop            → Temp dir automatically deleted
/// ```
///
/// # Example
///
/// ```ignore
/// let workspace = BuildWorkspace::new("myapp", "https://github.com/...", None)?;
/// workspace.clone_repo().await?;
/// let repo = workspace.repository();
///
/// // ... run build ...
///
/// // Move artifacts to output before workspace is dropped
/// workspace.collect_artifacts(&artifact_paths, output_dir)?;
/// // workspace automatically cleaned up here
/// ```
pub struct BuildWorkspace {
    /// The temporary directory (auto-cleaned on drop).
    temp_dir: TempDir,
    /// Repository URL to clone.
    repo_url: String,
    /// Repository name (derived from URL).
    repo_name: String,
    /// GitHub token for private repository access.
    github_token: Option<String>,
    /// Cached repository handle after cloning.
    repository: OnceLock<Repository>,
}

impl BuildWorkspace {
    /// Create a new isolated build workspace.
    ///
    /// Creates a temporary directory with a consumer-provided namespace prefix
    /// for easier identification in `/tmp` or system temp directory.
    ///
    /// # Arguments
    ///
    /// * `namespace` - Prefix for the temp directory (e.g., "myapp", "backwpup-build")
    /// * `repo_url` - Repository URL to clone
    /// * `github_token` - Optional GitHub token for cloning private repositories
    ///
    /// # Directory Naming
    ///
    /// The temp directory will be named like:
    /// - `{namespace}-XXXXXX` (e.g., `myapp-a1b2c3`)
    ///
    /// The repository will be cloned inside as `{namespace}-XXXXXX/{repo_name}/`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let workspace = BuildWorkspace::new(
    ///     "backwpup-build",
    ///     "https://github.com/inpsyde/backwpup.git",
    ///     Some("ghp_token"),
    /// )?;
    /// // Creates: /tmp/backwpup-build-XXXXXX/backwpup/
    /// ```
    pub fn new(namespace: &str, repo_url: &str, github_token: Option<&str>) -> Result<Self> {
        let prefix = format!("{}-", namespace);

        let temp_dir = Builder::new()
            .prefix(&prefix)
            .tempdir()
            .map_err(|e: std::io::Error| Error::Io(e.into()))?;

        let repo_name = Self::repo_name_from_url(repo_url);

        tracing::debug!("Created build workspace: {}", temp_dir.path().display());

        Ok(Self {
            temp_dir,
            repo_url: repo_url.to_string(),
            repo_name,
            github_token: github_token.map(String::from),
            repository: OnceLock::new(),
        })
    }

    /// Clone the repository into this workspace.
    ///
    /// # Returns
    ///
    /// A [`Repository`] handle for the cloned repository. The repository
    /// is cached, so calling this multiple times returns the same handle.
    pub async fn clone_repo(&self) -> Result<&Repository> {
        if self.repository.get().is_some() {
            return Ok(self.repository.get().unwrap());
        }

        let repo_path = self.temp_dir.path().join(&self.repo_name);

        tracing::debug!("Cloning {} into {}", self.repo_url, repo_path.display());

        let repo = match &self.github_token {
            Some(token) if self.repo_url.starts_with("https://") => {
                Repository::clone_with_token(&self.repo_url, token, &repo_path).await?
            }
            _ => Repository::clone(&self.repo_url, &repo_path).await?,
        };

        // Store the repository (ignoring if another thread got there first)
        let _ = self.repository.set(repo);
        Ok(self.repository.get().unwrap())
    }

    /// Get the cloned repository handle.
    ///
    /// # Panics
    ///
    /// Panics if `clone_repo()` has not been called first.
    pub fn repository(&self) -> &Repository {
        self.repository
            .get()
            .expect("repository() called before clone_repo()")
    }

    /// Get the workspace root path.
    pub fn path(&self) -> &Path {
        self.temp_dir.path()
    }

    /// Get the path where the repository is/will be cloned.
    pub fn repo_path(&self) -> PathBuf {
        self.temp_dir.path().join(&self.repo_name)
    }

    /// Fetch updates for the repository using the workspace's token.
    pub async fn fetch(&self) -> Result<()> {
        let repo = self.repository();
        match &self.github_token {
            Some(token) => repo.fetch_with_token(token).await,
            None => repo.fetch().await,
        }
    }

    /// Pull latest changes using the workspace's token.
    pub async fn pull(&self) -> Result<()> {
        let repo = self.repository();
        match &self.github_token {
            Some(token) => repo.pull_with_token(token).await,
            None => repo.pull().await,
        }
    }

    /// Get the GitHub token if available.
    pub fn github_token(&self) -> Option<&str> {
        self.github_token.as_deref()
    }

    /// Collect artifacts from the workspace to an output directory.
    ///
    /// Moves files from the workspace to the specified output directory.
    /// This should be called before the workspace is dropped to preserve artifacts.
    ///
    /// # Arguments
    ///
    /// * `artifact_paths` - Paths to artifacts within the workspace
    /// * `output_dir` - Destination directory for artifacts
    ///
    /// # Returns
    ///
    /// Vector of paths to the collected artifacts in the output directory.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let workspace = BuildWorkspace::new("apvm", Some("backwpup"), None)?;
    /// // ... build produces artifacts ...
    ///
    /// let artifacts = vec![
    ///     workspace.path().join("backwpup-5.1.0.zip"),
    ///     workspace.path().join("backwpup-pro-5.1.0.zip"),
    /// ];
    ///
    /// let collected = workspace.collect_artifacts(&artifacts, "/output")?;
    /// // Artifacts are now in /output/, workspace will be cleaned up on drop
    /// ```
    pub fn collect_artifacts<P: AsRef<Path>>(
        &self,
        artifact_paths: &[PathBuf],
        output_dir: P,
    ) -> Result<Vec<PathBuf>> {
        let output_dir = output_dir.as_ref();
        fs::create_dir_all(output_dir)?;

        let mut collected = Vec::with_capacity(artifact_paths.len());

        for src in artifact_paths {
            if !src.exists() {
                tracing::warn!("Artifact not found, skipping: {}", src.display());
                continue;
            }

            let filename = src
                .file_name()
                .ok_or_else(|| Error::Build(format!("Invalid artifact path: {}", src.display())))?;

            let dest = output_dir.join(filename);

            // Try rename first (fast, same filesystem), fall back to copy+delete
            if fs::rename(src, &dest).is_err() {
                fs::copy(src, &dest)?;
                fs::remove_file(src)?;
            }

            tracing::debug!("Collected artifact: {}", dest.display());
            collected.push(dest);
        }

        Ok(collected)
    }

    /// Extract repository name from URL.
    ///
    /// Examples:
    /// - `https://github.com/org/repo.git` → `repo`
    /// - `https://github.com/org/repo` → `repo`
    /// - `git@github.com:org/repo.git` → `repo`
    fn repo_name_from_url(url: &str) -> String {
        url.trim_end_matches('/')
            .trim_end_matches(".git")
            .rsplit('/')
            .next()
            .unwrap_or("repo")
            .to_string()
    }
}

impl Drop for BuildWorkspace {
    fn drop(&mut self) {
        // TempDir already handles cleanup, but we log it for debugging
        tracing::debug!(
            "Cleaning up build workspace: {}",
            self.temp_dir.path().display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_creates_temp_dir() {
        let workspace =
            BuildWorkspace::new("test", "https://github.com/org/repo.git", None).unwrap();
        assert!(workspace.path().exists());
        assert!(workspace.path().is_dir());
    }

    #[test]
    fn workspace_with_namespace_prefix() {
        let workspace =
            BuildWorkspace::new("myapp-build", "https://github.com/org/repo.git", None).unwrap();
        let path_str = workspace.path().to_string_lossy();
        assert!(path_str.contains("myapp-build-"));
    }

    #[test]
    fn workspace_cleaned_on_drop() {
        let path = {
            let workspace =
                BuildWorkspace::new("test", "https://github.com/org/repo.git", None).unwrap();
            workspace.path().to_path_buf()
        };
        // After drop, temp dir should be gone
        assert!(!path.exists());
    }

    #[test]
    fn repo_name_from_url_https() {
        assert_eq!(
            BuildWorkspace::repo_name_from_url("https://github.com/org/repo.git"),
            "repo"
        );
    }

    #[test]
    fn repo_name_from_url_https_no_git() {
        assert_eq!(
            BuildWorkspace::repo_name_from_url("https://github.com/org/repo"),
            "repo"
        );
    }

    #[test]
    fn repo_name_from_url_trailing_slash() {
        assert_eq!(
            BuildWorkspace::repo_name_from_url("https://github.com/org/repo/"),
            "repo"
        );
    }

    #[test]
    fn collect_artifacts_moves_files() {
        let workspace =
            BuildWorkspace::new("test", "https://github.com/org/repo.git", None).unwrap();
        let output_dir = tempfile::tempdir().unwrap();

        // Create a test file in workspace
        let test_file = workspace.path().join("test-artifact.zip");
        fs::write(&test_file, b"test content").unwrap();

        // Collect it
        let collected = workspace
            .collect_artifacts(&[test_file.clone()], output_dir.path())
            .unwrap();

        // File should be in output, not in workspace
        assert_eq!(collected.len(), 1);
        assert!(collected[0].exists());
        assert!(!test_file.exists());
        assert_eq!(fs::read_to_string(&collected[0]).unwrap(), "test content");
    }
}
