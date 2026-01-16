//! APVM Core Library
//!
//! This library provides the core functionality for the Automation Plugin Version Manager.
//! It can be used by the CLI or external applications.

pub mod build;
pub mod commands;
pub mod error;
pub mod git;
pub mod github;
pub mod projects;

// Re-export config types from apvm-config
pub use apvm_config::{Config, Paths, PathsBuilder};
pub use error::{Error, Result};

use git::RepoCache;
use github::GitHubClient;
use projects::ProjectRegistry;

/// Main APVM instance that orchestrates all operations.
pub struct Apvm {
    /// Application configuration.
    pub config: Config,
    /// GitHub API client.
    pub github: GitHubClient,
    /// Repository cache.
    pub cache: RepoCache,
    /// Project registry.
    pub registry: ProjectRegistry,
}

impl Apvm {
    /// Create a new APVM instance with the given configuration.
    pub fn new(config: Config) -> Result<Self> {
        let github = match &config.github_token {
            Some(token) => GitHubClient::new(token)?,
            None => GitHubClient::anonymous()?,
        };

        let cache = RepoCache::new(config.cache_dir.clone());
        let registry = ProjectRegistry::new();

        Ok(Self {
            config,
            github,
            cache,
            registry,
        })
    }

    /// Build a project from a PR.
    ///
    /// Returns the build result containing produced artifacts.
    pub async fn build_from_pr(
        &self,
        project: &str,
        version: &str,
        pr_number: u64,
        variants: Option<&[&str]>,
    ) -> Result<build::BuildResult> {
        // Now we pass references - no new instances created
        let cmd = commands::BuildCommand::new(
            &self.github,
            &self.cache,
            &self.registry,
        );
        let variants = variants.unwrap_or(&[]);

        cmd.execute(project, version, pr_number, variants).await
    }
}