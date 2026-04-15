//! Git reference resolution.
//!
//! Automatically detects the type of a git reference (PR, branch, tag, commit)
//! from user input and resolves it to a concrete ref for checkout.

use std::path::Path;

use apvm_storage::path::BuildSource;
use tokio::process::Command;

use crate::error::{Error, Result};
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
/// Convertible to/from [`apvm_storage::path::BuildSource`] for storage operations.
///
/// # Examples
///
/// ```
/// use apvm_core::git::RefSource;
/// use apvm_storage::path::BuildSource;
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
            Self::Commit(sha) => format!("commit {}", &sha[..7.min(sha.len())]),
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
/// - `branch:main` → Force branch interpretation
/// - `commit:a1b2c3d` → Force commit interpretation
pub struct RefResolver<'a> {
    /// GitHub client for PR lookups.
    github: &'a GitHubClient,
    /// Repository owner.
    owner: &'a str,
    /// Repository name.
    repo: &'a str,
    /// Local repository path (for git commands).
    repo_path: Option<&'a Path>,
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
        }
    }

    /// Set the local repository path for git-based resolution.
    ///
    /// Required for resolving tags, branches, and commits locally.
    pub fn with_repo_path(mut self, path: &'a Path) -> Self {
        self.repo_path = Some(path);
        self
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
            "tag" => self.resolve_tag(input, value).await.map(Some),
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
                 Valid prefixes are: pr:, tag:, branch:, commit:, release:"
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
            match self.resolve_commit(input, input).await {
                Ok(resolved) => return Ok(resolved),
                Err(_) => {
                    tracing::debug!("'{input}' is not a valid commit, trying other types");
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
        }

        // 5. No local repo - try as branch by default
        if self.repo_path.is_none() {
            tracing::warn!(
                "No local repo available, assuming '{input}' is a branch. \
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
             Use explicit prefix (pr:, tag:, branch:, commit:, release:) to specify type."
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
    async fn resolve_tag(&self, input: &str, tag: &str) -> Result<ResolvedRef> {
        tracing::debug!("Resolving tag '{tag}'");

        // Validate tag exists if we have a local repo
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
        } else {
            // No local repo, trust the user
            Ok(ResolvedRef {
                input: input.to_string(),
                source: RefSource::Tag(tag.to_string()),
                git_ref: tag.to_string(),
                commit_sha: None,
            })
        }
    }

    /// Resolve a branch name.
    async fn resolve_branch(&self, input: &str, branch: &str) -> Result<ResolvedRef> {
        tracing::debug!("Resolving branch '{branch}'");

        // Validate branch exists if we have a local repo
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
        } else {
            // No local repo, trust the user
            Ok(ResolvedRef {
                input: input.to_string(),
                source: RefSource::Branch(branch.to_string()),
                git_ref: branch.to_string(),
                commit_sha: None,
            })
        }
    }

    /// Resolve a commit SHA.
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
        } else {
            // No local repo, trust the user
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
}
