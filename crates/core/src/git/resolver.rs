//! Git reference resolution.
//!
//! Automatically detects the type of a git reference (PR, branch, tag, commit)
//! from user input and resolves it to a concrete ref for checkout.
//!
//! # Resolution backends
//!
//! - **Local** ([`RefResolver::with_repo_path`]): validates refs against an
//!   existing clone (`git show-ref` / `rev-parse`).
//! - **Remote** ([`RefResolver::with_remote`]): validates refs **without any
//!   clone** — tags/branches via one cached `git ls-remote` round-trip,
//!   commits via the GitHub commits API, `tag:` keywords via a minimal
//!   tags-only fetch. This is what lets a build pipeline reject a bad ref
//!   (and learn the commit SHA of a good one) before paying for a clone.
//! - **Neither**: last-resort trust-the-user mode (bare names are assumed
//!   to be branches, with a warning).
//!
//! Local wins when both are configured; the ordering of automatic detection
//! (PR → commit → tag → branch) is identical across backends.
//!
//! Supports special keywords for tags and releases:
//! - `tag:latest-stable` / `release:latest-stable` — latest non-prerelease
//! - `tag:previous-stable` / `release:previous-stable` — previous non-prerelease
//! - `tag:latest` / `release:latest` — very latest (including prereleases)
//! - `tag:previous-latest` / `release:previous-latest` — previous to the very latest

use std::path::Path;

use apvm_storage::BuildSource;
use tokio::process::Command;
use tokio::sync::OnceCell;

use crate::error::{Error, Result};
use crate::git::remote::{RemoteGit, RemoteRefs};
use crate::github::GitHubClient;

/// Resolved git reference with metadata.
#[derive(Debug, Clone)]
pub struct ResolvedRef {
    /// The original input from user.
    pub input: String,
    /// The detected source type.
    pub source: RefSource,
    /// The git ref to checkout (branch name, tag, or commit SHA).
    pub git_ref: String,
    /// Full commit SHA (if resolved).
    pub commit_sha: Option<String>,
}

impl ResolvedRef {
    /// Convert to a `BuildSource` for storage operations.
    pub fn into_build_source(self) -> BuildSource {
        self.source.into()
    }

    /// Get a reference to the source as `BuildSource`.
    pub fn to_build_source(&self) -> BuildSource {
        BuildSource::from(&self.source)
    }

    /// Human-readable description including the resolved git ref.
    ///
    /// For PRs this includes the resolved branch name so users can see
    /// exactly which branch backs the pull request they requested:
    ///
    /// - PR → `"PR #123 (branch: feature/my-feature)"`
    /// - Branch → `"branch 'develop'"`
    /// - Tag → `"tag 'v1.0.0'"`
    /// - Commit → `"commit a1b2c3d"`
    pub fn detailed_description(&self) -> String {
        match &self.source {
            RefSource::PullRequest(num) => {
                format!("PR #{num} (branch: {})", self.git_ref)
            }
            _ => self.source.description(),
        }
    }
}

/// Source type of a git reference.
///
/// Convertible to/from [`apvm_storage::BuildSource`] for storage operations.
///
/// # Examples
///
/// ```
/// use apvm_core::git::RefSource;
/// use apvm_storage::BuildSource;
///
/// // RefSource → BuildSource
/// let ref_source = RefSource::Branch("develop".to_string());
/// let build_source: BuildSource = ref_source.into();
///
/// // BuildSource → RefSource
/// let build_source = BuildSource::Tag("v1.0.0".to_string());
/// let ref_source: RefSource = build_source.into();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefSource {
    /// Pull request number.
    PullRequest(u64),
    /// Branch name.
    Branch(String),
    /// Tag name.
    Tag(String),
    /// Commit SHA.
    Commit(String),
    /// GitHub Release tag (pre-built assets).
    Release(String),
}

impl RefSource {
    /// Get a human-readable description.
    pub fn description(&self) -> String {
        match self {
            Self::PullRequest(num) => format!("PR #{num}"),
            Self::Tag(tag) => format!("tag '{tag}'"),
            Self::Branch(branch) => format!("branch '{branch}'"),
            // chars() (not byte slicing) so a non-hex value containing
            // multibyte characters can never panic on a char boundary.
            Self::Commit(sha) => {
                format!("commit {}", sha.chars().take(7).collect::<String>())
            }
            Self::Release(tag) => format!("release '{tag}'"),
        }
    }

    /// Convert to a `BuildSource` for storage operations.
    ///
    /// This is equivalent to `Into<BuildSource>` but more explicit.
    pub fn into_build_source(self) -> BuildSource {
        self.into()
    }

    /// Get as a `BuildSource` without consuming self.
    pub fn to_build_source(&self) -> BuildSource {
        BuildSource::from(self)
    }
}

impl std::fmt::Display for RefSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description())
    }
}

// ============================================================================
// Conversions: RefSource <-> BuildSource
// ============================================================================

/// Convert `RefSource` into `BuildSource`.
impl From<RefSource> for BuildSource {
    fn from(source: RefSource) -> Self {
        match source {
            RefSource::PullRequest(n) => BuildSource::PullRequest(n),
            RefSource::Branch(s) => BuildSource::Branch(s),
            RefSource::Tag(s) => BuildSource::Tag(s),
            RefSource::Commit(s) => BuildSource::Commit(s),
            RefSource::Release(s) => BuildSource::Release(s),
        }
    }
}

