//! Build command implementation.

use crate::build::{BuildResult, BuildRunner};
use crate::error::Result;
use crate::git::RepoCache;
use crate::github::GitHubClient;
use crate::projects::ProjectRegistry;

/// Command to build a project from a PR.
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

    /// Execute the build command.
    ///
    /// # Arguments
    /// * `project` - Project name from registry
    /// * `version` - Version to build
    /// * `pr_number` - Pull request number
    /// * `variants` - Specific variants to build (empty = all)
    ///
    /// # Returns
    ///
    /// The build result containing artifact information.
    pub async fn execute(
        &self,
        project: &str,
        version: &str,
        pr_number: u64,
        variants: &[&str],
    ) -> Result<BuildResult> {
        // 1. Look up project in registry
        let project_info = self.registry.get(project)?;

        // 2. Get PR info from GitHub
        let pr = self
            .github
            .get_pull_request(&project_info.owner, &project_info.repo, pr_number)
            .await?;

        tracing::info!("Building {} v{} from PR #{}", project, version, pr_number);
        tracing::debug!("PR: {} -> {}", pr.head_branch, pr.base_branch);

        if variants.is_empty() {
            tracing::info!("Building all variants");
        } else {
            tracing::info!("Building variants: {}", variants.join(", "));
        }

        // 3. Get or clone repository
        let repo = self.cache.get_or_clone(&project_info.repo_url, &project_info.repo)?;

        // 4. Checkout the PR's head branch
        repo.fetch()?;
        repo.checkout(&pr.head_branch)?;

        // 5. Run the build using the project's builder
        let mut runner = BuildRunner::new(repo.path().to_path_buf());
        runner
            .execute_build(project_info.builder.as_ref(), version, variants)
            .await
    }
}
