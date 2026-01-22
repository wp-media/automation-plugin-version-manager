//! APVM Core Library
//!
//! This library provides the core functionality for the Automation Plugin Version Manager.
//! It can be used by the CLI or external applications.
//!
//! # Architecture
//!
//! The core library follows a **composition over integration** philosophy:
//!
//! - **Core builds artifacts** - handles git, builds, version detection
//! - **Storage is separate** - consumers compose core + storage as needed
//! - **Helpers for convenience** - conversion methods eliminate boilerplate
//!
//! See [`build::plugins::VersionRequirement`] for how different projects handle versions.
//!
//! # Quick Start
//!
//! ```ignore
//! use apvm_core::{Apvm, config_io};
//! use apvm_storage::ArtifactStore;
//!
//! // Initialize with config helpers
//! let (config, paths) = config_io::init_config(None)?;
//! let apvm = Apvm::new(config)?;
//!
//! // BackWPup: version required
//! let output = apvm.build("backwpup", Some("5.1.0"), "pr:123", None).await?;
//!
//! // WP Rocket: version auto-detected from source
//! let output = apvm.build("wp-rocket", None, "pr:456", None).await?;
//!
//! // Store artifacts (optional - you choose!)
//! let store = ArtifactStore::new(paths.builds_dir().clone());
//! for artifact in output.to_source_artifacts_filtered(&store, "wp-rocket")? {
//!     store.store(&artifact)?;
//! }
//! ```

pub mod build;
pub mod commands;
pub mod config_io;
pub mod error;
pub mod git;
pub mod github;
pub mod projects;

// Re-export config types from apvm-config
pub use apvm_config::{Config, Paths, PathsBuilder};
pub use error::{Error, Result};

// Re-export key types for convenience
pub use commands::BuildOutput;
pub use git::{RefResolver, RefSource, ResolvedRef};

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
    /// Source of the resolved token (for diagnostics).
    pub token_source: Option<git::TokenSource>,
}

impl Apvm {
    /// Create a new APVM instance with the given configuration.
    ///
    /// This is the synchronous constructor that uses only the config token.
    /// For automatic token resolution from gh CLI, use [`Apvm::new_with_token_resolution`].
    pub fn new(config: Config) -> Result<Self> {
        let github = match &config.github_token {
            Some(token) => GitHubClient::new(token)?,
            None => GitHubClient::anonymous()?,
        };

        let token_source = config.github_token.as_ref().map(|_| git::TokenSource::Config);
        let cache = RepoCache::new(config.cache_dir.clone(), config.github_token.clone());
        let registry = ProjectRegistry::with_known_projects();

        Ok(Self {
            config,
            github,
            cache,
            registry,
            token_source,
        })
    }

    /// Create a new APVM instance with automatic token resolution.
    ///
    /// This will try to resolve a GitHub token from multiple sources:
    ///
    /// 1. Config file token (explicit)
    /// 2. `GITHUB_TOKEN` environment variable
    /// 3. `GH_TOKEN` environment variable
    /// 4. `gh auth token` command (gh CLI >= 2.17.0)
    /// 5. gh CLI config file (`~/.config/gh/hosts.yml`)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let apvm = Apvm::new_with_token_resolution(config).await?;
    ///
    /// if let Some(source) = &apvm.token_source {
    ///     println!("Using token from: {}", source);
    /// }
    /// ```
    pub async fn new_with_token_resolution(mut config: Config) -> Result<Self> {
        // Resolve token from multiple sources
        let resolved = git::resolve_github_token(config.github_token.as_deref()).await;

        let (github, token_source) = match &resolved {
            Some(rt) => {
                tracing::info!("Using GitHub token from {}", rt.source);
                (GitHubClient::new(&rt.token)?, Some(rt.source))
            }
            None => {
                tracing::debug!("No GitHub token found, using anonymous client");
                (GitHubClient::anonymous()?, None)
            }
        };

        // Update config with resolved token (for cache usage)
        if let Some(rt) = &resolved {
            config.github_token = Some(rt.token.clone());
        }

        let cache = RepoCache::new(config.cache_dir.clone(), config.github_token.clone());
        let registry = ProjectRegistry::with_known_projects();

        Ok(Self {
            config,
            github,
            cache,
            registry,
            token_source,
        })
    }

