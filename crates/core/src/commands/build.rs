//! Build command implementation.

use crate::build::{BuildOutput, BuildRunner};
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
    pub async fn execute(
        &self,
        project: &str,
        version: &str,
        pr_number: u64,
    ) -> Result<BuildOutput> {
        // 1. Look up project in registry
        let project_info = self.registry.get(project)?;

        // 2. Get PR info from GitHub
        let pr = self
            .github
            .get_pull_request(&project_info.owner, &project_info.repo, pr_number)
            .await?;

        tracing::info!("Building {} v{} from PR #{}", project, version, pr_number);
        tracing::debug!("PR: {} -> {}", pr.head_branch, pr.base_branch);

        // 3. Get or clone repository
        let repo = self.cache.get_or_clone(&project_info.repo_url, &project_info.repo)?;

        // 4. Checkout the PR's head branch
        repo.fetch()?;
        repo.checkout(&pr.head_branch)?;

        // 5. Run the build
        let runner = BuildRunner::new(repo.path().to_path_buf());
        let script = project_info.builder.build_command(version);
        runner.run(&script).await
    }
}
