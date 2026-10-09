//! Build workspace management using temporary directories.
//!
//! Provides isolated workspaces for builds using temp directories that are
//! automatically cleaned up when the workspace is dropped.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::{Builder, TempDir};
use tokio::sync::OnceCell;

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
    /// Cached repository handle after cloning. An async cell, so concurrent
    /// [`clone_repo`](Self::clone_repo) calls wait for one clone instead of
    /// racing two `git clone`s into the same directory.
    repository: OnceCell<Repository>,
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
            .map_err(|e: std::io::Error| Error::Io(e))?;

        let repo_name = Self::repo_name_from_url(repo_url);

        tracing::debug!("Created build workspace: {}", temp_dir.path().display());

        Ok(Self {
            temp_dir,
            repo_url: repo_url.to_string(),
            repo_name,
            github_token: github_token.map(String::from),
            repository: OnceCell::new(),
        })
    }

    /// Clone the repository into this workspace.
    ///
    /// The workspace's token authenticates the clone only for an `https://`
    /// repository URL; other transports (SSH, `file://`) use the system git's
    /// own authentication. [`fetch`](Self::fetch) and [`pull`](Self::pull)
    /// follow the same rule.
    ///
    /// # Returns
    ///
    /// A [`Repository`] handle for the cloned repository. The repository
    /// is cached, so calling this multiple times — even concurrently —
    /// clones once and returns the same handle. A failed clone is not
    /// cached: the next call tries again.
    ///
    /// # Errors
    ///
    /// [`Error::Git`] when the clone fails.
    pub async fn clone_repo(&self) -> Result<&Repository> {
        self.repository.get_or_try_init(|| self.clone_now()).await
    }

    /// Clone the repository into [`repo_path`](Self::repo_path), with the
    /// token only for an HTTPS URL ([`clone_repo`](Self::clone_repo) caches
    /// the result).
    ///
    /// # Errors
    ///
    /// [`Error::Git`] when the clone fails.
    async fn clone_now(&self) -> Result<Repository> {
        let repo_path = self.repo_path();

        tracing::debug!("Cloning {} into {}", self.repo_url, repo_path.display());

        match self.https_token() {
            Some(token) => Repository::clone_with_token(&self.repo_url, token, &repo_path).await,
            None => Repository::clone(&self.repo_url, &repo_path).await,
        }
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

    /// Create a [`crate::build::BuildContext`] from this workspace.
    ///
    /// Maps the workspace's directory structure to the build context:
    /// - `workspace_dir` → the temp directory root ([`path()`](Self::path))
    /// - `repo_dir` → the cloned repository inside it ([`repo_path()`](Self::repo_path))
    ///
    /// # Example
    ///
    /// ```ignore
    /// let workspace = BuildWorkspace::new("wp-rocket", "https://...", None)?;
    /// workspace.clone_repo().await?;
    ///
    /// let context = workspace.to_build_context();
    /// // context.workspace_dir() == /tmp/wp-rocket-XXXXXX/
    /// // context.repo_dir()      == /tmp/wp-rocket-XXXXXX/wp-rocket/
    /// ```
    pub fn to_build_context(&self) -> crate::build::BuildContext {
        crate::build::BuildContext::new(self.repo_path(), self.temp_dir.path().to_path_buf())
    }

    /// Fetch updates for the repository. The workspace's token is used only
    /// for an `https://` repository URL, exactly as in
    /// [`clone_repo`](Self::clone_repo).
    pub async fn fetch(&self) -> Result<()> {
        let repo = self.repository();
        match self.https_token() {
            Some(token) => repo.fetch_with_token(token).await,
            None => repo.fetch().await,
        }
    }

    /// Pull latest changes. The workspace's token is used only for an
    /// `https://` repository URL, exactly as in [`clone_repo`](Self::clone_repo).
    pub async fn pull(&self) -> Result<()> {
        let repo = self.repository();
        match self.https_token() {
            Some(token) => repo.pull_with_token(token).await,
            None => repo.pull().await,
        }
    }

    /// Get the GitHub token if available.
    pub fn github_token(&self) -> Option<&str> {
        self.github_token.as_deref()
    }

    /// The token to authenticate git operations with: only for an `https://`
    /// repository URL. Other transports (SSH, `file://`) use the system git's
    /// own authentication, so clone, fetch and pull all follow one rule.
    ///
    /// # Returns
    ///
    /// The token, or `None` without one or for a non-HTTPS URL.
    fn https_token(&self) -> Option<&str> {
        self.github_token
            .as_deref()
            .filter(|_| self.repo_url.starts_with("https://"))
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

            // Try rename first (fast, same filesystem).
            // If it fails (e.g., cross-device), fall back to:
            // 1) copy to a temp file in destination dir
            // 2) atomically rename temp -> final destination
            // 3) remove source
            if fs::rename(src, &dest).is_err() {
                let parent = dest.parent().ok_or_else(|| {
                    Error::Build(format!("Invalid destination path: {}", dest.display()))
                })?;

                let tmp = tempfile::Builder::new()
                    .prefix(".apvm-artifact-")
                    .suffix(".tmp")
                    .tempfile_in(parent)
                    .map_err(Error::Io)?;

                let tmp_path = tmp.path().to_path_buf();

                fs::copy(src, &tmp_path)?;
                tmp.as_file().sync_all()?;

                fs::rename(&tmp_path, &dest)?;
                let _ = fs::remove_file(src);
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
            .collect_artifacts(std::slice::from_ref(&test_file), output_dir.path())
            .unwrap();

        // File should be in output, not in workspace
        assert_eq!(collected.len(), 1);
        assert!(collected[0].exists());
        assert!(!test_file.exists());
        assert_eq!(fs::read_to_string(&collected[0]).unwrap(), "test content");
    }

    #[test]
    fn repo_name_from_url_scp_style_ssh() {
        assert_eq!(
            BuildWorkspace::repo_name_from_url("git@github.com:org/repo.git"),
            "repo"
        );
    }

    #[test]
    fn workspace_exposes_token_and_repo_path() {
        let workspace = BuildWorkspace::new(
            "test",
            "https://github.com/org/my-plugin.git",
            Some("ghp_x"),
        )
        .unwrap();
        assert_eq!(workspace.github_token(), Some("ghp_x"));
        assert_eq!(workspace.repo_path(), workspace.path().join("my-plugin"));

        let anonymous =
            BuildWorkspace::new("test", "https://github.com/org/repo.git", None).unwrap();
        assert_eq!(anonymous.github_token(), None);
    }

    #[test]
    fn to_build_context_maps_workspace_and_repo_dirs() {
        let workspace =
            BuildWorkspace::new("test", "https://github.com/org/my-plugin.git", None).unwrap();
        let context = workspace.to_build_context();
        assert_eq!(context.workspace_dir(), workspace.path());
        assert_eq!(context.repo_dir(), workspace.repo_path());
    }

    #[test]
    #[should_panic(expected = "repository() called before clone_repo()")]
    fn repository_before_clone_panics() {
        let workspace =
            BuildWorkspace::new("test", "https://github.com/org/repo.git", None).unwrap();
        let _ = workspace.repository();
    }

    #[test]
    fn collect_artifacts_skips_missing_and_creates_output_dir() {
        let workspace =
            BuildWorkspace::new("test", "https://github.com/org/repo.git", None).unwrap();
        let out = tempfile::tempdir().unwrap();
        let output_dir = out.path().join("nested/out");
        let present = workspace.path().join("present.zip");
        fs::write(&present, b"ok").unwrap();
        let missing = workspace.path().join("missing.zip");

        let collected = workspace
            .collect_artifacts(&[missing, present], &output_dir)
            .unwrap();

        // A missing artifact is skipped (warned), not fatal for the rest.
        assert_eq!(collected, vec![output_dir.join("present.zip")]);
        assert_eq!(fs::read(&collected[0]).unwrap(), b"ok");
    }

    #[test]
    fn collect_artifacts_replaces_stale_output_file() {
        let workspace =
            BuildWorkspace::new("test", "https://github.com/org/repo.git", None).unwrap();
        let out = tempfile::tempdir().unwrap();
        fs::write(out.path().join("a.zip"), b"stale").unwrap();
        let src = workspace.path().join("a.zip");
        fs::write(&src, b"fresh").unwrap();

        workspace
            .collect_artifacts(std::slice::from_ref(&src), out.path())
            .unwrap();

        assert_eq!(fs::read(out.path().join("a.zip")).unwrap(), b"fresh");
    }

    #[test]
    fn collect_artifacts_rejects_path_without_file_name() {
        let workspace =
            BuildWorkspace::new("test", "https://github.com/org/repo.git", None).unwrap();
        let out = tempfile::tempdir().unwrap();
        // Exists on disk but has no final file-name component.
        let bogus = workspace.path().join("..");

        let err = workspace
            .collect_artifacts(&[bogus], out.path())
            .unwrap_err();
        assert!(err.to_string().contains("Invalid artifact path"), "{err}");
    }

    #[test]
    fn collect_artifacts_failed_fallback_keeps_source_and_no_temp_files() {
        // A directory squatting on the destination name makes the fast
        // rename AND the copy-then-rename fallback fail. The artifact must
        // survive in the workspace and no `.apvm-artifact-*.tmp` may linger.
        let workspace =
            BuildWorkspace::new("test", "https://github.com/org/repo.git", None).unwrap();
        let out = tempfile::tempdir().unwrap();
        fs::create_dir(out.path().join("a.zip")).unwrap();
        fs::write(out.path().join("a.zip/occupant"), b"x").unwrap();
        let src = workspace.path().join("a.zip");
        fs::write(&src, b"artifact").unwrap();

        assert!(
            workspace
                .collect_artifacts(std::slice::from_ref(&src), out.path())
                .is_err()
        );

        assert_eq!(fs::read(&src).unwrap(), b"artifact");
        let leftovers: Vec<_> = fs::read_dir(out.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".apvm-artifact-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    // =========================================================================
    // Clone / fetch / pull against a local origin (file:// — no network)
    //
    // Unix-only: the clone directory is named after the URL's last `/`
    // segment, and a Windows `file://C:\…` URL has none — the derived name
    // would be the whole origin path. Real remotes are `https://` URLs.
    // =========================================================================

    #[cfg(unix)]
    use crate::git::testutil::{commit_change, file_url, git_stdout, make_remote_repo};

    #[cfg(unix)]
    #[tokio::test]
    async fn clone_repo_clones_once_and_caches_the_handle() {
        let origin = make_remote_repo();
        let workspace = BuildWorkspace::new("test", &file_url(&origin), None).unwrap();

        let first = workspace.clone_repo().await.unwrap().path().to_path_buf();
        // Clone dir is named after the URL's last path segment.
        assert_eq!(first, workspace.repo_path());
        assert!(first.join(".git").exists());

        // A second call must reuse the clone (a re-clone into the existing
        // non-empty dir would fail).
        let second = workspace.clone_repo().await.unwrap().path().to_path_buf();
        assert_eq!(first, second);
        assert_eq!(workspace.repository().path(), first);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_clone_repo_calls_share_one_clone() {
        // Two racing callers used to run two `git clone`s into the same
        // directory; the second failed with "already exists".
        let origin = make_remote_repo();
        let workspace = BuildWorkspace::new("test", &file_url(&origin), None).unwrap();

        let (first, second) = tokio::join!(workspace.clone_repo(), workspace.clone_repo());

        let (first, second) = (first.unwrap(), second.unwrap());
        assert!(
            std::ptr::eq(first, second),
            "both callers get the one handle"
        );
        assert_eq!(first.path(), workspace.repo_path());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clone_repo_with_token_on_non_https_url_uses_plain_clone() {
        // Tokens only apply to HTTPS; other transports (SSH, file) must
        // clone with the system git's own auth instead of failing.
        let origin = make_remote_repo();
        let workspace = BuildWorkspace::new("test", &file_url(&origin), Some("ghp_x")).unwrap();

        let repo = workspace.clone_repo().await.unwrap();
        assert_eq!(
            git_stdout(repo.path(), &["remote", "get-url", "origin"]),
            file_url(&origin)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_and_pull_with_token_on_non_https_url_use_plain_git() {
        // Regression: clone ignored the token for a non-HTTPS URL, but
        // fetch/pull always took the token path and failed with "requires
        // HTTPS". All three must follow the same rule.
        let origin = make_remote_repo();
        let workspace = BuildWorkspace::new("test", &file_url(&origin), Some("ghp_x")).unwrap();
        workspace.clone_repo().await.unwrap();
        let new_tip = commit_change(origin.path(), "four", "2025-02-01T10:00:00");

        workspace.fetch().await.unwrap();
        workspace.pull().await.unwrap();

        assert_eq!(
            workspace.repository().get_head_commit().await.unwrap(),
            new_tip
        );
    }

    #[test]
    fn tokens_apply_only_to_https_urls() {
        let https = BuildWorkspace::new("test", "https://github.com/o/r.git", Some("t")).unwrap();
        assert_eq!(https.https_token(), Some("t"));
        for url in ["git@github.com:o/r.git", "file:///tmp/r", "http://h/r.git"] {
            let other = BuildWorkspace::new("test", url, Some("t")).unwrap();
            assert_eq!(other.https_token(), None, "{url}");
            // The token is still reported as configured.
            assert_eq!(other.github_token(), Some("t"), "{url}");
        }
        let anonymous = BuildWorkspace::new("test", "https://github.com/o/r.git", None).unwrap();
        assert_eq!(anonymous.https_token(), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_and_pull_without_token_update_the_clone() {
        let origin = make_remote_repo();
        let workspace = BuildWorkspace::new("test", &file_url(&origin), None).unwrap();
        workspace.clone_repo().await.unwrap();
        let new_tip = commit_change(origin.path(), "four", "2025-02-01T10:00:00");

        workspace.fetch().await.unwrap();
        workspace.pull().await.unwrap();

        assert_eq!(
            workspace.repository().get_head_commit().await.unwrap(),
            new_tip
        );
    }

    #[tokio::test]
    async fn clone_repo_failure_propagates() {
        let missing = tempfile::tempdir().unwrap();
        let url = format!("file://{}/nope", missing.path().display());
        let workspace = BuildWorkspace::new("test", &url, None).unwrap();

        let err = workspace.clone_repo().await.err().expect("must fail");
        assert!(err.to_string().contains("git clone failed"), "{err}");
    }
}
