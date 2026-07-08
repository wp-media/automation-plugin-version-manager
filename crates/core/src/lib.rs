//! APVM Core Library
//!
//! This library provides the core functionality for the Automation Plugin Version Manager.
//! It can be used by the CLI or external applications.
//!
//! # Architecture
//!
//! - **Core builds artifacts** — handles git, builds, version detection.
//! - **Caching is built in** — when `Config::cache_enabled` is set (the
//!   default), builds and release downloads are served from and stored to
//!   the `apvm-storage` cache at `Config::cache_dir`. Cache failures always
//!   degrade to a normal build, never an error.
//! - **No default paths** — consumers supply the cache directory (the CLI
//!   and Node bindings each provide their own default, `~/.apvm/cache`).
//!
//! See [`build::plugins::VersionRequirement`] for how different projects handle versions.
//!
//! # Quick Start
//!
//! ```ignore
//! use apvm_core::{Apvm, BuildRequest, NullReporter};
//! use apvm_config::Config;
//! use std::path::PathBuf;
//!
//! // Create config with an explicit cache directory
//! let config = Config::new(PathBuf::from("/var/lib/myapp/cache"));
//! let apvm = Apvm::new(config)?;
//!
//! // BackWPup: version required, specify output directory
//! let request = BuildRequest::new("backwpup", "pr:123", "/output")
//!     .version(Some("5.1.0".to_string()));
//! let output = apvm.build(request, &NullReporter).await?;
//!
//! // WP Rocket: version auto-detected from source
//! let output = apvm
//!     .build(BuildRequest::new("wp-rocket", "pr:456", "/output"), &NullReporter)
//!     .await?;
//!
//! // Artifacts are delivered to the output directory. When caching is enabled
//! // (the default), the build is served from / written to the cache configured
//! // via `config.cache_dir`.
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
pub use build::BuildContext;
pub use build::progress::{
    BuildEvent, BuildPhase, BuildStep, ClosureReporter, NullReporter, ProgressReporter,
};
pub use build::{ArtifactOrigin, ProducedArtifact};
pub use commands::{BuildOutput, BuildRequest};
pub use git::{BuildWorkspace, RefResolver, RefSource, ResolvedRef};

use std::path::Path;
use std::sync::Arc;

use apvm_storage::ArtifactStore;
use github::GitHubClient;
use projects::ProjectRegistry;

/// Open the artifact cache for a configuration, best-effort.
///
/// Returns `None` when caching is disabled **or** the store cannot be opened
/// (e.g. an unwritable cache directory or a corrupt database). A cache problem
/// must never prevent APVM from building, so the failure is logged and
/// caching is simply inactive for the session.
///
/// The store is opened once and shared (via `Arc`) across every build on the
/// instance, matching the concurrent-build model of the Node bindings.
///
/// I/O note: this performs a quick, one-time SQLite open + directory create.
/// It is called from the async constructor too; the cost is negligible at
/// startup and per-build cache operations (Phase 4) run on `spawn_blocking`.
fn open_cache_store(config: &Config) -> Option<Arc<ArtifactStore>> {
    if !config.cache_enabled {
        tracing::debug!("artifact cache disabled by configuration");
        return None;
    }
    match ArtifactStore::open(&config.cache_dir) {
        Ok(store) => {
            tracing::debug!(cache_dir = %config.cache_dir.display(), "artifact cache ready");
            Some(Arc::new(store))
        }
        Err(e) => {
            tracing::warn!(
                cache_dir = %config.cache_dir.display(),
                error = %e,
                "failed to open artifact cache; caching disabled for this session"
            );
            None
        }
    }
}

/// Selects which GitHub Release to download.
///
/// Used with [`Apvm::download_release`] to specify a concrete release tag
/// or a dynamic keyword that resolves to the latest/previous release.
///
/// # Keyword resolution
///
/// | Variant            | GitHub API endpoint                                       |
/// |--------------------|-----------------------------------------------------------|
/// | `LatestStable`     | `GET /repos/{owner}/{repo}/releases/latest`               |
/// | `PreviousStable`   | Second non-prerelease, non-draft from list releases       |
/// | `Latest`           | First non-draft from list releases (includes prereleases) |
/// | `PreviousLatest`   | Second non-draft from list releases                       |
///
/// References:
/// - <https://docs.github.com/en/rest/releases/releases#get-the-latest-release>
/// - <https://docs.github.com/en/rest/releases/releases#list-releases>
///
/// # Examples
///
/// ```ignore
/// use apvm_core::{Apvm, ReleaseSelector, NullReporter};
///
/// // Download a specific release by tag
/// apvm.download_release("backwpup", ReleaseSelector::Tag("5.6.8"), None, "/output", &NullReporter).await?;
///
/// // Download the latest stable release
/// apvm.download_release("backwpup", ReleaseSelector::LatestStable, None, "/output", &NullReporter).await?;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseSelector<'a> {
    /// A specific release tag (e.g., `"v5.6.8"`, `"5.6.8"`).
    Tag(&'a str),
    /// The latest stable release (non-prerelease, non-draft).
    LatestStable,
    /// The previous stable release (second non-prerelease, non-draft).
    PreviousStable,
    /// The very latest non-draft release, including prereleases.
    Latest,
    /// The previous non-draft release (second in the list).
    PreviousLatest,
}