    /// Create an APVM instance with an empty registry.
    ///
    /// Use this when you want to inject projects manually via [`Apvm::register_project`].
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut apvm = Apvm::new_empty(config)?;
    /// apvm.register_project("my-plugin", my_project_info);
    /// ```
    pub fn new_empty(config: Config) -> Result<Self> {
        let github = match &config.github_token {
            Some(token) => GitHubClient::new(token)?,
            None => GitHubClient::anonymous()?,
        };

        let token_source = config.github_token.as_ref().map(|_| git::TokenSource::Config);
        let cache = RepoCache::new(config.cache_dir.clone(), config.github_token.clone());
        let registry = ProjectRegistry::new(); // Empty registry

        Ok(Self {
            config,
            github,
            cache,
            registry,
            token_source,
        })
    }

    /// Register a project with the registry.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use apvm_core::projects::Project;
    /// use apvm_core::build::plugins::BackWPupBuilder;
    ///
    /// let project = Project {
    ///     name: "my-plugin".to_string(),
    ///     repo_url: "https://github.com/org/my-plugin.git".to_string(),
    ///     owner: "org".to_string(),
    ///     repo: "my-plugin".to_string(),
    ///     default_branch: "main".to_string(),
    ///     builder: Box::new(BackWPupBuilder),
    /// };
    ///
    /// apvm.register_project(project);
    /// ```
    pub fn register_project(&mut self, project: projects::Project) {
        self.registry.register(project);
    }

    /// Check if a GitHub token is available.
    pub fn has_token(&self) -> bool {
        self.token_source.is_some()
    }

    /// Get the source of the current token.
    pub fn token_source(&self) -> Option<git::TokenSource> {
        self.token_source
    }

    /// Build a project from any git reference.
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
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `git_ref` - Git reference (PR number, branch, tag, or commit)
    /// * `variants` - Optional specific variants to build (None = all)
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // BackWPup: version REQUIRED
    /// apvm.build("backwpup", Some("5.1.0"), "pr:123", None).await?;
    ///
    /// // WP Rocket: version auto-detected from source
    /// apvm.build("wp-rocket", None, "pr:456", None).await?;
    ///
    /// // Other plugin: explicit version override (In case plugin support specific version and auto-detection)
    /// apvm.build("other-plugin", Some("4.17.0-custom"), "develop", None).await?;
    /// ```
    ///
    /// # Returns
    ///
    /// A `BuildOutput` containing the build result and git metadata.
    pub async fn build(
        &self,
        project: &str,
        version: Option<&str>,
        git_ref: &str,
        variants: Option<&[&str]>,
    ) -> Result<commands::BuildOutput> {
        let cmd = commands::BuildCommand::new(&self.github, &self.cache, &self.registry);
        let variants = variants.unwrap_or(&[]);

        cmd.execute(project, version, git_ref, variants).await
    }

    /// Build a project from a PR number.
    ///
    /// This is a convenience method equivalent to `build(project, version, "{pr_number}", variants)`.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `pr_number` - Pull request number
    /// * `variants` - Optional specific variants to build (None = all)
    pub async fn build_from_pr(
        &self,
        project: &str,
        version: Option<&str>,
        pr_number: u64,
        variants: Option<&[&str]>,
    ) -> Result<commands::BuildOutput> {
        self.build(project, version, &pr_number.to_string(), variants)
            .await
    }

    /// Build a project from a branch.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `branch` - Branch name
    /// * `variants` - Optional specific variants to build (None = all)
    pub async fn build_from_branch(
        &self,
        project: &str,
        version: Option<&str>,
        branch: &str,
        variants: Option<&[&str]>,
    ) -> Result<commands::BuildOutput> {
        self.build(project, version, &format!("branch:{branch}"), variants)
            .await
    }

    /// Build a project from a tag.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `tag` - Tag name (e.g., "v1.0.0")
    /// * `variants` - Optional specific variants to build (None = all)
    pub async fn build_from_tag(
        &self,
        project: &str,
        version: Option<&str>,
        tag: &str,
        variants: Option<&[&str]>,
    ) -> Result<commands::BuildOutput> {
        self.build(project, version, &format!("tag:{tag}"), variants)
            .await
    }

    /// Build a project from a specific commit SHA.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `commit` - Commit SHA (minimum 7 characters)
    /// * `variants` - Optional specific variants to build (None = all)
    pub async fn build_from_commit(
        &self,
        project: &str,
        version: Option<&str>,
        commit: &str,
        variants: Option<&[&str]>,
    ) -> Result<commands::BuildOutput> {
        self.build(project, version, &format!("commit:{commit}"), variants)
            .await
    }
}