/// Convert `&RefSource` into `BuildSource`.
impl From<&RefSource> for BuildSource {
    fn from(source: &RefSource) -> Self {
        match source {
            RefSource::PullRequest(n) => BuildSource::PullRequest(*n),
            RefSource::Branch(s) => BuildSource::Branch(s.clone()),
            RefSource::Tag(s) => BuildSource::Tag(s.clone()),
            RefSource::Commit(s) => BuildSource::Commit(s.clone()),
            RefSource::Release(s) => BuildSource::Release(s.clone()),
        }
    }
}

/// Convert `BuildSource` into `RefSource`.
impl From<BuildSource> for RefSource {
    fn from(source: BuildSource) -> Self {
        match source {
            BuildSource::PullRequest(n) => RefSource::PullRequest(n),
            BuildSource::Branch(s) => RefSource::Branch(s),
            BuildSource::Tag(s) => RefSource::Tag(s),
            BuildSource::Commit(s) => RefSource::Commit(s),
            BuildSource::Release(s) => RefSource::Release(s),
        }
    }
}

/// Convert `&BuildSource` into `RefSource`.
impl From<&BuildSource> for RefSource {
    fn from(source: &BuildSource) -> Self {
        match source {
            BuildSource::PullRequest(n) => RefSource::PullRequest(*n),
            BuildSource::Branch(s) => RefSource::Branch(s.clone()),
            BuildSource::Tag(s) => RefSource::Tag(s.clone()),
            BuildSource::Commit(s) => RefSource::Commit(s.clone()),
            BuildSource::Release(s) => RefSource::Release(s.clone()),
        }
    }
}

/// Resolves git references from user input.
///
/// Supports automatic detection and explicit prefix syntax:
/// - `123` → PR #123 (if exists) or branch "123"
/// - `v1.0.0` → Tag (if exists) or branch
/// - `fix/issue-123` → Branch
/// - `a1b2c3d4` → Commit SHA
/// - `pr:123` → Force PR interpretation
/// - `tag:v1.0.0` → Force tag interpretation
/// - `tag:latest-stable` → Latest tag excluding prereleases (alpha/beta)
/// - `tag:previous-stable` → Previous tag excluding prereleases
/// - `tag:latest` → Very latest tag (including prereleases)
/// - `tag:previous-latest` → Tag before the very latest
/// - `branch:main` → Force branch interpretation
/// - `commit:a1b2c3d` → Force commit interpretation
pub struct RefResolver<'a> {
    /// GitHub client for PR lookups (and commit validation in remote mode).
    github: &'a GitHubClient,
    /// Repository owner.
    owner: &'a str,
    /// Repository name.
    repo: &'a str,
    /// Local repository path (for git commands).
    repo_path: Option<&'a Path>,
    /// Remote client for clone-free resolution (`git ls-remote` + tags probe).
    remote: Option<RemoteGit>,
    /// Cached `ls-remote` snapshot: one network round-trip serves every
    /// tag/branch check within a single resolution session.
    remote_refs: OnceCell<RemoteRefs>,
}

impl<'a> RefResolver<'a> {
    /// Create a new reference resolver.
    ///
    /// # Arguments
    ///
    /// * `github` - GitHub client for PR lookups
    /// * `owner` - Repository owner (e.g., "wp-media")
    /// * `repo` - Repository name (e.g., "backwpup-pro")
    pub fn new(github: &'a GitHubClient, owner: &'a str, repo: &'a str) -> Self {
        Self {
            github,
            owner,
            repo,
            repo_path: None,
            remote: None,
            remote_refs: OnceCell::new(),
        }
    }

    /// Set the local repository path for git-based resolution.
    ///
    /// Takes priority over remote resolution when both are configured
    /// (local lookups are free once a clone exists).
    pub fn with_repo_path(mut self, path: &'a Path) -> Self {
        self.repo_path = Some(path);
        self
    }

    /// Enable clone-free resolution against the remote repository.
    ///
    /// Tags and branches are checked with a single `git ls-remote` call,
    /// commits are validated through the GitHub commits API, and `tag:`
    /// keywords use a minimal tags-only fetch — so a build pipeline can
    /// resolve (or reject) any reference **before** paying for a clone.
    pub fn with_remote(mut self, remote: RemoteGit) -> Self {
        self.remote = Some(remote);
        self
    }

    /// Fetch (once) and cache the remote's advertised refs.
    ///
    /// Only callable when a remote is configured.
    async fn remote_refs(&self) -> Result<&RemoteRefs> {
        let remote = self
            .remote
            .as_ref()
            .ok_or_else(|| Error::Git("No remote configured for ref resolution".to_string()))?;
        self.remote_refs
            .get_or_try_init(|| remote.ls_remote())
            .await
    }

