//! Git repository operations.
//!
//! Uses the system `git` command for maximum compatibility with existing
//! authentication methods (SSH keys, credential helpers, etc.).

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::error::{Error, Result};

/// A Git repository wrapper.
///
/// This uses the system `git` CLI for operations, which provides:
/// - Automatic SSH key usage
/// - Credential helper integration (macOS Keychain, etc.)
/// - GitHub CLI authentication support
/// - Maximum compatibility across environments
pub struct Repository {
    path: PathBuf,
}

impl Repository {
    /// Open an existing repository at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        if !path.join(".git").exists() {
            return Err(Error::RepositoryNotFound(path.to_path_buf()));
        }

        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Clone a repository from a URL to the given path.
    ///
    /// This uses the system `git` command, which automatically handles:
    /// - SSH keys (`~/.ssh/id_rsa`, `id_ed25519`, etc.)
    /// - Git credential helpers (macOS Keychain, Windows Credential Manager)
    /// - GitHub CLI authentication (`gh auth`)
    /// - Environment variables (`GIT_SSH_COMMAND`, etc.)
    ///
    /// # Arguments
    ///
    /// * `url` - Repository URL (HTTPS or SSH)
    /// * `path` - Local path to clone into
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Public repo - no auth needed
    /// Repository::clone("https://github.com/rust-lang/rust.git", &path).await?;
    ///
    /// // Private repo with SSH key configured
    /// Repository::clone("git@github.com:org/private-repo.git", &path).await?;
    /// ```
    pub async fn clone(url: &str, path: &Path) -> Result<Self> {
        tracing::info!("Cloning {} into {}", url, path.display());

        let output = Command::new("git")
            .args(["clone", "--single-branch", url])
            .arg(path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to execute git: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("git clone failed: {stderr}")));
        }

        tracing::debug!("Clone completed successfully");

        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Clone a repository with explicit token authentication.
    ///
    /// Use this for private repositories when you have a GitHub token.
    /// The token is embedded in the URL only during clone, then removed
    /// from the remote configuration for security.
    ///
    /// # Arguments
    ///
    /// * `url` - Repository HTTPS URL (must start with `https://`)
    /// * `token` - GitHub Personal Access Token or App token
    /// * `path` - Local path to clone into
    ///
    /// # Security
    ///
    /// - Token is NOT stored in the cloned repository
    /// - Token is NOT logged in error messages
    /// - Remote URL is reset to token-free URL after clone
    ///
    /// # Example
    ///
    /// ```ignore
    /// let token = std::env::var("GITHUB_TOKEN")?;
    /// Repository::clone_with_token(
    ///     "https://github.com/org/private-repo.git",
    ///     &token,
    ///     &path
    /// ).await?;
    /// ```
    pub async fn clone_with_token(url: &str, token: &str, path: &Path) -> Result<Self> {
        // Validate URL format
        if !url.starts_with("https://") {
            return Err(Error::Git(
                "Token authentication requires HTTPS URL (not SSH)".to_string(),
            ));
        }

        tracing::info!("Cloning {} into {} (with token auth)", url, path.display());

        // Embed token in URL: https://github.com/... → https://x-access-token:{token}@github.com/...
        let authenticated_url =
            url.replacen("https://", &format!("https://x-access-token:{token}@"), 1);

        let output = Command::new("git")
            .args(["clone", "--single-branch", &authenticated_url])
            .arg(path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to execute git: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // SECURITY: Never expose the token in error messages
            let safe_stderr = stderr.replace(token, "[REDACTED]");
            return Err(Error::Git(format!("git clone failed: {safe_stderr}")));
        }

        let repo = Self {
            path: path.to_path_buf(),
        };

        // SECURITY: Remove token from stored remote URL
        repo.set_remote_url("origin", url).await?;

        tracing::debug!("Clone completed successfully (token removed from remote)");

        Ok(repo)
    }

    /// Get the repository path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Fetch updates from all remotes.
    ///
    /// This fetches all branches and prunes deleted remote branches.
    pub async fn fetch(&self) -> Result<()> {
        tracing::debug!("Fetching updates in {}", self.path.display());

        let output = Command::new("git")
            .args(["fetch", "--all", "--prune"])
            .current_dir(&self.path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to execute git fetch: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("git fetch failed: {stderr}")));
        }

        Ok(())
    }

    /// Fetch with token authentication.
    ///
    /// Temporarily sets the remote URL with the token, fetches, then restores.
    pub async fn fetch_with_token(&self, token: &str) -> Result<()> {
        // Get current remote URL
        let original_url = self.get_remote_url("origin").await?;

        if !original_url.starts_with("https://") {
            return Err(Error::Git(
                "Token authentication requires HTTPS remote URL".to_string(),
            ));
        }

        // Temporarily set authenticated URL
        let auth_url =
            original_url.replacen("https://", &format!("https://x-access-token:{token}@"), 1);
        self.set_remote_url("origin", &auth_url).await?;

        // Fetch
        let result = self.fetch().await;

        // ALWAYS restore original URL (even on error)
        let restore_result = self.set_remote_url("origin", &original_url).await;

        // Return fetch error first, then restore error
        result?;
        restore_result?;

        Ok(())
    }

    /// Checkout a specific ref (branch, tag, or commit).
    pub async fn checkout(&self, reference: &str) -> Result<()> {
        tracing::debug!("Checking out '{}' in {}", reference, self.path.display());

        let output = Command::new("git")
            .args(["checkout", reference])
            .current_dir(&self.path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to execute git checkout: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("git checkout failed: {stderr}")));
        }

        Ok(())
    }

    /// Get the current branch name.
    pub async fn current_branch(&self) -> Result<String> {
        let output = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&self.path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to get current branch: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("git rev-parse failed: {stderr}")));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Pull latest changes from remote.
    pub async fn pull(&self) -> Result<()> {
        tracing::debug!("Pulling latest changes in {}", self.path.display());

        let output = Command::new("git")
            .args(["pull", "--ff-only"])
            .current_dir(&self.path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to execute git pull: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("git pull failed: {stderr}")));
        }

        Ok(())
    }

    /// Reset the working directory to a clean state.
    pub async fn reset_hard(&self) -> Result<()> {
        tracing::debug!("Resetting repository in {}", self.path.display());

        let output = Command::new("git")
            .args(["reset", "--hard", "HEAD"])
            .current_dir(&self.path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to execute git reset: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("git reset failed: {stderr}")));
        }

        // Also clean untracked files
        let output = Command::new("git")
            .args(["clean", "-fdx"])
            .current_dir(&self.path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to execute git clean: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("git clean failed: {stderr}")));
        }

        Ok(())
    }

    /// Set the URL for a remote.
    async fn set_remote_url(&self, remote: &str, url: &str) -> Result<()> {
        let output = Command::new("git")
            .args(["remote", "set-url", remote, url])
            .current_dir(&self.path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to set remote URL: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("git remote set-url failed: {stderr}")));
        }

        Ok(())
    }

    /// Get the URL for a remote.
    async fn get_remote_url(&self, remote: &str) -> Result<String> {
        let output = Command::new("git")
            .args(["remote", "get-url", remote])
            .current_dir(&self.path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to get remote URL: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("git remote get-url failed: {stderr}")));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}
