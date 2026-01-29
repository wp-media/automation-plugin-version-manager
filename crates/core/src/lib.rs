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
//! - **No path conventions** - consumers decide directory layout
//!
//! See [`build::plugins::VersionRequirement`] for how different projects handle versions.
//!
//! # Quick Start
//!
//! ```ignore
//! use apvm_core::Apvm;
//! use apvm_config::Config;
//! use apvm_storage::ArtifactStore;
//! use std::path::PathBuf;
//!
//! // Create config with explicit paths
//! let config = Config::new(PathBuf::from("/var/lib/myapp/builds"));
//! let apvm = Apvm::new(config)?;
//!
//! // BackWPup: version required, specify output directory
//! let output = apvm.build("backwpup", Some("5.1.0"), "pr:123", None, "/output").await?;
//!
//! // WP Rocket: version auto-detected from source
//! let output = apvm.build("wp-rocket", None, "pr:456", None, "/output").await?;
//!
//! // Store artifacts (optional - you choose!)
//! let store = ArtifactStore::new(PathBuf::from("/var/lib/myapp/builds"));
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

// Re-export Config from apvm-config
pub use apvm_config::Config;
pub use error::{Error, Result};

// Re-export key types for convenience
pub use commands::BuildOutput;
pub use git::{BuildWorkspace, RefResolver, RefSource, ResolvedRef};

use github::GitHubClient;
use projects::ProjectRegistry;

/// Main APVM instance that orchestrates all operations.
pub struct Apvm {
    /// Application configuration.
    pub config: Config,
    /// GitHub API client.
    pub github: GitHubClient,
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
        let registry = ProjectRegistry::with_known_projects();

        Ok(Self {
            config,
            github,
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

        let registry = ProjectRegistry::with_known_projects();

        Ok(Self {
            config,
            github,
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
        let registry = ProjectRegistry::new(); // Empty registry

        Ok(Self {
            config,
            github,
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
    /// * `output_dir` - Directory where build artifacts will be placed
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // BackWPup: version REQUIRED
    /// apvm.build("backwpup", Some("5.1.0"), "pr:123", None, "/output").await?;
    ///
    /// // WP Rocket: version auto-detected from source
    /// apvm.build("wp-rocket", None, "pr:456", None, "/output").await?;
    ///
    /// // Other plugin: explicit version override (In case plugin support specific version and auto-detection)
    /// apvm.build("other-plugin", Some("4.17.0-custom"), "develop", None, "/output").await?;
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
        output_dir: impl AsRef<std::path::Path>,
    ) -> Result<commands::BuildOutput> {
        let cmd = commands::BuildCommand::new(&self.github, &self.registry, &self.config);
        let variants = variants.unwrap_or(&[]);

        cmd.execute(project, version, git_ref, variants, output_dir).await
    }

    /// Build a project from a PR number.
    ///
    /// This is a convenience method equivalent to `build(project, version, "{pr_number}", variants, output_dir)`.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `pr_number` - Pull request number
    /// * `variants` - Optional specific variants to build (None = all)
    /// * `output_dir` - Directory where build artifacts will be placed
    pub async fn build_from_pr(
        &self,
        project: &str,
        version: Option<&str>,
        pr_number: u64,
        variants: Option<&[&str]>,
        output_dir: impl AsRef<std::path::Path>,
    ) -> Result<commands::BuildOutput> {
        self.build(project, version, &pr_number.to_string(), variants, output_dir)
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
    /// * `output_dir` - Directory where build artifacts will be placed
    pub async fn build_from_branch(
        &self,
        project: &str,
        version: Option<&str>,
        branch: &str,
        variants: Option<&[&str]>,
        output_dir: impl AsRef<std::path::Path>,
    ) -> Result<commands::BuildOutput> {
        self.build(project, version, &format!("branch:{branch}"), variants, output_dir)
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
    /// * `output_dir` - Directory where build artifacts will be placed
    pub async fn build_from_tag(
        &self,
        project: &str,
        version: Option<&str>,
        tag: &str,
        variants: Option<&[&str]>,
        output_dir: impl AsRef<std::path::Path>,
    ) -> Result<commands::BuildOutput> {
        self.build(project, version, &format!("tag:{tag}"), variants, output_dir)
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
    /// * `output_dir` - Directory where build artifacts will be placed
    pub async fn build_from_commit(
        &self,
        project: &str,
        version: Option<&str>,
        commit: &str,
        variants: Option<&[&str]>,
        output_dir: impl AsRef<std::path::Path>,
    ) -> Result<commands::BuildOutput> {
        self.build(project, version, &format!("commit:{commit}"), variants, output_dir)
            .await
    }
}