    /// Resolve a reference string to a concrete ref.
    ///
    /// # Detection Priority
    ///
    /// 1. Explicit prefix (`pr:`, `tag:`, `branch:`, `commit:`)
    /// 2. PR number (all digits)
    /// 3. Commit SHA (hex, 7-40 chars)
    /// 4. Tag (exists in refs/tags)
    /// 5. Branch (exists in refs/heads or refs/remotes)
    ///
    /// # Errors
    ///
    /// Returns an error if the reference cannot be resolved.
    pub async fn resolve(&self, input: &str) -> Result<ResolvedRef> {
        let input = input.trim();

        if input.is_empty() {
            return Err(Error::Git("Reference cannot be empty".to_string()));
        }

        // 1. Check for explicit prefix
        if let Some(resolved) = self.try_resolve_explicit_prefix(input).await? {
            return Ok(resolved);
        }

        // 2. Try automatic detection
        self.resolve_automatic(input).await
    }

    /// Try to resolve with explicit prefix syntax.
    async fn try_resolve_explicit_prefix(&self, input: &str) -> Result<Option<ResolvedRef>> {
        let (prefix, value) = match input.split_once(':') {
            Some((p, v)) if !v.is_empty() => (p.to_lowercase(), v),
            _ => return Ok(None),
        };

        match prefix.as_str() {
            "pr" => {
                let pr_number = value
                    .parse::<u64>()
                    .map_err(|_| Error::Git(format!("Invalid PR number: '{value}'")))?;
                self.resolve_pr(input, pr_number).await.map(Some)
            }
            "tag" => {
                if let Some(resolved) = self.resolve_tag_keyword(input, value).await? {
                    return Ok(Some(resolved));
                }
                self.resolve_tag(input, value).await.map(Some)
            }
            "branch" => self.resolve_branch(input, value).await.map(Some),
            "commit" => self.resolve_commit(input, value).await.map(Some),
            "release" => self.resolve_release(input, value).map(Some),
            _ => Ok(None), // Unknown prefix, try automatic detection
        }
    }

    /// Automatic reference detection.
    async fn resolve_automatic(&self, input: &str) -> Result<ResolvedRef> {
        // If input contains ':', it's either an unknown prefix or invalid.
        // Since ':' is forbidden in git ref names, reject it early.
        if input.contains(':') {
            return Err(Error::Git(format!(
                "Invalid reference '{input}'. Unknown prefix or ':' is not allowed in git refs. \
                 Valid prefixes are: pr:, tag:, branch:, commit:, release:\n\
                 Special keywords: tag:latest-stable, tag:previous-stable, tag:latest, \
                 tag:previous-latest, release:latest-stable, release:previous-stable, \
                 release:latest, release:previous-latest"
            )));
        }

        // 1. All digits → likely PR number
        if input.chars().all(|c| c.is_ascii_digit())
            && let Ok(pr_number) = input.parse::<u64>()
        {
            // Try PR first, fall back to branch if not found
            match self.resolve_pr(input, pr_number).await {
                Ok(resolved) => return Ok(resolved),
                Err(_) => {
                    tracing::debug!("PR #{pr_number} not found, trying as branch");
                }
            }
        }

        // 2. Looks like a commit SHA (hex, 7-40 chars)
        if Self::looks_like_commit_sha(input) {
            if self.repo_path.is_some() {
                match self.resolve_commit(input, input).await {
                    Ok(resolved) => return Ok(resolved),
                    Err(_) => {
                        tracing::debug!("'{input}' is not a valid commit, trying other types");
                    }
                }
            } else if self.remote.is_some() {
                // Auto-detection must only accept a CONFIRMED commit — on a
                // 404 or an API outage alike, fall through to tag/branch
                // checks (a hex-looking name may well be a branch).
                match self
                    .github
                    .get_commit_sha(self.owner, self.repo, input)
                    .await
                {
                    Ok(Some(full_sha)) => {
                        return Ok(ResolvedRef {
                            input: input.to_string(),
                            source: RefSource::Commit(full_sha.clone()),
                            git_ref: full_sha.clone(),
                            commit_sha: Some(full_sha),
                        });
                    }
                    Ok(None) => {
                        tracing::debug!("'{input}' is not a commit on remote, trying other types");
                    }
                    Err(e) => {
                        tracing::debug!(
                            "Commit check for '{input}' failed ({e}), trying other types"
                        );
                    }
                }
            }
        }

        // 3. Try as tag (tags often have priority in version-based workflows)
        if self.repo_path.is_some() {
            if self.ref_exists_as_tag(input).await? {
                return self.resolve_tag(input, input).await;
            }

            // 4. Try as branch
            if self.ref_exists_as_branch(input).await? {
                return self.resolve_branch(input, input).await;
            }
        } else if self.remote.is_some() {
            // Clone-free: one ls-remote snapshot answers both checks.
            let refs = self.remote_refs().await?;
            if refs.tag_sha(input).is_some() {
                return self.resolve_tag(input, input).await;
            }
            if refs.branch_sha(input).is_some() {
                return self.resolve_branch(input, input).await;
            }
        }

        // 5. No local repo AND no remote — cannot verify anything, assume
        // branch (last-resort behavior for standalone resolver use).
        if self.repo_path.is_none() && self.remote.is_none() {
            tracing::warn!(
                "No local repo or remote available, assuming '{input}' is a branch. \
                 Use explicit prefix (tag:, commit:) if needed."
            );
            return Ok(ResolvedRef {
                input: input.to_string(),
                source: RefSource::Branch(input.to_string()),
                git_ref: input.to_string(),
                commit_sha: None,
            });
        }

        Err(Error::Git(format!(
            "Could not resolve '{input}' as PR, release, tag, branch, or commit. \
             Use explicit prefix (pr:, tag:, branch:, commit:, release:) to specify type. \
             Special keywords: tag:latest-stable, tag:previous-stable, tag:latest, \
             tag:previous-latest, release:latest-stable, release:previous-stable, \
             release:latest, release:previous-latest."
        )))
    }

