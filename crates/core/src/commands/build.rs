//! Build command implementation.
//!
//! This module provides the main build command that handles building a project
//! from any git reference (PR, branch, tag, or commit) with automatic detection.

use crate::build::{BuildResult, BuildRunner};
use crate::error::Result;
use crate::git::{RefResolver, RefSource, RepoCache, ResolvedRef};
use crate::github::GitHubClient;
use crate::projects::ProjectRegistry;

/// Extended build result with git metadata.
///
/// Contains the build artifacts plus all the metadata needed
/// for storage operations (commit SHA, source type, branch name).
#[derive(Debug)]
pub struct BuildOutput {
    /// The build result containing artifacts.
    pub result: BuildResult,
    /// The resolved git reference.
    pub resolved_ref: ResolvedRef,
    /// Full commit SHA of what was built.
    pub commit: String,
    /// Short commit SHA (7 characters).
    pub commit_short: String,
    /// Branch name that was checked out.
    pub branch: String,
}

impl BuildOutput {
    /// Get the source type for storage.
    pub fn source(&self) -> &RefSource {
        &self.resolved_ref.source
    }

    /// Get a human-readable description of what was built.
    pub fn description(&self) -> String {
        format!(
            "{} @ {}",
            self.resolved_ref.source.description(),
            &self.commit_short
        )
    }
}

/// Command to build a project from any git reference.
///
/// Supports automatic detection of reference types:
/// - `123` → PR #123 (or branch if PR doesn't exist)
/// - `v1.0.0` → Tag (if exists) or branch
/// - `develop` → Branch
/// - `a1b2c3d` → Commit SHA
///
/// Explicit prefixes are also supported:
/// - `pr:123` → Force PR interpretation
/// - `tag:v1.0.0` → Force tag interpretation
/// - `branch:main` → Force branch interpretation
/// - `commit:a1b2c3d` → Force commit interpretation
pub struct BuildCommand<'a> {
    github: &'a GitHubClient,
    cache: &'a RepoCache,
    registry: &'a ProjectRegistry,
}

impl<'a> BuildCommand<'a> {
    /// Create a new build command.
    pub fn new(
        github: &'a GitHubClient,
        cache: &'a RepoCache,
        registry: &'a ProjectRegistry,
    ) -> Self {
        Self {
            github,
            cache,
            registry,
        }
    }

    /// Execute the build command with automatic ref detection.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build (e.g., "6.1.0")
    /// * `git_ref` - Git reference (PR number, branch, tag, or commit)
    /// * `variants` - Specific variants to build (empty = all)
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Build from PR #123
    /// cmd.execute("wp-rocket", "3.17.0", "123", &[]).await?;
    ///
    /// // Build from branch
    /// cmd.execute("wp-rocket", "3.17.0", "develop", &[]).await?;
    ///
    /// // Build from tag
    /// cmd.execute("wp-rocket", "3.17.0", "v3.17.0", &[]).await?;
    ///
    /// // Build specific variants from PR
    /// cmd.execute("wp-rocket", "3.17.0", "pr:456", &["starter", "pro"]).await?;
    /// ```
    ///
    /// # Returns
    ///
    /// A `BuildOutput` containing the build result and git metadata.
    pub async fn execute(
        &self,
        project: &str,
        version: &str,
        git_ref: &str,
        variants: &[&str],
    ) -> Result<BuildOutput> {
        // 1. Look up project in registry
        let project_info = self.registry.get(project)?;

        // 2. Get or clone repository (need it for ref resolution)
        let repo = self
            .cache
            .get_or_clone(&project_info.repo_url, &project_info.name, &project_info.repo)
            .await?;

        // Fetch latest refs before resolution
        repo.fetch().await?;

        // 3. Resolve the git reference
        let resolver = RefResolver::new(self.github, &project_info.owner, &project_info.repo)
            .with_repo_path(repo.path());

        let resolved = resolver.resolve(git_ref).await?;

        tracing::info!(
            "Building {} v{} from {}",
            project,
            version,
            resolved.source.description()
        );

        if variants.is_empty() {
            tracing::info!("Building all variants");
        } else {
            tracing::info!("Building variants: {}", variants.join(", "));
        }

        // 4. Prepare repository for checkout (clean state)
        // Reset FIRST to avoid checkout failures due to uncommitted changes.
        // See: https://git-scm.com/docs/git-checkout#_description
        // "git checkout refuses to switch branches if there are local modifications"
        tracing::debug!(
            "Resetting repository and switching to default branch '{}'",
            project_info.default_branch
        );
        repo.reset_hard().await?;
        repo.checkout(&project_info.default_branch).await?;
        repo.pull().await?;

        // 5. Checkout the resolved ref
        repo.checkout(&resolved.git_ref).await?;

        // 6. Get the commit SHA after checkout
        let (commit, commit_short) = repo.get_head_commit_pair().await?;

        // Determine branch name based on source type
        let branch = match &resolved.source {
            RefSource::PullRequest(_) | RefSource::Branch(_) => resolved.git_ref.clone(),
            RefSource::Tag(tag) => format!("tag/{tag}"),
            RefSource::Commit(sha) => format!("commit/{}", &sha[..7.min(sha.len())]),
        };

        tracing::debug!("Checked out {} at commit {}", resolved.git_ref, commit_short);

        // 7. Run the build using the project's builder
        let mut runner = BuildRunner::new(repo.path().to_path_buf());
        let result = runner
            .execute_build(project_info.builder.as_ref(), version, variants)
            .await?;

        Ok(BuildOutput {
            result,
            resolved_ref: resolved,
            commit,
            commit_short,
            branch,
        })
    }

    /// Execute a build from a specific PR number.
    ///
    /// This is a convenience method equivalent to `execute(project, version, "pr:{pr_number}", variants)`.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build
    /// * `pr_number` - Pull request number
    /// * `variants` - Specific variants to build (empty = all)
    pub async fn execute_pr(
        &self,
        project: &str,
        version: &str,
        pr_number: u64,
        variants: &[&str],
    ) -> Result<BuildOutput> {
        self.execute(project, version, &format!("pr:{pr_number}"), variants)
            .await
    }

    /// Execute a build from a specific branch.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build
    /// * `branch` - Branch name
    /// * `variants` - Specific variants to build (empty = all)
    pub async fn execute_branch(
        &self,
        project: &str,
        version: &str,
        branch: &str,
        variants: &[&str],
    ) -> Result<BuildOutput> {
        self.execute(project, version, &format!("branch:{branch}"), variants)
            .await
    }

    /// Execute a build from a specific tag.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build
    /// * `tag` - Tag name (e.g., "v1.0.0")
    /// * `variants` - Specific variants to build (empty = all)
    pub async fn execute_tag(
        &self,
        project: &str,
        version: &str,
        tag: &str,
        variants: &[&str],
    ) -> Result<BuildOutput> {
        self.execute(project, version, &format!("tag:{tag}"), variants)
            .await
    }

    /// Execute a build from a specific commit SHA.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build
    /// * `commit` - Commit SHA (minimum 7 characters)
    /// * `variants` - Specific variants to build (empty = all)
    pub async fn execute_commit(
        &self,
        project: &str,
        version: &str,
        commit: &str,
        variants: &[&str],
    ) -> Result<BuildOutput> {
        self.execute(project, version, &format!("commit:{commit}"), variants)
            .await
    }
}

