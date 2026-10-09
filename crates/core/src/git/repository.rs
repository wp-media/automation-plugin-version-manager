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
            .args(["clone", url])
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
            .args(["clone", &authenticated_url])
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

    /// Pull with token authentication.
    ///
    /// Temporarily sets the remote URL with the token, pulls, then restores.
    pub async fn pull_with_token(&self, token: &str) -> Result<()> {
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

        // Pull
        let result = self.pull().await;

        // ALWAYS restore original URL (even on error)
        let restore_result = self.set_remote_url("origin", &original_url).await;

        // Return pull error first, then restore error
        result?;
        restore_result?;

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

    /// Get the full commit SHA of the current HEAD.
    ///
    /// Returns the commit hash as a hexadecimal string:
    /// - 40 characters for SHA-1 repositories (most common)
    /// - 64 characters for SHA-256 repositories (Git 2.29+)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let repo = Repository::open(&path)?;
    /// repo.checkout("main").await?;
    /// let commit = repo.get_head_commit().await?;
    /// // commit = "a1b2c3d4e5f6789012345678901234567890abcd"
    /// ```
    ///
    /// # References
    ///
    /// - [git-rev-parse(1)](https://git-scm.com/docs/git-rev-parse)
    pub async fn get_head_commit(&self) -> Result<String> {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to get HEAD commit: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("git rev-parse HEAD failed: {stderr}")));
        }

        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Validate the commit hash format (hex characters only)
        if !commit.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Git(format!("Invalid commit hash format: {commit}")));
        }

        // Validate length: SHA-1 (40) or SHA-256 (64)
        if commit.len() != 40 && commit.len() != 64 {
            return Err(Error::Git(format!(
                "Unexpected commit hash length {}: {commit}",
                commit.len()
            )));
        }

        Ok(commit)
    }

    /// Get the short commit SHA of the current HEAD.
    ///
    /// Returns the first 7 characters of the commit hash, which is the
    /// standard short form used by Git and GitHub.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let commit_short = repo.get_head_commit_short().await?;
    /// // commit_short = "a1b2c3d"
    /// ```
    pub async fn get_head_commit_short(&self) -> Result<String> {
        let full = self.get_head_commit().await?;
        Ok(full[..7].to_string())
    }

    /// Get both full and short commit SHA of the current HEAD.
    ///
    /// This is a convenience method that returns both formats in a single call,
    /// useful when you need both (e.g., for storage metadata).
    ///
    /// # Returns
    ///
    /// A tuple of `(full_commit, short_commit)`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (commit, commit_short) = repo.get_head_commit_pair().await?;
    /// // commit = "a1b2c3d4e5f6789012345678901234567890abcd"
    /// // commit_short = "a1b2c3d"
    /// ```
    pub async fn get_head_commit_pair(&self) -> Result<(String, String)> {
        let full = self.get_head_commit().await?;
        let short = full[..7].to_string();
        Ok((full, short))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that validates commit hash format checking logic.
    #[test]
    fn test_commit_hash_validation_logic() {
        // Valid SHA-1 (40 hex chars)
        let sha1 = "a1b2c3d4e5f6789012345678901234567890abcd";
        assert_eq!(sha1.len(), 40);
        assert!(sha1.chars().all(|c| c.is_ascii_hexdigit()));

        // Valid SHA-256 (64 hex chars)
        let sha256 = "a1b2c3d4e5f6789012345678901234567890abcda1b2c3d4e5f6789012345678";
        assert_eq!(sha256.len(), 64);
        assert!(sha256.chars().all(|c| c.is_ascii_hexdigit()));

        // Invalid: contains non-hex character
        let invalid = "a1b2c3d4e5f6789012345678901234567890abcg"; // 'g' is invalid
        assert!(!invalid.chars().all(|c| c.is_ascii_hexdigit()));

        // Invalid: wrong length
        let short = "a1b2c3d";
        assert!(short.len() != 40 && short.len() != 64);
    }

    /// Test short commit extraction.
    #[test]
    fn test_short_commit_extraction() {
        let full = "a1b2c3d4e5f6789012345678901234567890abcd";
        let short = &full[..7];
        assert_eq!(short, "a1b2c3d");
        assert_eq!(short.len(), 7);
    }

    /// Integration test that runs against the actual workspace repository.
    /// This test only runs if we're in a git repository.
    #[tokio::test]
    async fn test_get_head_commit_in_real_repo() {
        // Get the workspace root (where .git exists)
        let workspace = std::env::current_dir().expect("Failed to get current dir");

        // Skip if not in a git repo
        if !workspace.join(".git").exists() {
            eprintln!("Skipping test: not in a git repository");
            return;
        }

        let repo = Repository::open(&workspace).expect("Failed to open repo");

        // Test get_head_commit
        let commit = repo
            .get_head_commit()
            .await
            .expect("Failed to get HEAD commit");
        assert_eq!(commit.len(), 40, "SHA-1 commit should be 40 chars");
        assert!(
            commit.chars().all(|c| c.is_ascii_hexdigit()),
            "Commit should be hex only"
        );

        // Test get_head_commit_short
        let short = repo
            .get_head_commit_short()
            .await
            .expect("Failed to get short commit");
        assert_eq!(short.len(), 7, "Short commit should be 7 chars");
        assert_eq!(&commit[..7], short, "Short should match first 7 of full");

        // Test get_head_commit_pair
        let (full, short2) = repo
            .get_head_commit_pair()
            .await
            .expect("Failed to get commit pair");
        assert_eq!(full, commit);
        assert_eq!(short2, short);
    }

    // =========================================================================
    // Live behavior against local repositories (file:// — no network)
    // =========================================================================

    use crate::git::testutil::{commit_change, file_url, git, git_stdout, make_remote_repo};
    use tempfile::TempDir;

    /// Token used by the auth-plumbing tests; distinctive so a leak is
    /// unmistakable in an assertion message.
    const TOKEN: &str = "ghp_TESTTOKEN_must_never_leak";

    /// An HTTPS URL nothing listens on: loopback port 1 is refused
    /// immediately, so token-auth paths fail fast without leaving the host.
    const DEAD_HTTPS_URL: &str = "https://127.0.0.1:1/org/repo.git";

    /// Clone `origin` (via `file://`) into a fresh temp dir. Returns the
    /// holder (keeps the clone alive) and the opened [`Repository`].
    async fn clone_of(origin: &TempDir) -> (TempDir, Repository) {
        let holder = TempDir::new().unwrap();
        let repo = Repository::clone(&file_url(origin), &holder.path().join("clone"))
            .await
            .expect("local clone must succeed");
        (holder, repo)
    }

    /// Point `origin` of `repo` at `url` (simulates an HTTPS-cloned repo).
    fn set_origin(repo: &Repository, url: &str) {
        git(
            repo.path(),
            &["remote", "set-url", "origin", url],
            "2025-01-01T00:00:00",
        );
    }

    #[test]
    fn open_rejects_directory_without_git_metadata() {
        let dir = TempDir::new().unwrap();
        let err = Repository::open(dir.path()).err().expect("must fail");
        assert!(
            matches!(&err, Error::RepositoryNotFound(p) if p == dir.path()),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn open_accepts_initialized_repository() {
        let origin = make_remote_repo();
        let repo = Repository::open(origin.path()).expect("must open");
        assert_eq!(repo.path(), origin.path());
    }

    #[tokio::test]
    async fn clone_checks_out_default_branch_at_remote_tip() {
        let origin = make_remote_repo();
        let (_holder, repo) = clone_of(&origin).await;

        assert_eq!(repo.current_branch().await.unwrap(), "main");
        let (full, short) = repo.get_head_commit_pair().await.unwrap();
        assert_eq!(full, git_stdout(origin.path(), &["rev-parse", "main"]));
        assert_eq!(full.len(), 40);
        assert_eq!(short, &full[..7]);
        assert_eq!(repo.get_head_commit().await.unwrap(), full);
        assert_eq!(repo.get_head_commit_short().await.unwrap(), short);
    }

    #[tokio::test]
    async fn clone_of_missing_source_fails_with_git_error() {
        let missing = TempDir::new().unwrap();
        let url = format!("file://{}/nope", missing.path().display());
        let dest = missing.path().join("dest");

        let err = Repository::clone(&url, &dest)
            .await
            .err()
            .expect("must fail");
        assert!(err.to_string().contains("git clone failed"), "{err}");
    }

    #[tokio::test]
    async fn checkout_moves_head_to_tags_commits_and_branches() {
        let origin = make_remote_repo();
        let (_holder, repo) = clone_of(&origin).await;

        // Annotated tag → detached HEAD on the *tagged commit*.
        repo.checkout("v2.0.0-beta1").await.unwrap();
        assert_eq!(repo.current_branch().await.unwrap(), "HEAD");
        assert_eq!(
            repo.get_head_commit().await.unwrap(),
            git_stdout(origin.path(), &["rev-parse", "v2.0.0-beta1^{commit}"])
        );

        // Full commit SHA → detached HEAD on exactly that commit.
        let c1 = git_stdout(origin.path(), &["rev-parse", "v1.0.0"]);
        repo.checkout(&c1).await.unwrap();
        assert_eq!(repo.get_head_commit().await.unwrap(), c1);

        // Remote-only branch → DWIM creates a tracking branch (what builds
        // rely on for PR head branches that exist only on origin).
        repo.checkout("feature/x").await.unwrap();
        assert_eq!(repo.current_branch().await.unwrap(), "feature/x");
    }

    #[tokio::test]
    async fn checkout_of_unknown_ref_fails_with_git_error() {
        let origin = make_remote_repo();
        let (_holder, repo) = clone_of(&origin).await;

        let err = repo.checkout("no-such-ref").await.unwrap_err();
        assert!(err.to_string().contains("git checkout failed"), "{err}");
    }

    #[tokio::test]
    async fn fetch_and_pull_bring_in_new_remote_commits() {
        let origin = make_remote_repo();
        let (_holder, repo) = clone_of(&origin).await;
        let new_tip = commit_change(origin.path(), "four", "2025-02-01T10:00:00");

        repo.fetch().await.unwrap();
        // Fetch updates the remote-tracking ref but not the checkout...
        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", "origin/main"]),
            new_tip
        );
        assert_ne!(repo.get_head_commit().await.unwrap(), new_tip);

        // ...pull fast-forwards the checkout.
        repo.pull().await.unwrap();
        assert_eq!(repo.get_head_commit().await.unwrap(), new_tip);
    }

    #[tokio::test]
    async fn pull_refuses_non_fast_forward() {
        // `--ff-only` must never create a merge commit in a build workspace:
        // diverged histories fail instead.
        let origin = make_remote_repo();
        let (_holder, repo) = clone_of(&origin).await;
        commit_change(origin.path(), "remote side", "2025-02-01T10:00:00");
        commit_change(repo.path(), "local side", "2025-02-01T11:00:00");

        let err = repo.pull().await.unwrap_err();
        assert!(err.to_string().contains("git pull failed"), "{err}");
    }

    #[tokio::test]
    async fn fetch_without_reachable_remote_fails_with_git_error() {
        let origin = make_remote_repo();
        let (_holder, repo) = clone_of(&origin).await;
        set_origin(&repo, "file:///definitely/not/a/repo");

        let err = repo.fetch().await.unwrap_err();
        assert!(err.to_string().contains("git fetch failed"), "{err}");
    }

    #[tokio::test]
    async fn reset_hard_discards_changes_untracked_and_ignored_files() {
        let origin = make_remote_repo();
        let (_holder, repo) = clone_of(&origin).await;
        let path = repo.path();
        std::fs::write(path.join(".gitignore"), "ignored.log\n").unwrap();
        std::fs::write(path.join("f"), "dirty").unwrap();
        std::fs::create_dir(path.join("untracked-dir")).unwrap();
        std::fs::write(path.join("untracked-dir/x"), "x").unwrap();
        std::fs::write(path.join("ignored.log"), "build output").unwrap();

        repo.reset_hard().await.unwrap();

        // Tracked edits reverted; untracked AND ignored files (`-x`) gone, so
        // a reused workspace can never leak artifacts into the next build.
        assert_eq!(std::fs::read_to_string(path.join("f")).unwrap(), "three");
        assert!(!path.join(".gitignore").exists());
        assert!(!path.join("untracked-dir").exists());
        assert!(!path.join("ignored.log").exists());
    }

    #[tokio::test]
    async fn head_commit_of_repository_without_commits_is_an_error() {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "--quiet"], "2025-01-01T00:00:00");
        let repo = Repository::open(dir.path()).unwrap();

        let err = repo.get_head_commit().await.unwrap_err();
        assert!(err.to_string().contains("rev-parse HEAD failed"), "{err}");
        // The short/pair variants must propagate, never slice an empty SHA.
        assert!(repo.get_head_commit_short().await.is_err());
        assert!(repo.get_head_commit_pair().await.is_err());
    }

    #[tokio::test]
    async fn head_commit_supports_sha256_repositories() {
        let dir = TempDir::new().unwrap();
        git(
            dir.path(),
            &["init", "--quiet", "--object-format=sha256"],
            "2025-01-01T00:00:00",
        );
        commit_change(dir.path(), "one", "2025-01-01T00:00:00");
        let repo = Repository::open(dir.path()).unwrap();

        let (full, short) = repo.get_head_commit_pair().await.unwrap();
        assert_eq!(full.len(), 64);
        assert_eq!(short, &full[..7]);
    }

    // -------------------------------------------------------------------------
    // Token authentication plumbing
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn clone_with_token_rejects_non_https_urls_before_running_git() {
        let dir = TempDir::new().unwrap();
        for url in [
            "git@github.com:org/repo.git",
            "file:///tmp/repo",
            "http://h/r",
        ] {
            let dest = dir.path().join("dest");
            let err = Repository::clone_with_token(url, TOKEN, &dest)
                .await
                .err()
                .expect("must fail");
            assert!(err.to_string().contains("requires HTTPS"), "{url}: {err}");
            assert!(!dest.exists(), "{url}: nothing may be cloned");
        }
    }

    #[tokio::test]
    async fn clone_with_token_failure_never_leaks_the_token() {
        let dir = TempDir::new().unwrap();
        let err = Repository::clone_with_token(DEAD_HTTPS_URL, TOKEN, &dir.path().join("d"))
            .await
            .err()
            .expect("must fail");
        let msg = err.to_string();
        assert!(msg.contains("git clone failed"), "{msg}");
        assert!(!msg.contains(TOKEN), "token leaked: {msg}");
    }

    #[tokio::test]
    async fn token_fetch_and_pull_reject_non_https_origin_untouched() {
        let origin = make_remote_repo();
        let (_holder, repo) = clone_of(&origin).await;
        let url_before = repo.get_remote_url("origin").await.unwrap();

        let err = repo.fetch_with_token(TOKEN).await.unwrap_err();
        assert!(err.to_string().contains("requires HTTPS"), "{err}");
        let err = repo.pull_with_token(TOKEN).await.unwrap_err();
        assert!(err.to_string().contains("requires HTTPS"), "{err}");

        assert_eq!(repo.get_remote_url("origin").await.unwrap(), url_before);
    }

    #[tokio::test]
    async fn token_fetch_and_pull_restore_clean_url_even_on_failure() {
        // SECURITY: the token-bearing URL is only temporary — a failed
        // network operation must not leave it persisted in .git/config.
        let origin = make_remote_repo();
        let (_holder, repo) = clone_of(&origin).await;
        set_origin(&repo, DEAD_HTTPS_URL);

        let err = repo.fetch_with_token(TOKEN).await.unwrap_err();
        assert!(err.to_string().contains("git fetch failed"), "{err}");
        assert!(!err.to_string().contains(TOKEN), "token leaked: {err}");
        assert_eq!(repo.get_remote_url("origin").await.unwrap(), DEAD_HTTPS_URL);

        let err = repo.pull_with_token(TOKEN).await.unwrap_err();
        assert!(err.to_string().contains("git pull failed"), "{err}");
        assert!(!err.to_string().contains(TOKEN), "token leaked: {err}");
        assert_eq!(repo.get_remote_url("origin").await.unwrap(), DEAD_HTTPS_URL);
    }

    // Unix-only: on Windows `file_url` embeds a backslash path, which would
    // become the `url.<base>.insteadOf` config subsection — untested there.
    #[cfg(unix)]
    #[tokio::test]
    async fn token_fetch_and_pull_authenticate_then_restore_clean_url() {
        // Route the exact token-bearing URL to the local origin with a
        // repo-local `insteadOf`, so the success path runs hermetically.
        let origin = make_remote_repo();
        let (_holder, repo) = clone_of(&origin).await;
        let clean_url = "https://example.invalid/org/repo.git";
        let auth_url = format!("https://x-access-token:{TOKEN}@example.invalid/org/repo.git");
        set_origin(&repo, clean_url);
        git(
            repo.path(),
            &[
                "config",
                &format!("url.{}.insteadOf", file_url(&origin)),
                &auth_url,
            ],
            "2025-01-01T00:00:00",
        );
        let new_tip = commit_change(origin.path(), "four", "2025-02-01T10:00:00");

        repo.fetch_with_token(TOKEN).await.unwrap();
        assert_eq!(repo.get_remote_url("origin").await.unwrap(), clean_url);
        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", "origin/main"]),
            new_tip
        );

        repo.pull_with_token(TOKEN).await.unwrap();
        assert_eq!(repo.get_remote_url("origin").await.unwrap(), clean_url);
        assert_eq!(repo.get_head_commit().await.unwrap(), new_tip);

        let config = std::fs::read_to_string(repo.path().join(".git/config")).unwrap();
        assert!(
            !config.contains(&format!("url = {auth_url}")),
            "token-bearing remote URL persisted"
        );
    }
}