    /// Check if input looks like a commit SHA.
    fn looks_like_commit_sha(input: &str) -> bool {
        let len = input.len();
        (7..=40).contains(&len) && input.chars().all(|c| c.is_ascii_hexdigit())
    }

    /// Resolve a PR number.
    async fn resolve_pr(&self, input: &str, pr_number: u64) -> Result<ResolvedRef> {
        tracing::debug!("Resolving PR #{pr_number}");

        let pr = self
            .github
            .get_pull_request(self.owner, self.repo, pr_number)
            .await
            .map_err(|e| Error::Git(format!("Failed to fetch PR #{pr_number}: {e}")))?;

        Ok(ResolvedRef {
            input: input.to_string(),
            source: RefSource::PullRequest(pr_number),
            git_ref: pr.head_branch.clone(),
            commit_sha: None, // Could fetch from PR if needed
        })
    }

    /// Resolve a tag name.
    ///
    /// Validation order: local repository (free once cloned), then remote
    /// (`ls-remote`, no clone needed), then trust-the-user as a last resort
    /// when neither is available.
    async fn resolve_tag(&self, input: &str, tag: &str) -> Result<ResolvedRef> {
        tracing::debug!("Resolving tag '{tag}'");

        if let Some(repo_path) = self.repo_path {
            let exists = self.ref_exists_as_tag(tag).await?;
            if !exists {
                return Err(Error::Git(format!("Tag '{tag}' not found")));
            }

            // Get the commit SHA for the tag
            let commit_sha = self.get_ref_commit(repo_path, tag).await.ok();

            Ok(ResolvedRef {
                input: input.to_string(),
                source: RefSource::Tag(tag.to_string()),
                git_ref: tag.to_string(),
                commit_sha,
            })
        } else if self.remote.is_some() {
            // Clone-free: check the remote's advertised refs. The SHA from
            // ls-remote is the peeled commit, exactly what checkout lands on.
            let refs = self.remote_refs().await?;
            match refs.tag_sha(tag) {
                Some(sha) => Ok(ResolvedRef {
                    input: input.to_string(),
                    source: RefSource::Tag(tag.to_string()),
                    git_ref: tag.to_string(),
                    commit_sha: Some(sha.to_string()),
                }),
                None => Err(Error::Git(format!(
                    "Tag '{tag}' not found on remote {}/{}",
                    self.owner, self.repo
                ))),
            }
        } else {
            // No local repo and no remote configured: trust the user.
            Ok(ResolvedRef {
                input: input.to_string(),
                source: RefSource::Tag(tag.to_string()),
                git_ref: tag.to_string(),
                commit_sha: None,
            })
        }
    }

    /// Resolve a branch name.
    ///
    /// Same validation order as [`Self::resolve_tag`].
    async fn resolve_branch(&self, input: &str, branch: &str) -> Result<ResolvedRef> {
        tracing::debug!("Resolving branch '{branch}'");

        if let Some(repo_path) = self.repo_path {
            let exists = self.ref_exists_as_branch(branch).await?;
            if !exists {
                return Err(Error::Git(format!("Branch '{branch}' not found")));
            }

            // Get the commit SHA for the branch
            let commit_sha = self.get_ref_commit(repo_path, branch).await.ok();

            Ok(ResolvedRef {
                input: input.to_string(),
                source: RefSource::Branch(branch.to_string()),
                git_ref: branch.to_string(),
                commit_sha,
            })
        } else if self.remote.is_some() {
            let refs = self.remote_refs().await?;
            match refs.branch_sha(branch) {
                Some(sha) => Ok(ResolvedRef {
                    input: input.to_string(),
                    source: RefSource::Branch(branch.to_string()),
                    git_ref: branch.to_string(),
                    commit_sha: Some(sha.to_string()),
                }),
                None => Err(Error::Git(format!(
                    "Branch '{branch}' not found on remote {}/{}",
                    self.owner, self.repo
                ))),
            }
        } else {
            // No local repo and no remote configured: trust the user.
            Ok(ResolvedRef {
                input: input.to_string(),
                source: RefSource::Branch(branch.to_string()),
                git_ref: branch.to_string(),
                commit_sha: None,
            })
        }
    }

