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
    /// Build a resolved reference from a fetched pull request.
    ///
    /// The PR's head SHA becomes [`commit_sha`](Self::commit_sha) so a cache
    /// lookup can happen **before** cloning, and its head branch becomes the
    /// [`git_ref`](Self::git_ref) that will be checked out. Shared by both the
    /// clone-free early-resolve path and the standard resolver so the mapping
    /// lives in exactly one place.
    pub(crate) fn from_pull_request(pr: &crate::github::PullRequest, input: &str) -> Self {
        Self {
            input: input.to_string(),
            source: RefSource::PullRequest(pr.number),
            git_ref: pr.head_branch.clone(),
            commit_sha: pr.head_sha.clone(),
        }
    }

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
    /// 5. Branch (exists in refs/heads, or as origin's remote-tracking
    ///    branch in refs/remotes/origin)
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

        Ok(ResolvedRef::from_pull_request(&pr, input))
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

            // Peel to the commit checkout lands on (an annotated tag's own
            // object SHA would never match a build's commit).
            let commit_sha = peeled_commit(repo_path, &format!("refs/tags/{tag}")).await?;

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
            let Some(branch_ref) = local_branch_ref(repo_path, branch).await? else {
                return Err(Error::Git(format!("Branch '{branch}' not found")));
            };

            // Fully qualified, so a same-named tag can't answer instead.
            let commit_sha = peeled_commit(repo_path, &branch_ref).await?;

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

        // 64 hex is the longest object name (SHA-256); git aborts on longer
        // prefixes instead of reporting them missing.
        if sha.len() > 64 {
            return Err(Error::Git(format!(
                "Commit SHA too long (maximum 64 characters): '{sha}'"
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

        Ok(local_branch_ref(repo_path, name).await?.is_some())
    }

    /// Validate that `sha` (full or abbreviated) names exactly one commit
    /// object, and expand it to the full SHA.
    ///
    /// Only objects are considered — never refs — so a branch named like a
    /// SHA (`deadbeef`) can't pose as a commit: it falls through to the
    /// branch lookup instead.
    async fn validate_and_expand_commit(&self, repo_path: &Path, sha: &str) -> Result<String> {
        let candidates = objects_with_prefix(repo_path, sha).await?;
        pick_commit(sha, &candidates)
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

/// The fully-qualified ref a branch name resolves to in a local clone: the
/// local branch (`refs/heads/<name>`), else origin's remote-tracking branch
/// (`refs/remotes/origin/<name>`). Only branch namespaces are searched, so a
/// same-named tag never answers.
///
/// # Arguments
///
/// * `repo_path` - The local repository
/// * `name` - Branch name as the user wrote it
///
/// # Returns
///
/// The first of those refs that exists, or `None` when neither does.
///
/// # Errors
///
/// [`Error::Git`] when git cannot be started.
async fn local_branch_ref(repo_path: &Path, name: &str) -> Result<Option<String>> {
    for full_ref in [
        format!("refs/heads/{name}"),
        format!("refs/remotes/origin/{name}"),
    ] {
        let output = Command::new("git")
            .args(["show-ref", "--verify", "--quiet", &full_ref])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to check branch '{name}': {e}")))?;
        if output.status.success() {
            return Ok(Some(full_ref));
        }
    }
    Ok(None)
}

/// The commit a fully-qualified ref points at, peeling annotated tags
/// (`<ref>^{commit}`).
///
/// # Arguments
///
/// * `repo_path` - The local repository
/// * `full_ref` - A fully-qualified ref such as `refs/tags/v1.0.0`; never a
///   bare name, which git would resolve tags-first
///
/// # Returns
///
/// The full commit SHA, or `None` when the ref does not exist or does not
/// lead to a commit (e.g. a tag on a tree).
///
/// # Errors
///
/// [`Error::Git`] when git cannot be started.
async fn peeled_commit(repo_path: &Path, full_ref: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{full_ref}^{{commit}}"),
        ])
        .current_dir(repo_path)
        .output()
        .await
        .map_err(|e| Error::Git(format!("Failed to resolve '{full_ref}': {e}")))?;

    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

/// Every object whose name starts with `prefix`, as `(full SHA, type)`.
///
/// `git rev-parse --disambiguate` lists object names only — unlike
/// `cat-file`/`rev-parse <name>`, which would also resolve a branch or tag
/// spelled like the prefix. Each full SHA is then typed with `cat-file -t`
/// (unambiguous for a full-length name).
///
/// # Arguments
///
/// * `repo_path` - The local repository
/// * `prefix` - Hex SHA prefix (validated as hex, 7–64 chars, by the caller)
///
/// # Returns
///
/// Every matching object; empty when nothing matches.
///
/// # Errors
///
/// [`Error::Git`] when git cannot be started or the lookup fails.
async fn objects_with_prefix(repo_path: &Path, prefix: &str) -> Result<Vec<(String, String)>> {
    let listed = Command::new("git")
        .args(["rev-parse", &format!("--disambiguate={prefix}")])
        .current_dir(repo_path)
        .output()
        .await
        .map_err(|e| Error::Git(format!("Failed to look up commit '{prefix}': {e}")))?;
    if !listed.status.success() {
        let stderr = String::from_utf8_lossy(&listed.stderr);
        return Err(Error::Git(format!(
            "Failed to look up commit '{prefix}': {}",
            stderr.trim()
        )));
    }

    let mut objects = Vec::new();
    for oid in String::from_utf8_lossy(&listed.stdout).split_whitespace() {
        let typed = Command::new("git")
            .args(["cat-file", "-t", oid])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to read object '{oid}': {e}")))?;
        if typed.status.success() {
            let kind = String::from_utf8_lossy(&typed.stdout).trim().to_string();
            objects.push((oid.to_string(), kind));
        }
    }
    Ok(objects)
}

/// Choose the commit that `prefix` names among the objects sharing it.
///
/// Pure (the git queries happen in [`objects_with_prefix`]) so every outcome
/// is unit-tested.
///
/// # Arguments
///
/// * `prefix` - The SHA (prefix) the user gave, for error messages
/// * `objects` - `(full SHA, object type)` of every object with that prefix
///
/// # Returns
///
/// The full SHA of the single matching commit.
///
/// # Errors
///
/// [`Error::Git`] when nothing matches, when only non-commit objects match,
/// or when several commits match (the prefix is too short to be unique).
fn pick_commit(prefix: &str, objects: &[(String, String)]) -> Result<String> {
    // Only names that start with the whole input count — git also lists a
    // shorter object for an over-long input (`<sha>0` in a SHA-1 repo).
    let wanted = prefix.to_ascii_lowercase();
    let objects: Vec<&(String, String)> = objects
        .iter()
        .filter(|(oid, _)| oid.to_ascii_lowercase().starts_with(&wanted))
        .collect();
    let commits: Vec<&str> = objects
        .iter()
        .filter(|(_, kind)| kind == "commit")
        .map(|(oid, _)| oid.as_str())
        .collect();

    match (commits.as_slice(), objects.as_slice()) {
        ([commit], _) => Ok((*commit).to_string()),
        ([], []) => Err(Error::Git(format!("Commit '{prefix}' not found"))),
        ([], [(_, kind)]) => Err(Error::Git(format!("'{prefix}' is a {kind}, not a commit"))),
        ([], _) => Err(Error::Git(format!(
            "'{prefix}' matches no commit (only {} other objects)",
            objects.len()
        ))),
        (many, _) => Err(Error::Git(format!(
            "Commit SHA '{prefix}' is ambiguous: it matches {} commits. \
             Use more characters.",
            many.len()
        ))),
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

    // =========================================================================
    // ResolvedRef::from_pull_request — PR head SHA becomes the pre-clone commit
    // =========================================================================

    fn make_pr(
        number: u64,
        head_branch: &str,
        head_sha: Option<&str>,
    ) -> crate::github::PullRequest {
        crate::github::PullRequest {
            number,
            title: "Some PR".to_string(),
            base_branch: "trunk".to_string(),
            head_branch: head_branch.to_string(),
            head_sha: head_sha.map(str::to_string),
            owner: "wp-media".to_string(),
            repo: "wp-rocket".to_string(),
            html_url: None,
        }
    }

    #[test]
    fn from_pull_request_carries_head_sha_as_commit() {
        let pr = make_pr(
            8556,
            "feature/faster",
            Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678"),
        );
        let resolved = ResolvedRef::from_pull_request(&pr, "pr:8556");

        assert_eq!(resolved.input, "pr:8556");
        assert_eq!(resolved.source, RefSource::PullRequest(8556));
        assert_eq!(resolved.git_ref, "feature/faster");
        assert_eq!(
            resolved.commit_sha.as_deref(),
            Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678")
        );
    }

    #[test]
    fn from_pull_request_uses_pr_number_not_input() {
        // The source discriminant comes from the fetched PR, not the raw input.
        let pr = make_pr(42, "develop", Some("deadbeef"));
        let resolved = ResolvedRef::from_pull_request(&pr, "42");
        assert_eq!(resolved.source, RefSource::PullRequest(42));
    }

    #[test]
    fn from_pull_request_without_head_sha_yields_no_commit() {
        // A missing head SHA degrades to "no pre-clone cache key", never an error.
        let pr = make_pr(1, "main", None);
        let resolved = ResolvedRef::from_pull_request(&pr, "pr:1");
        assert!(resolved.commit_sha.is_none());
        assert_eq!(resolved.git_ref, "main");
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
        RefResolver::new(leaked_client(), "owner", "repo").with_remote(RemoteGit::new(url, None))
    }

    /// An anonymous GitHub client with a `'static` lifetime. Leaked: tests
    /// only — keeps the borrow-based resolver API simple. Never called over
    /// the network by these tests (no PR inputs are resolved).
    fn leaked_client() -> &'static GitHubClient {
        Box::leak(Box::new(GitHubClient::anonymous().unwrap()))
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

    // =========================================================================
    // Pure conversions / descriptions not covered above
    // =========================================================================

    #[test]
    fn release_source_round_trips_and_describes() {
        let source = RefSource::Release("v3.0.0".into());
        assert_eq!(source.description(), "release 'v3.0.0'");
        assert_eq!(source.to_string(), "release 'v3.0.0'");

        let build: BuildSource = (&source).into();
        assert_eq!(build, BuildSource::Release("v3.0.0".into()));
        let back: RefSource = (&build).into();
        assert_eq!(back, source);
        let owned: RefSource = build.into();
        assert_eq!(owned, source);
    }

    #[test]
    fn borrowed_conversions_match_owned_for_every_variant() {
        let all = [
            RefSource::PullRequest(7),
            RefSource::Branch("b".into()),
            RefSource::Tag("t".into()),
            RefSource::Commit("abcdef1".into()),
            RefSource::Release("r".into()),
        ];
        for source in all {
            let borrowed = BuildSource::from(&source);
            assert_eq!(RefSource::from(&borrowed), source);
            assert_eq!(borrowed, BuildSource::from(source));
        }
    }

    #[test]
    fn commit_description_never_panics_on_multibyte_input() {
        // Byte slicing at 7 would split "é"; chars() must be used.
        let source = RefSource::Commit("abcdeéfgh".into());
        assert_eq!(source.description(), "commit abcdeéf");
    }

    #[test]
    fn resolved_ref_build_source_helpers_use_the_source() {
        let resolved = ResolvedRef {
            input: "tag:v1".into(),
            source: RefSource::Tag("v1".into()),
            git_ref: "v1".into(),
            commit_sha: None,
        };
        assert_eq!(resolved.to_build_source(), BuildSource::Tag("v1".into()));
        assert_eq!(resolved.into_build_source(), BuildSource::Tag("v1".into()));
    }

    // =========================================================================
    // Input validation and standalone (no repo, no remote) mode
    // =========================================================================

    /// Resolver with neither a local repo nor a remote: the trust-the-user
    /// fallback. Every path exercised here is offline (no PR inputs).
    fn standalone_resolver() -> RefResolver<'static> {
        RefResolver::new(leaked_client(), "owner", "repo")
    }

    #[tokio::test]
    async fn empty_or_blank_input_is_rejected() {
        for input in ["", "   ", "\t\n"] {
            let err = standalone_resolver().resolve(input).await.unwrap_err();
            assert!(err.to_string().contains("cannot be empty"), "{input:?}");
        }
    }

    #[tokio::test]
    async fn input_is_trimmed_before_resolution() {
        let resolved = standalone_resolver()
            .resolve("  branch:develop \n")
            .await
            .unwrap();
        assert_eq!(resolved.git_ref, "develop");
        assert_eq!(resolved.input, "branch:develop");
    }

    #[tokio::test]
    async fn invalid_pr_numbers_fail_before_any_api_call() {
        for input in ["pr:abc", "pr:-1", "pr:1.5", "PR:x"] {
            let err = standalone_resolver().resolve(input).await.unwrap_err();
            assert!(err.to_string().contains("Invalid PR number"), "{input}");
        }
    }

    #[tokio::test]
    async fn unknown_prefix_or_empty_value_is_rejected_with_help() {
        // ':' is illegal in git refs, so these can never be valid names.
        for input in ["foo:bar", "tag:", "branch:", "v1:2"] {
            let err = standalone_resolver().resolve(input).await.unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("Invalid reference"), "{input}: {msg}");
            assert!(msg.contains("Valid prefixes"), "{input}: {msg}");
        }
    }

    #[tokio::test]
    async fn prefixes_are_case_insensitive() {
        let resolved = standalone_resolver().resolve("TAG:v1.0.0").await.unwrap();
        assert_eq!(resolved.source, RefSource::Tag("v1.0.0".into()));
        let resolved = standalone_resolver().resolve("Branch:Main").await.unwrap();
        // Only the prefix is case-folded; the ref name is kept verbatim.
        assert_eq!(resolved.source, RefSource::Branch("Main".into()));
    }

    #[tokio::test]
    async fn release_prefix_resolves_without_any_lookup() {
        for value in ["v3.1.0", "latest-stable", "previous-latest"] {
            let input = format!("release:{value}");
            let resolved = standalone_resolver().resolve(&input).await.unwrap();
            assert_eq!(resolved.source, RefSource::Release(value.into()));
            assert_eq!(resolved.git_ref, value);
            assert!(resolved.commit_sha.is_none());
        }
    }

    #[tokio::test]
    async fn invalid_commit_values_are_rejected() {
        let err = standalone_resolver()
            .resolve("commit:xyz1234")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid commit SHA"), "{err}");

        let err = standalone_resolver()
            .resolve("commit:abc12")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too short"), "{err}");
    }

    #[tokio::test]
    async fn standalone_trusts_explicit_refs_unverified() {
        let r = standalone_resolver();
        let tag = r.resolve("tag:v9.9.9").await.unwrap();
        assert_eq!(tag.source, RefSource::Tag("v9.9.9".into()));
        assert!(tag.commit_sha.is_none());

        let branch = r.resolve("branch:anything").await.unwrap();
        assert_eq!(branch.source, RefSource::Branch("anything".into()));
        assert!(branch.commit_sha.is_none());

        // A trusted commit is its own (unexpanded) commit SHA.
        let commit = r.resolve("commit:abcdef1").await.unwrap();
        assert_eq!(commit.source, RefSource::Commit("abcdef1".into()));
        assert_eq!(commit.commit_sha.as_deref(), Some("abcdef1"));
    }

    #[tokio::test]
    async fn standalone_assumes_bare_names_are_branches() {
        for input in ["feature/foo", "abcdef1"] {
            let resolved = standalone_resolver().resolve(input).await.unwrap();
            assert_eq!(resolved.source, RefSource::Branch(input.into()), "{input}");
            assert!(resolved.commit_sha.is_none());
        }
    }

    #[tokio::test]
    async fn standalone_tag_keywords_need_a_backend() {
        let err = standalone_resolver()
            .resolve("tag:latest")
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("requires a local repository or a configured remote"),
            "{err}"
        );
    }

    // =========================================================================
    // Local-repository mode (post-clone validation)
    // =========================================================================

    use crate::git::testutil::{git, git_stdout, make_repo_with_tags};

    /// Clone `origin` into a temp dir and return it. The clone has a local
    /// `main`, remote-tracking `origin/*` branches only, and all tags.
    fn local_clone(origin: &tempfile::TempDir) -> tempfile::TempDir {
        let holder = tempfile::TempDir::new().unwrap();
        git(
            holder.path(),
            &["clone", "--quiet", &file_url(origin), "."],
            "2025-01-01T00:00:00",
        );
        holder
    }

    /// Resolver in local mode over `path` (no test here resolves a PR, so
    /// the GitHub client is never used).
    fn local_resolver(path: &Path) -> RefResolver<'_> {
        RefResolver::new(leaked_client(), "owner", "repo").with_repo_path(path)
    }

    #[tokio::test]
    async fn local_resolves_explicit_and_bare_tags_with_sha() {
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        let expected = git_stdout(clone.path(), &["rev-parse", "v1.0.0"]);

        for input in ["tag:v1.0.0", "v1.0.0"] {
            let resolved = local_resolver(clone.path()).resolve(input).await.unwrap();
            assert_eq!(resolved.source, RefSource::Tag("v1.0.0".into()), "{input}");
            assert_eq!(resolved.git_ref, "v1.0.0");
            assert_eq!(resolved.commit_sha.as_deref(), Some(expected.as_str()));
        }
    }

    #[tokio::test]
    async fn local_resolves_local_branch_with_sha() {
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        let main = git_stdout(clone.path(), &["rev-parse", "main"]);

        for input in ["main", "branch:main"] {
            let resolved = local_resolver(clone.path()).resolve(input).await.unwrap();
            assert_eq!(resolved.source, RefSource::Branch("main".into()), "{input}");
            assert_eq!(resolved.commit_sha.as_deref(), Some(main.as_str()));
        }
    }

    #[tokio::test]
    async fn local_resolves_branch_that_exists_only_on_origin() {
        // A fresh clone has `feature/x` only as refs/remotes/origin/feature/x;
        // it must still resolve as a branch (checkout DWIM-creates it).
        let origin = make_remote_repo();
        let clone = local_clone(&origin);

        let resolved = local_resolver(clone.path())
            .resolve("feature/x")
            .await
            .unwrap();
        assert_eq!(resolved.source, RefSource::Branch("feature/x".into()));
        assert_eq!(resolved.git_ref, "feature/x");
    }

    #[tokio::test]
    async fn local_tag_wins_over_same_named_branch() {
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        git(
            clone.path(),
            &["branch", "v1.0.0", "main"],
            "2025-01-01T00:00:00",
        );

        let resolved = local_resolver(clone.path())
            .resolve("v1.0.0")
            .await
            .unwrap();
        assert_eq!(resolved.source, RefSource::Tag("v1.0.0".into()));
    }

    #[tokio::test]
    async fn local_expands_short_commit_sha_to_full() {
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        let full = git_stdout(clone.path(), &["rev-parse", "v1.0.0"]);
        let short = &full[..8];

        for input in [format!("commit:{short}"), short.to_string()] {
            let resolved = local_resolver(clone.path()).resolve(&input).await.unwrap();
            assert_eq!(resolved.source, RefSource::Commit(full.clone()), "{input}");
            assert_eq!(resolved.git_ref, full);
            assert_eq!(resolved.commit_sha.as_deref(), Some(full.as_str()));
        }
    }

    #[tokio::test]
    async fn local_rejects_non_commit_objects_and_unknown_shas() {
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        let tree = git_stdout(clone.path(), &["rev-parse", "HEAD^{tree}"]);

        let err = local_resolver(clone.path())
            .resolve(&format!("commit:{tree}"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is a tree, not a commit"), "{err}");

        let err = local_resolver(clone.path())
            .resolve("commit:0123456789abcdef")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn local_annotated_tags_resolve_to_the_tagged_commit() {
        // The SHA must be the commit checkout lands on — never the tag
        // object — matching what remote mode reports for the same tag.
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        let commit = git_stdout(clone.path(), &["rev-parse", "v2.0.0^{commit}"]);
        let remote = remote_resolver(&file_url(&origin))
            .resolve("tag:v2.0.0")
            .await
            .unwrap();

        for input in ["tag:v2.0.0", "v2.0.0", "tag:latest"] {
            let local = local_resolver(clone.path()).resolve(input).await.unwrap();
            assert_eq!(local.source, RefSource::Tag("v2.0.0".into()), "{input}");
            assert_eq!(
                local.commit_sha.as_deref(),
                Some(commit.as_str()),
                "{input}"
            );
        }
        assert_eq!(remote.commit_sha.as_deref(), Some(commit.as_str()));
    }

    #[tokio::test]
    async fn local_origin_only_branches_report_their_commit() {
        // A fresh clone has `feature/x` only as a remote-tracking ref; its
        // commit must still be reported (checkout DWIM-creates the branch).
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        let tip = git_stdout(clone.path(), &["rev-parse", "origin/feature/x"]);

        let resolved = local_resolver(clone.path())
            .resolve("branch:feature/x")
            .await
            .unwrap();

        assert_eq!(resolved.commit_sha.as_deref(), Some(tip.as_str()));
    }

    #[tokio::test]
    async fn local_branch_named_like_a_tag_reports_the_branch_commit() {
        // `git rev-parse v1.0.0` prefers the tag; `branch:v1.0.0` must report
        // the branch's own commit.
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        git(
            clone.path(),
            &["branch", "v1.0.0", "main"],
            "2025-01-01T00:00:00",
        );
        let main = git_stdout(clone.path(), &["rev-parse", "main"]);

        let resolved = local_resolver(clone.path())
            .resolve("branch:v1.0.0")
            .await
            .unwrap();

        assert_eq!(resolved.source, RefSource::Branch("v1.0.0".into()));
        assert_eq!(resolved.commit_sha.as_deref(), Some(main.as_str()));
    }

    #[tokio::test]
    async fn local_hex_named_branch_is_a_branch_not_a_commit() {
        // `git cat-file -t deadbeef` resolves the *branch*, which used to make
        // it pass as a commit. Only real commit objects may match a SHA.
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        assert!(
            git_stdout(clone.path(), &["rev-parse", "--disambiguate=deadbeef"]).is_empty(),
            "fixture precondition: no object starts with deadbeef"
        );
        git(
            clone.path(),
            &["branch", "deadbeef", "main"],
            "2025-01-01T00:00:00",
        );
        let main = git_stdout(clone.path(), &["rev-parse", "main"]);
        let r = local_resolver(clone.path());

        let resolved = r.resolve("deadbeef").await.unwrap();
        assert_eq!(resolved.source, RefSource::Branch("deadbeef".into()));
        assert_eq!(resolved.commit_sha.as_deref(), Some(main.as_str()));

        let err = r.resolve("commit:deadbeef").await.unwrap_err();
        assert!(
            err.to_string().contains("Commit 'deadbeef' not found"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn local_commit_wins_over_a_branch_named_like_its_prefix() {
        // A real commit object beats a hex-looking branch name: commit
        // detection runs before the branch lookup.
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        let c1 = git_stdout(clone.path(), &["rev-parse", "v1.0.0"]);
        let prefix = &c1[..8];
        git(
            clone.path(),
            &["branch", prefix, "main"],
            "2025-01-01T00:00:00",
        );

        let resolved = local_resolver(clone.path()).resolve(prefix).await.unwrap();

        assert_eq!(resolved.source, RefSource::Commit(c1.clone()));
    }

    #[tokio::test]
    async fn commit_shas_longer_than_64_chars_are_rejected_before_git_runs() {
        // 64 hex is the SHA-256 maximum; longer input made git itself abort
        // (`--disambiguate` asserts on the length) instead of "not found".
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        let too_long = "a".repeat(65);
        for resolver in [standalone_resolver(), local_resolver(clone.path())] {
            let err = resolver
                .resolve(&format!("commit:{too_long}"))
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("too long (maximum 64 characters)"),
                "{err}"
            );
        }
    }

    #[tokio::test]
    async fn local_sha_with_extra_characters_is_not_found() {
        // In a SHA-1 repo, `<sha>0` (41 chars) used to resolve to `<sha>`:
        // only objects whose name starts with the whole input may match.
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        let full = git_stdout(clone.path(), &["rev-parse", "v1.0.0"]);

        for input in [format!("{full}0"), format!("{full}{}", "0".repeat(24))] {
            let err = local_resolver(clone.path())
                .resolve(&format!("commit:{input}"))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("not found"), "{input}: {err}");
        }
    }

    #[test]
    fn pick_commit_ignores_objects_that_do_not_start_with_the_prefix() {
        let objects = [("ffff0000".to_string(), "commit".to_string())];
        let err = pick_commit("abc1234", &objects).unwrap_err().to_string();
        assert!(err.contains("Commit 'abc1234' not found"), "{err}");
        // Case-insensitive, like git: an upper-case prefix still matches.
        let objects = [("abc1234f".to_string(), "commit".to_string())];
        assert_eq!(pick_commit("ABC1234", &objects).unwrap(), "abc1234f");
    }

    #[test]
    fn pick_commit_requires_exactly_one_commit_among_the_matches() {
        let object = |oid: &str, kind: &str| (oid.to_string(), kind.to_string());

        assert_eq!(
            pick_commit("abc1234", &[object("abc1234f", "commit")]).unwrap(),
            "abc1234f"
        );
        // A tree sharing the prefix does not make a lone commit ambiguous.
        assert_eq!(
            pick_commit(
                "abc1234",
                &[object("abc1234a", "tree"), object("abc1234f", "commit")]
            )
            .unwrap(),
            "abc1234f"
        );

        let message = |candidates: &[(String, String)]| {
            pick_commit("abc1234", candidates).unwrap_err().to_string()
        };
        assert!(message(&[]).contains("Commit 'abc1234' not found"));
        assert!(message(&[object("abc1234a", "tree")]).contains("is a tree, not a commit"));
        let ambiguous = message(&[object("abc1234a", "commit"), object("abc1234b", "commit")]);
        assert!(ambiguous.contains("ambiguous"), "{ambiguous}");
        assert!(ambiguous.contains("2 commits"), "{ambiguous}");
        assert!(
            message(&[object("abc1234a", "tree"), object("abc1234b", "blob")])
                .contains("matches no commit")
        );
    }

    #[tokio::test]
    async fn local_missing_refs_fail_with_specific_errors() {
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        let r = local_resolver(clone.path());

        let err = r.resolve("tag:v9.9.9").await.unwrap_err();
        assert!(err.to_string().contains("Tag 'v9.9.9' not found"), "{err}");
        let err = r.resolve("branch:nope").await.unwrap_err();
        assert!(err.to_string().contains("Branch 'nope' not found"), "{err}");
        // With a repo available, an unknown bare name is an error, not a
        // guessed branch.
        let err = r.resolve("totally-unknown").await.unwrap_err();
        assert!(err.to_string().contains("Could not resolve"), "{err}");
    }

    #[tokio::test]
    async fn local_and_remote_tag_keywords_agree() {
        // Both backends must order tags identically (creation date, newest
        // first), or a pre-clone resolve and a post-clone resolve could pick
        // different tags for the same keyword.
        let origin = make_remote_repo();
        let clone = local_clone(&origin);
        let url = file_url(&origin);
        let expected = [
            ("tag:latest", "v2.0.0"),
            ("tag:latest-stable", "v2.0.0"),
            ("tag:previous-latest", "v2.0.0-beta1"),
            ("tag:previous-stable", "v1.0.0"),
            ("tag:LATEST", "v2.0.0"),
        ];

        for (input, tag) in expected {
            let local = local_resolver(clone.path()).resolve(input).await.unwrap();
            let remote = remote_resolver(&url).resolve(input).await.unwrap();
            assert_eq!(local.source, RefSource::Tag(tag.into()), "local {input}");
            assert_eq!(remote.source, local.source, "remote {input}");
            assert_eq!(local.input, input);
        }
    }

    #[tokio::test]
    async fn tag_keywords_report_missing_or_insufficient_tags() {
        // (tags in creation order, keyword, expected error fragment)
        let cases: [(&[&str], &str, &str); 4] = [
            (&[], "tag:latest", "No tags found"),
            (
                &["v1.0.0-beta1"],
                "tag:latest-stable",
                "No stable tags found",
            ),
            (
                &["v1.0.0"],
                "tag:previous-stable",
                "need at least 2, found 1",
            ),
            (
                &["v1.0.0"],
                "tag:previous-latest",
                "need at least 2, found 1",
            ),
        ];

        for (tags, input, fragment) in cases {
            let origin = make_repo_with_tags(tags);
            for err in [
                local_resolver(origin.path())
                    .resolve(input)
                    .await
                    .unwrap_err(),
                remote_resolver(&file_url(&origin))
                    .resolve(input)
                    .await
                    .unwrap_err(),
            ] {
                assert!(
                    err.to_string().contains(fragment),
                    "{tags:?} {input}: {err}"
                );
            }
        }
    }

    #[tokio::test]
    async fn non_keyword_tag_values_resolve_literally() {
        // A tag literally named like a near-keyword must not be mistaken for
        // one (only the four exact keywords are special).
        let origin = make_repo_with_tags(&["latest-beta"]);
        let resolved = local_resolver(origin.path())
            .resolve("tag:latest-beta")
            .await
            .unwrap();
        assert_eq!(resolved.source, RefSource::Tag("latest-beta".into()));
    }

    // =========================================================================
    // Remote mode: session caching
    // =========================================================================

    #[tokio::test]
    async fn remote_ls_remote_snapshot_is_reused_within_a_session() {
        // One resolver = one ls-remote round-trip. Proven by deleting the
        // remote after the first lookup: later lookups still succeed from
        // the cached snapshot.
        let origin = make_remote_repo();
        let url = file_url(&origin);
        let resolver = remote_resolver(&url);

        resolver.resolve("branch:main").await.unwrap();
        drop(origin);

        let resolved = resolver.resolve("feature/x").await.unwrap();
        assert_eq!(resolved.source, RefSource::Branch("feature/x".into()));
        let resolved = resolver.resolve("tag:v1.0.0").await.unwrap();
        assert_eq!(resolved.source, RefSource::Tag("v1.0.0".into()));
    }
}