impl<'a> ReleaseSelector<'a> {
    /// Convert to the `release:xxx` git ref string understood by the build pipeline.
    fn to_git_ref(self) -> String {
        match self {
            Self::Tag(tag) => format!("release:{tag}"),
            Self::LatestStable => "release:latest-stable".to_string(),
            Self::PreviousStable => "release:previous-stable".to_string(),
            Self::Latest => "release:latest".to_string(),
            Self::PreviousLatest => "release:previous-latest".to_string(),
        }
    }
}

impl std::fmt::Display for ReleaseSelector<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tag(tag) => write!(f, "release tag '{tag}'"),
            Self::LatestStable => write!(f, "latest stable release"),
            Self::PreviousStable => write!(f, "previous stable release"),
            Self::Latest => write!(f, "latest release"),
            Self::PreviousLatest => write!(f, "previous latest release"),
        }
    }
}

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
    /// Shared artifact cache, or `None` when caching is disabled or the store
    /// could not be opened. Opened once and reused across builds.
    store: Option<Arc<ArtifactStore>>,
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

        let token_source = config
            .github_token
            .as_ref()
            .map(|_| git::TokenSource::Config);
        let registry = ProjectRegistry::with_known_projects();
        let store = open_cache_store(&config);

        Ok(Self {
            config,
            github,
            registry,
            token_source,
            store,
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
        let store = open_cache_store(&config);

        Ok(Self {
            config,
            github,
            registry,
            token_source,
            store,
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

        let token_source = config
            .github_token
            .as_ref()
            .map(|_| git::TokenSource::Config);
        let registry = ProjectRegistry::new(); // Empty registry
        let store = open_cache_store(&config);

        Ok(Self {
            config,
            github,
            registry,
            token_source,
            store,
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
    ///     is_private: false,
    ///     has_releases: false,
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

    /// Whether the artifact cache is active for this instance.
    ///
    /// `true` only when caching is enabled *and* the store opened
    /// successfully. `false` when disabled by config or when the store could
    /// not be opened (builds still work — they just don't cache).
    pub fn cache_active(&self) -> bool {
        self.store.is_some()
    }

    /// The configured artifact cache directory (regardless of whether the
    /// store opened successfully).
    pub fn cache_dir(&self) -> &Path {
        &self.config.cache_dir
    }

    /// Build a project from a [`BuildRequest`].
    ///
    /// This is the primary entry point; the `build_from_*` helpers are thin
    /// wrappers over it. See [`BuildRequest`] for the available options
    /// (version pin, variants, cache control).
    ///
    /// # Returns
    ///
    /// A [`BuildOutput`] containing the build result and git metadata.
    pub async fn build(
        &self,
        request: BuildRequest,
        reporter: &dyn build::progress::ProgressReporter,
    ) -> Result<commands::BuildOutput> {
        let cmd = commands::BuildCommand::new(
            &self.github,
            &self.registry,
            &self.config,
            self.store.clone(),
        );
        cmd.execute(request, reporter).await
    }

    /// Shared helper for the `build_from_*` conveniences: assemble a
    /// [`BuildRequest`] from a concrete git ref and run it.
    async fn build_ref(
        &self,
        project: &str,
        version: Option<&str>,
        git_ref: String,
        variants: Option<&[&str]>,
        output_dir: impl AsRef<Path>,
        reporter: &dyn build::progress::ProgressReporter,
    ) -> Result<commands::BuildOutput> {
        let request = BuildRequest::new(project, git_ref, output_dir.as_ref())
            .version(version.map(str::to_string))
            .variants(
                variants
                    .map(|v| v.iter().map(|s| s.to_string()).collect())
                    .unwrap_or_default(),
            );
        self.build(request, reporter).await
    }

    /// Build a project from a PR number.
    ///
    /// Convenience wrapper over [`Apvm::build`] with a `pr:{pr_number}` ref
    /// and default cache behavior.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `pr_number` - Pull request number
    /// * `variants` - Optional specific variants to build (None = all)
    /// * `output_dir` - Directory where build artifacts will be placed
    /// * `reporter` - Progress reporter for receiving build events.
    ///   Use [`NullReporter`] to discard all events.
    pub async fn build_from_pr(
        &self,
        project: &str,
        version: Option<&str>,
        pr_number: u64,
        variants: Option<&[&str]>,
        output_dir: impl AsRef<Path>,
        reporter: &dyn build::progress::ProgressReporter,
    ) -> Result<commands::BuildOutput> {
        self.build_ref(
            project,
            version,
            format!("pr:{pr_number}"),
            variants,
            output_dir,
            reporter,
        )
        .await
    }

    /// Build a project from a branch.
    ///
    /// Convenience wrapper over [`Apvm::build`] with a `branch:{branch}` ref.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `branch` - Branch name
    /// * `variants` - Optional specific variants to build (None = all)
    /// * `output_dir` - Directory where build artifacts will be placed
    /// * `reporter` - Progress reporter for receiving build events.
    ///   Use [`NullReporter`] to discard all events.
    pub async fn build_from_branch(
        &self,
        project: &str,
        version: Option<&str>,
        branch: &str,
        variants: Option<&[&str]>,
        output_dir: impl AsRef<Path>,
        reporter: &dyn build::progress::ProgressReporter,
    ) -> Result<commands::BuildOutput> {
        self.build_ref(
            project,
            version,
            format!("branch:{branch}"),
            variants,
            output_dir,
            reporter,
        )
        .await
    }

    /// Build a project from a tag.
    ///
    /// Convenience wrapper over [`Apvm::build`] with a `tag:{tag}` ref.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `tag` - Tag name (e.g., "v1.0.0")
    /// * `variants` - Optional specific variants to build (None = all)
    /// * `output_dir` - Directory where build artifacts will be placed
    /// * `reporter` - Progress reporter for receiving build events.
    ///   Use [`NullReporter`] to discard all events.
    pub async fn build_from_tag(
        &self,
        project: &str,
        version: Option<&str>,
        tag: &str,
        variants: Option<&[&str]>,
        output_dir: impl AsRef<Path>,
        reporter: &dyn build::progress::ProgressReporter,
    ) -> Result<commands::BuildOutput> {
        self.build_ref(
            project,
            version,
            format!("tag:{tag}"),
            variants,
            output_dir,
            reporter,
        )
        .await
    }

    /// Build a project from a specific commit SHA.
    ///
    /// Convenience wrapper over [`Apvm::build`] with a `commit:{commit}` ref.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `commit` - Commit SHA (minimum 7 characters)
    /// * `variants` - Optional specific variants to build (None = all)
    /// * `output_dir` - Directory where build artifacts will be placed
    /// * `reporter` - Progress reporter for receiving build events.
    ///   Use [`NullReporter`] to discard all events.
    pub async fn build_from_commit(
        &self,
        project: &str,
        version: Option<&str>,
        commit: &str,
        variants: Option<&[&str]>,
        output_dir: impl AsRef<Path>,
        reporter: &dyn build::progress::ProgressReporter,
    ) -> Result<commands::BuildOutput> {
        self.build_ref(
            project,
            version,
            format!("commit:{commit}"),
            variants,
            output_dir,
            reporter,
        )
        .await
    }

    /// Download pre-built assets from a GitHub Release.
    ///
    /// This bypasses the clone → build pipeline entirely: release assets are
    /// downloaded directly from GitHub. Use [`ReleaseSelector`] to specify
    /// an exact tag or a dynamic keyword (latest, previous, etc.).
    ///
    /// The version is always derived from the release tag — there is no
    /// version parameter because release assets are pre-built at a fixed
    /// version embedded in the tag name.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `selector` - Which release to download (specific tag or keyword)
    /// * `variants` - Optional specific variants to download (None = all)
    /// * `output_dir` - Directory where downloaded assets will be placed
    /// * `reporter` - Progress reporter for receiving download events.
    ///   Use [`NullReporter`] to discard all events.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use apvm_core::{Apvm, ReleaseSelector, NullReporter};
    ///
    /// // Download a specific release
    /// apvm.download_release("backwpup", ReleaseSelector::Tag("5.6.8"), None, "/output", &NullReporter).await?;
    ///
    /// // Download the latest stable release
    /// apvm.download_release("backwpup", ReleaseSelector::LatestStable, None, "/output", &NullReporter).await?;
    ///
    /// // Download the latest release (including prereleases)
    /// apvm.download_release("backwpup", ReleaseSelector::Latest, None, "/output", &NullReporter).await?;
    ///
    /// // Download specific variants only
    /// apvm.download_release("backwpup", ReleaseSelector::LatestStable, Some(&["free", "pro-en"]), "/output", &NullReporter).await?;
    /// ```
    pub async fn download_release(
        &self,
        project: &str,
        selector: ReleaseSelector<'_>,
        variants: Option<&[&str]>,
        output_dir: impl AsRef<Path>,
        reporter: &dyn build::progress::ProgressReporter,
    ) -> Result<commands::BuildOutput> {
        self.build_ref(
            project,
            None,
            selector.to_git_ref(),
            variants,
            output_dir,
            reporter,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Artifact cache lifecycle (Apvm::new store opening — best-effort)
    // =========================================================================

    // `Apvm::new` builds an octocrab client, which requires a Tokio reactor,
    // hence `#[tokio::test]`. No network is performed — only client + store
    // construction — so these stay fast and offline.

    #[tokio::test]
    async fn cache_enabled_valid_dir_opens_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::new(dir.path().join("cache")); // enabled by default
        let apvm = Apvm::new(config).unwrap();
        assert!(
            apvm.cache_active(),
            "store should open for a writable cache dir"
        );
        assert!(apvm.cache_dir().ends_with("cache"));
    }

    #[tokio::test]
    async fn cache_disabled_opens_no_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::new(dir.path().join("cache")).set_cache_enabled(false);
        let apvm = Apvm::new(config).unwrap();
        assert!(!apvm.cache_active(), "disabled cache must not open a store");
    }

    #[tokio::test]
    async fn new_survives_unopenable_cache_dir() {
        // A regular file where a directory is expected: `create_dir_all` on
        // "<file>/cache" fails, so the store cannot open — but Apvm::new must
        // still succeed with caching simply inactive. (The async constructor
        // shares the same `open_cache_store` helper.)
        let file = tempfile::NamedTempFile::new().unwrap();
        let bad_cache_dir = file.path().join("cache");
        let config = Config::new(bad_cache_dir); // enabled by default
        let apvm = Apvm::new(config).expect("Apvm::new must not fail on a cache-open error");
        assert!(
            !apvm.cache_active(),
            "a broken cache dir must leave the store None"
        );
    }

    #[test]
    fn release_selector_tag_to_git_ref() {
        assert_eq!(ReleaseSelector::Tag("5.6.8").to_git_ref(), "release:5.6.8");
        assert_eq!(
            ReleaseSelector::Tag("v1.0.0").to_git_ref(),
            "release:v1.0.0"
        );
    }

    #[test]
    fn release_selector_keywords_to_git_ref() {
        assert_eq!(
            ReleaseSelector::LatestStable.to_git_ref(),
            "release:latest-stable"
        );
        assert_eq!(
            ReleaseSelector::PreviousStable.to_git_ref(),
            "release:previous-stable"
        );
        assert_eq!(ReleaseSelector::Latest.to_git_ref(), "release:latest");
        assert_eq!(
            ReleaseSelector::PreviousLatest.to_git_ref(),
            "release:previous-latest"
        );
    }

    #[test]
    fn release_selector_display() {
        assert_eq!(
            ReleaseSelector::Tag("5.6.8").to_string(),
            "release tag '5.6.8'"
        );
        assert_eq!(
            ReleaseSelector::LatestStable.to_string(),
            "latest stable release"
        );
        assert_eq!(
            ReleaseSelector::PreviousStable.to_string(),
            "previous stable release"
        );
        assert_eq!(ReleaseSelector::Latest.to_string(), "latest release");
        assert_eq!(
            ReleaseSelector::PreviousLatest.to_string(),
            "previous latest release"
        );
    }

    #[test]
    fn release_selector_equality() {
        assert_eq!(ReleaseSelector::LatestStable, ReleaseSelector::LatestStable);
        assert_ne!(ReleaseSelector::LatestStable, ReleaseSelector::Latest);
        assert_eq!(ReleaseSelector::Tag("v1.0"), ReleaseSelector::Tag("v1.0"));
        assert_ne!(ReleaseSelector::Tag("v1.0"), ReleaseSelector::Tag("v2.0"));
    }

    #[test]
    fn release_selector_is_copy() {
        let selector = ReleaseSelector::LatestStable;
        let copied = selector; // Copy
        assert_eq!(selector, copied); // Both still usable
    }
}