    /// Resolve a commit SHA.
    ///
    /// In remote mode the commit is validated (and a short SHA expanded)
    /// through the GitHub commits API — `ls-remote` only lists refs and
    /// cannot confirm arbitrary commits. If the API is unreachable (rate
    /// limit, outage), resolution degrades to trusting the input with a
    /// warning: the post-clone checkout still validates it definitively.
    async fn resolve_commit(&self, input: &str, sha: &str) -> Result<ResolvedRef> {
        tracing::debug!("Resolving commit '{sha}'");

        // Validate it's a valid hex string
        if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Git(format!("Invalid commit SHA: '{sha}'")));
        }

        if sha.len() < 7 {
            return Err(Error::Git(format!(
                "Commit SHA too short (minimum 7 characters): '{sha}'"
            )));
        }

        // Validate commit exists if we have a local repo
        if let Some(repo_path) = self.repo_path {
            let full_sha = self.validate_and_expand_commit(repo_path, sha).await?;

            Ok(ResolvedRef {
                input: input.to_string(),
                source: RefSource::Commit(full_sha.clone()),
                git_ref: full_sha.clone(),
                commit_sha: Some(full_sha),
            })
        } else if self.remote.is_some() {
            match self.github.get_commit_sha(self.owner, self.repo, sha).await {
                Ok(Some(full_sha)) => Ok(ResolvedRef {
                    input: input.to_string(),
                    source: RefSource::Commit(full_sha.clone()),
                    git_ref: full_sha.clone(),
                    commit_sha: Some(full_sha),
                }),
                Ok(None) => Err(Error::Git(format!(
                    "Commit '{sha}' not found in {}/{}",
                    self.owner, self.repo
                ))),
                Err(e) => {
                    // API unavailable ≠ commit missing. Availability wins:
                    // proceed unvalidated, checkout will be the final judge.
                    tracing::warn!(
                        "Could not verify commit '{sha}' via GitHub API ({e}); \
                         proceeding — checkout will validate it"
                    );
                    Ok(ResolvedRef {
                        input: input.to_string(),
                        source: RefSource::Commit(sha.to_string()),
                        git_ref: sha.to_string(),
                        commit_sha: Some(sha.to_string()),
                    })
                }
            }
        } else {
            // No local repo and no remote configured: trust the user.
            Ok(ResolvedRef {
                input: input.to_string(),
                source: RefSource::Commit(sha.to_string()),
                git_ref: sha.to_string(),
                commit_sha: Some(sha.to_string()),
            })
        }
    }

    /// Resolve a release tag.
    ///
    /// Unlike other resolution methods, this does not require a local repository.
    /// The actual GitHub API check happens later in the build command's early-resolve phase.
    fn resolve_release(&self, input: &str, tag: &str) -> Result<ResolvedRef> {
        tracing::debug!("Resolving release '{tag}'");

        Ok(ResolvedRef {
            input: input.to_string(),
            source: RefSource::Release(tag.to_string()),
            git_ref: tag.to_string(),
            commit_sha: None,
        })
    }

    /// Check if a ref exists as a tag.
    async fn ref_exists_as_tag(&self, name: &str) -> Result<bool> {
        let repo_path = self.repo_path.ok_or_else(|| {
            Error::Git("Local repository path required for tag lookup".to_string())
        })?;

        let output = Command::new("git")
            .args([
                "show-ref",
                "--tags",
                "--verify",
                &format!("refs/tags/{name}"),
            ])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to check tag: {e}")))?;

        Ok(output.status.success())
    }

    /// Check if a ref exists as a branch.
    async fn ref_exists_as_branch(&self, name: &str) -> Result<bool> {
        let repo_path = self.repo_path.ok_or_else(|| {
            Error::Git("Local repository path required for branch lookup".to_string())
        })?;

        // Check local branches
        let local = Command::new("git")
            .args([
                "show-ref",
                "--heads",
                "--verify",
                &format!("refs/heads/{name}"),
            ])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to check local branch: {e}")))?;

        if local.status.success() {
            return Ok(true);
        }

        // Check remote branches (origin)
        let remote = Command::new("git")
            .args([
                "show-ref",
                "--verify",
                &format!("refs/remotes/origin/{name}"),
            ])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to check remote branch: {e}")))?;

        Ok(remote.status.success())
    }

    /// Get the commit SHA for a ref.
    async fn get_ref_commit(&self, repo_path: &Path, reference: &str) -> Result<String> {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", reference])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to get commit for '{reference}': {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!(
                "Failed to resolve '{reference}': {stderr}"
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Validate and expand a short commit SHA to full SHA.
    async fn validate_and_expand_commit(&self, repo_path: &Path, sha: &str) -> Result<String> {
        // Use cat-file to verify it's a commit object
        let output = Command::new("git")
            .args(["cat-file", "-t", sha])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to validate commit '{sha}': {e}")))?;

        if !output.status.success() {
            return Err(Error::Git(format!("Commit '{sha}' not found")));
        }

        let object_type = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if object_type != "commit" {
            return Err(Error::Git(format!(
                "'{sha}' is a {object_type}, not a commit"
            )));
        }

        // Expand to full SHA
        let output = Command::new("git")
            .args(["rev-parse", "--verify", sha])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to expand commit SHA: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("Failed to expand commit SHA: {stderr}")));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Resolve a `tag:` keyword to a concrete tag, or return `None` if the
    /// value is not a recognized keyword (i.e. it's a literal tag name).
    ///
    /// Recognized keywords:
    /// - `latest-stable` — latest tag excluding prereleases (`-alpha`, `-beta`)
    /// - `previous-stable` — previous tag excluding prereleases
    /// - `latest` — very latest tag (any, including prereleases)
    /// - `previous-latest` — the tag right before the very latest
    ///
    /// Tags are sorted by creation date (most recent first): via local
    /// `git tag --sort=-creatordate` when a repository is available, or via
    /// a minimal clone-free tags probe ([`RemoteGit::tags_by_creatordate`])
    /// in remote mode — both produce identical ordering.
    async fn resolve_tag_keyword(&self, input: &str, keyword: &str) -> Result<Option<ResolvedRef>> {
        let (stable_only, index) = match keyword.to_ascii_lowercase().as_str() {
            "latest-stable" => (true, 0),
            "previous-stable" => (true, 1),
            "latest" => (false, 0),
            "previous-latest" => (false, 1),
            _ => return Ok(None),
        };

        let tags = if self.repo_path.is_some() {
            self.get_tags_by_date().await?
        } else if let Some(remote) = &self.remote {
            remote.tags_by_creatordate().await?
        } else {
            return Err(Error::Git(
                "Resolving tag keywords requires a local repository or a configured remote"
                    .to_string(),
            ));
        };

        let filtered: Vec<&String> = if stable_only {
            tags.iter().filter(|t| !is_prerelease_tag(t)).collect()
        } else {
            tags.iter().collect()
        };

        let label = keyword.to_ascii_lowercase();

        if filtered.is_empty() {
            return Err(Error::Git(format!(
                "No {qualifier}tags found in repository",
                qualifier = if stable_only { "stable " } else { "" }
            )));
        }

        let tag = filtered.get(index).ok_or_else(|| {
            Error::Git(format!(
                "Not enough {qualifier}tags to determine '{label}' \
                 (need at least {needed}, found {found})",
                qualifier = if stable_only { "stable " } else { "" },
                needed = index + 1,
                found = filtered.len(),
            ))
        })?;

        tracing::info!("Resolved 'tag:{label}' to tag '{tag}'");
        self.resolve_tag(input, tag).await.map(Some)
    }

    /// List all tags in the local repository sorted by creation date
    /// (most recent first).
    ///
    /// Uses `git tag --sort=-creatordate` which orders tags by the date
    /// they were created (the `taggerdate` for annotated tags or
    /// `committerdate` for lightweight tags). This avoids the pitfalls
    /// of version-based sorting where e.g. `v28.19` would appear above
    /// `v3.21.1` because 28 > 3.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No local repository path is set
    /// - The `git tag` command fails
    ///
    /// # Sources
    ///
    /// - `git tag --sort`: <https://git-scm.com/docs/git-tag#Documentation/git-tag.txt---sortltkeygt>
    /// - `creatordate`: <https://git-scm.com/docs/git-for-each-ref#_field_names>
    ///   "For commit and tag objects, `creatordate` corresponds to the appropriate
    ///   date from the committer or tagger fields" (Git 2.12+).
    async fn get_tags_by_date(&self) -> Result<Vec<String>> {
        let repo_path = self.repo_path.ok_or_else(|| {
            Error::Git("Local repository path required for tag lookup".to_string())
        })?;

        let output = Command::new("git")
            .args(["tag", "--sort=-creatordate"])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to list tags: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!("Failed to list tags: {stderr}")));
        }

        let tags: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect();

        Ok(tags)
    }
}

/// Check whether a tag name looks like a prerelease.
///
/// Returns `true` if the tag contains `-alpha`, `-beta`, `-rc`, or
/// their numbered variants (e.g., `-alpha2`, `-beta3`, `-rc1`),
/// case-insensitively.
///
/// # Examples
///
/// ```
/// use apvm_core::git::is_prerelease_tag;
///
/// assert!(is_prerelease_tag("v3.21.1-alpha"));
/// assert!(is_prerelease_tag("v3.21.1-alpha2"));
/// assert!(is_prerelease_tag("v3.21.1-beta"));
/// assert!(is_prerelease_tag("v3.21.1-BETA3"));
/// assert!(is_prerelease_tag("v5.0.0-rc1"));
/// assert!(!is_prerelease_tag("v3.21.1"));
/// assert!(!is_prerelease_tag("v3.21.0"));
/// ```
pub fn is_prerelease_tag(tag: &str) -> bool {
    let lower = tag.to_ascii_lowercase();
    // Match `-alpha`, `-beta`, `-rc` optionally followed by digits.
    lower.contains("-alpha") || lower.contains("-beta") || lower.contains("-rc")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_looks_like_commit_sha() {
        // Valid SHAs
        assert!(RefResolver::looks_like_commit_sha("a1b2c3d"));
        assert!(RefResolver::looks_like_commit_sha(
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
        ));
        assert!(RefResolver::looks_like_commit_sha("ABCDEF1"));

        // Invalid - too short
        assert!(!RefResolver::looks_like_commit_sha("a1b2c3"));

        // Invalid - too long
        assert!(!RefResolver::looks_like_commit_sha(
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2x"
        ));

        // Invalid - non-hex characters
        assert!(!RefResolver::looks_like_commit_sha("a1b2c3g"));
        assert!(!RefResolver::looks_like_commit_sha("fix/issue"));
    }

    #[test]
    fn test_ref_source_description() {
        assert_eq!(RefSource::PullRequest(123).description(), "PR #123");
        assert_eq!(
            RefSource::Tag("v1.0.0".into()).description(),
            "tag 'v1.0.0'"
        );
        assert_eq!(
            RefSource::Branch("develop".into()).description(),
            "branch 'develop'"
        );
        assert_eq!(
            RefSource::Commit("a1b2c3d4e5f6".into()).description(),
            "commit a1b2c3d"
        );
    }

    #[test]
    fn test_ref_source_to_build_source() {
        // RefSource → BuildSource
        let ref_pr = RefSource::PullRequest(123);
        let build_pr: BuildSource = ref_pr.into();
        assert_eq!(build_pr, BuildSource::PullRequest(123));

        let ref_branch = RefSource::Branch("develop".into());
        let build_branch: BuildSource = ref_branch.into();
        assert_eq!(build_branch, BuildSource::Branch("develop".into()));

        let ref_tag = RefSource::Tag("v1.0.0".into());
        let build_tag: BuildSource = ref_tag.into();
        assert_eq!(build_tag, BuildSource::Tag("v1.0.0".into()));

        let ref_commit = RefSource::Commit("a1b2c3d".into());
        let build_commit: BuildSource = ref_commit.into();
        assert_eq!(build_commit, BuildSource::Commit("a1b2c3d".into()));
    }

    #[test]
    fn test_build_source_to_ref_source() {
        // BuildSource → RefSource
        let build_pr = BuildSource::PullRequest(456);
        let ref_pr: RefSource = build_pr.into();
        assert_eq!(ref_pr, RefSource::PullRequest(456));

        let build_branch = BuildSource::Branch("main".into());
        let ref_branch: RefSource = build_branch.into();
        assert_eq!(ref_branch, RefSource::Branch("main".into()));

        let build_tag = BuildSource::Tag("v2.0.0".into());
        let ref_tag: RefSource = build_tag.into();
        assert_eq!(ref_tag, RefSource::Tag("v2.0.0".into()));

        let build_commit = BuildSource::Commit("deadbeef".into());
        let ref_commit: RefSource = build_commit.into();
        assert_eq!(ref_commit, RefSource::Commit("deadbeef".into()));
    }

    #[test]
    fn test_ref_source_to_build_source_methods() {
        // Test the explicit methods
        let ref_source = RefSource::Branch("feature".into());

        // to_build_source (borrows)
        let build_source = ref_source.to_build_source();
        assert_eq!(build_source, BuildSource::Branch("feature".into()));

        // into_build_source (consumes)
        let build_source = ref_source.into_build_source();
        assert_eq!(build_source, BuildSource::Branch("feature".into()));
    }

    #[test]
    fn test_from_reference_preserves_data() {
        // Round-trip: RefSource → BuildSource → RefSource
        let original = RefSource::PullRequest(999);
        let build: BuildSource = original.clone().into();
        let back: RefSource = build.into();
        assert_eq!(original, back);

        // Round-trip: BuildSource → RefSource → BuildSource
        let original = BuildSource::Tag("release-1.0".into());
        let ref_source: RefSource = original.clone().into();
        let back: BuildSource = ref_source.into();
        assert_eq!(original, back);
    }

    #[test]
    fn test_detailed_description_pr() {
        let resolved = ResolvedRef {
            input: "pr:42".to_string(),
            source: RefSource::PullRequest(42),
            git_ref: "feature/my-feature".to_string(),
            commit_sha: None,
        };
        assert_eq!(
            resolved.detailed_description(),
            "PR #42 (branch: feature/my-feature)"
        );
    }

    #[test]
    fn test_detailed_description_branch() {
        let resolved = ResolvedRef {
            input: "develop".to_string(),
            source: RefSource::Branch("develop".to_string()),
            git_ref: "develop".to_string(),
            commit_sha: None,
        };
        assert_eq!(resolved.detailed_description(), "branch 'develop'");
    }

    #[test]
    fn test_detailed_description_tag() {
        let resolved = ResolvedRef {
            input: "tag:v1.0.0".to_string(),
            source: RefSource::Tag("v1.0.0".to_string()),
            git_ref: "v1.0.0".to_string(),
            commit_sha: None,
        };
        assert_eq!(resolved.detailed_description(), "tag 'v1.0.0'");
    }

    #[test]
    fn test_detailed_description_commit() {
        let resolved = ResolvedRef {
            input: "commit:a1b2c3d4e5f6".to_string(),
            source: RefSource::Commit("a1b2c3d4e5f6".to_string()),
            git_ref: "a1b2c3d4e5f6".to_string(),
            commit_sha: Some("a1b2c3d4e5f6".to_string()),
        };
        assert_eq!(resolved.detailed_description(), "commit a1b2c3d");
    }

    // =========================================================================
    // Remote-mode resolution (clone-free, against local file:// repos)
    // =========================================================================

    use crate::git::testutil::{file_url, make_remote_repo};

    /// Resolver in remote mode against a local test repo. The GitHub client
    /// is anonymous and unused by these paths (tags/branches/keywords go
    /// through git, not the API).
    fn remote_resolver(url: &str) -> RefResolver<'_> {
        // Leak the client: tests only — keeps the borrow-based API simple.
        let github: &'static GitHubClient = Box::leak(Box::new(GitHubClient::anonymous().unwrap()));
        RefResolver::new(github, "owner", "repo").with_remote(RemoteGit::new(url, None))
    }

    #[tokio::test]
    async fn remote_resolves_explicit_branch_with_sha() {
        let repo = make_remote_repo();
        let url = file_url(&repo);
        let resolver = remote_resolver(&url);

        let resolved = resolver.resolve("branch:feature/x").await.unwrap();
        assert_eq!(resolved.source, RefSource::Branch("feature/x".into()));
        assert_eq!(resolved.git_ref, "feature/x");
        let sha = resolved
            .commit_sha
            .expect("remote resolution must yield SHA");
        assert_eq!(sha.len(), 40);
    }

    #[tokio::test]
    async fn remote_resolves_explicit_tag_to_peeled_commit() {
        let repo = make_remote_repo();
        let url = file_url(&repo);
        let resolver = remote_resolver(&url);

        // v2.0.0 is annotated: the resolved SHA must be the tagged COMMIT
        // (peeled), which equals main's tip.
        let tag = resolver.resolve("tag:v2.0.0").await.unwrap();
        let main = remote_resolver(&url).resolve("branch:main").await.unwrap();
        assert_eq!(tag.source, RefSource::Tag("v2.0.0".into()));
        assert_eq!(tag.commit_sha, main.commit_sha);
    }

    #[tokio::test]
    async fn remote_missing_refs_fail_before_any_clone() {
        let repo = make_remote_repo();
        let url = file_url(&repo);

        let err = remote_resolver(&url)
            .resolve("tag:v9.9.9")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found on remote"));

        let err = remote_resolver(&url)
            .resolve("branch:does-not-exist")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found on remote"));
    }

    #[tokio::test]
    async fn remote_resolves_tag_keywords_by_creation_date() {
        let repo = make_remote_repo();
        let url = file_url(&repo);

        // latest / latest-stable → v2.0.0 (2025-01, stable).
        for input in ["tag:latest", "tag:latest-stable"] {
            let resolved = remote_resolver(&url).resolve(input).await.unwrap();
            assert_eq!(
                resolved.source,
                RefSource::Tag("v2.0.0".into()),
                "input {input}"
            );
        }

        // previous-latest → v2.0.0-beta1 (2024-06, prerelease included).
        let resolved = remote_resolver(&url)
            .resolve("tag:previous-latest")
            .await
            .unwrap();
        assert_eq!(resolved.source, RefSource::Tag("v2.0.0-beta1".into()));

        // previous-stable → v1.0.0 (beta filtered out).
        let resolved = remote_resolver(&url)
            .resolve("tag:previous-stable")
            .await
            .unwrap();
        assert_eq!(resolved.source, RefSource::Tag("v1.0.0".into()));
    }

    #[tokio::test]
    async fn remote_auto_detects_tag_then_branch() {
        let repo = make_remote_repo();
        let url = file_url(&repo);

        // Bare tag name → tag (tags have priority over branches).
        let resolved = remote_resolver(&url).resolve("v1.0.0").await.unwrap();
        assert_eq!(resolved.source, RefSource::Tag("v1.0.0".into()));

        // Bare branch names → branch, with SHA.
        for input in ["main", "feature/x"] {
            let resolved = remote_resolver(&url).resolve(input).await.unwrap();
            assert_eq!(
                resolved.source,
                RefSource::Branch(input.into()),
                "input {input}"
            );
            assert!(resolved.commit_sha.is_some());
        }
    }

    #[tokio::test]
    async fn remote_auto_detect_unknown_ref_fails_early() {
        let repo = make_remote_repo();
        let url = file_url(&repo);

        // With a remote configured, an unknown bare name must ERROR (the
        // old assume-it's-a-branch guess is reserved for the no-repo,
        // no-remote standalone case).
        let err = remote_resolver(&url)
            .resolve("totally-unknown-thing")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Could not resolve"));
    }

    #[tokio::test]
    async fn remote_unreachable_fails_early_with_clear_error() {
        let missing = tempfile::TempDir::new().unwrap();
        let url = format!("file://{}/nope", missing.path().display());

        let err = remote_resolver(&url)
            .resolve("branch:main")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ls-remote failed"));
    }

    #[test]
    fn test_is_prerelease_tag_alpha() {
        assert!(is_prerelease_tag("v3.21.1-alpha"));
        assert!(is_prerelease_tag("v3.21.1-alpha2"));
        assert!(is_prerelease_tag("v3.21.1-ALPHA"));
        assert!(is_prerelease_tag("v3.21.1-Alpha3"));
    }

    #[test]
    fn test_is_prerelease_tag_beta() {
        assert!(is_prerelease_tag("v3.21.1-beta"));
        assert!(is_prerelease_tag("v3.21.1-beta1"));
        assert!(is_prerelease_tag("v3.21.1-BETA"));
        assert!(is_prerelease_tag("v3.21.1-Beta2"));
    }

    #[test]
    fn test_is_prerelease_tag_rc() {
        assert!(is_prerelease_tag("v5.0.0-rc1"));
        assert!(is_prerelease_tag("v5.0.0-RC2"));
        assert!(is_prerelease_tag("v5.0.0-rc"));
    }

    #[test]
    fn test_is_prerelease_tag_stable() {
        assert!(!is_prerelease_tag("v3.21.1"));
        assert!(!is_prerelease_tag("v3.21.0"));
        assert!(!is_prerelease_tag("3.21.1"));
        assert!(!is_prerelease_tag("release-1.0"));
    }
}
