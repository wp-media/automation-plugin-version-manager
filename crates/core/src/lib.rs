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
//!   degrade to a normal build, never an error; an unusable cache is
//!   reported ([`Apvm::cache_status`], plus a build warning) and picked up
//!   again once repaired.
//! - **No default paths** — consumers supply the cache directory (the CLI
//!   and Node bindings each provide their own default, `~/.apvm/cache`).
//! - **One maintenance implementation** — [`maintenance::CacheMaintenance`]
//!   performs the `apvm cache` actions (usage, clean, clear, gc, verify,
//!   repair) for every front end.
//!
//! See [`build::plugins::VersionRequirement`] for how different projects handle versions.
//!
//! # Quick Start
//!
//! ```ignore
//! use apvm_core::{Apvm, BuildRequest, WarmRequest, NullReporter};
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
//!
//! // Warm the cache without producing output: same pipeline, but nothing is
//! // delivered to an output directory. A later `build` of the same ref is then
//! // a cache hit. `WarmRequest` has no output_dir / no_cache.
//! let _ = apvm
//!     .warm_cache(WarmRequest::new("wp-rocket", "branch:develop"), &NullReporter)
//!     .await?;
//! ```

pub mod build;
mod cache_status;
pub mod commands;
pub mod config_io;
pub mod error;
pub mod git;
pub mod github;
pub mod maintenance;
pub mod projects;

// Re-export Config from apvm-config
pub use apvm_config::Config;
pub use error::{Error, Result};

// Re-export key types for convenience
pub use build::BuildContext;
pub use build::progress::{
    BuildEvent, BuildPhase, BuildStep, ClosureReporter, NullReporter, ProgressReporter,
};
pub use build::{ArtifactOrigin, ProducedArtifact, VersionOverride};
pub use cache_status::CacheStatus;
pub use commands::{BuildOutput, BuildRequest, WarmRequest};
pub use git::{BuildWorkspace, RefResolver, RefSource, ResolvedRef};
pub use maintenance::{CacheMaintenance, CleanRequest};

use std::path::Path;

use cache_status::StoreSlot;
use github::GitHubClient;
use projects::ProjectRegistry;

/// Open the artifact cache for a new instance, best-effort.
///
/// Opening up front initializes the cache directory at construction, as
/// consumers expect. A cache problem must never prevent APVM from building,
/// so a failure only leaves the slot empty (and is logged): every build,
/// warm and status call checks the cache again — see [`Apvm::cache_status`].
///
/// I/O note: a quick SQLite open + directory create, also run from the async
/// constructor; per-build checks run on `spawn_blocking`.
fn open_cache_store(config: &Config) -> StoreSlot {
    let slot = StoreSlot::default();
    slot.refresh(config);
    slot
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
    /// The shared artifact cache, re-checked before each build (see
    /// [`Apvm::cache_status`]); empty while caching is off or unusable.
    store: StoreSlot,
}

impl Apvm {
    /// Create a new APVM instance with the given configuration.
    ///
    /// This is the synchronous constructor that uses only the config token.
    /// For automatic token resolution from gh CLI, use [`Apvm::new_with_token_resolution`].
    pub fn new(config: Config) -> Result<Self> {
        let config = config_io::pin_cache_dir(config);
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
    pub async fn new_with_token_resolution(config: Config) -> Result<Self> {
        let mut config = config_io::pin_cache_dir(config);
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
        let config = config_io::pin_cache_dir(config);
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

    /// Whether builds on this instance use the artifact cache right now, and
    /// if not, why.
    ///
    /// Checks the cache again, exactly as each build does: it opens the
    /// cache when none is open — picking up a cache repaired meanwhile, by
    /// any front end or process — and replaces an open one whose database
    /// was deleted or replaced.
    /// Builds still work whatever the status; they only run uncached.
    ///
    /// Blocking (a stat, plus a SQLite open when nothing usable is open):
    /// from async code, call it inside `tokio::task::spawn_blocking`.
    pub fn cache_status(&self) -> CacheStatus {
        self.store.refresh(&self.config).1
    }

    /// Whether the artifact cache is active for this instance: shorthand for
    /// `self.cache_status().is_active()`, with the same check and cost.
    pub fn cache_active(&self) -> bool {
        self.cache_status().is_active()
    }

    /// The configured artifact cache directory (regardless of whether the
    /// store opened successfully), made absolute at construction
    /// ([`config_io::pin_cache_dir`]).
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
        // `false` = deliver artifacts to the output directory (a normal build).
        self.command().await.execute(request, reporter, false).await
    }

    /// Warm the artifact cache for a project without producing any output.
    ///
    /// Runs the **same pipeline** as [`Apvm::build`] — resolve the ref, then
    /// serve/partially-reuse from the cache, clone + build the missing
    /// variants, or download release assets — and stores everything into the
    /// cache. The only difference is that **nothing is delivered to an output
    /// directory**: this primes the cache so that a later [`Apvm::build`] of the
    /// same reference is a fast cache hit.
    ///
    /// The request type is [`WarmRequest`], deliberately smaller than
    /// [`BuildRequest`]: there is no output directory (nothing is delivered), no
    /// cache-bypass (warming *is* a cache operation), and no version-strictness
    /// toggle (warming always pins the requested version exactly). See
    /// [`WarmRequest`] for the rationale.
    ///
    /// # Returns
    ///
    /// A [`BuildOutput`] describing what was cached. Each artifact's
    /// [`origin`](build::ProducedArtifact::origin) distinguishes what was
    /// **already cached** ([`ArtifactOrigin::Cache`]) from what had to be
    /// **built** ([`ArtifactOrigin::Built`]) or **downloaded**
    /// ([`ArtifactOrigin::Downloaded`]) — all of which are cached once this
    /// returns. When the cache is active, artifact paths point at their
    /// canonical locations inside the cache (never an output directory) —
    /// unless the cache refuses the write (e.g. a read-only database or a
    /// full disk), which is reported as a [`BuildEvent::Warning`].
    ///
    /// # Note
    ///
    /// If the cache is disabled (`Config::cache_enabled == false`) or unusable
    /// (see [`Apvm::cache_status`]), the pipeline still runs but caches
    /// nothing; a [`BuildEvent::Warning`] saying why is emitted to the
    /// reporter in that case, and because there is no cache to point at, the
    /// returned artifact paths are not durable (they reference a temporary
    /// workspace that is cleaned up). Inspect [`Apvm::cache_status`] up front
    /// if you need warming to be meaningful.
    pub async fn warm_cache(
        &self,
        request: WarmRequest,
        reporter: &dyn build::progress::ProgressReporter,
    ) -> Result<commands::BuildOutput> {
        // `true` = warm-only: run the whole pipeline but deliver nothing to an
        // output directory. `WarmRequest::into_build_request` supplies the
        // fixed `output_dir = ""`, `no_cache = false`.
        self.command()
            .await
            .execute(request.into_build_request(), reporter, true)
            .await
    }

    /// A build command over the cache as it is now: re-checked (see
    /// [`Apvm::cache_status`]), with its status, so an unusable cache is
    /// reported as a warning by the build.
    async fn command(&self) -> commands::BuildCommand<'_> {
        let (store, status) = self.store.refresh_async(&self.config).await;
        commands::BuildCommand::new(&self.github, &self.registry, &self.config, store)
            .with_cache_status(status)
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

    /// `path` (absolute) relative to the current directory — `..` up to the
    /// root, then `path` — so no test changes the process-wide cwd. `None`
    /// when no relative path exists (another drive on Windows).
    fn relative_to_cwd(path: &Path) -> Option<std::path::PathBuf> {
        use std::path::{Component, PathBuf};
        let cwd = std::env::current_dir().unwrap();
        let prefix = |p: &Path| match p.components().next() {
            Some(Component::Prefix(prefix)) => Some(prefix.as_os_str().to_owned()),
            _ => None,
        };
        if prefix(&cwd) != prefix(path) {
            return None;
        }
        let normal = |c: &Component| matches!(c, Component::Normal(_));
        let mut relative: PathBuf = cwd.components().filter(normal).map(|_| "..").collect();
        relative.extend(path.components().filter(normal));
        Some(relative)
    }

    #[tokio::test]
    async fn relative_cache_dir_is_pinned_and_its_store_kept() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("cache");
        let Some(relative) = relative_to_cwd(&dir) else {
            return;
        };
        assert!(relative.is_relative());
        let apvm = Apvm::new(Config::new(relative)).unwrap();

        // Pinned at construction: a later `chdir` cannot move the cache.
        assert!(
            apvm.cache_dir().is_absolute(),
            "{}",
            apvm.cache_dir().display()
        );
        assert_eq!(
            std::fs::canonicalize(apvm.cache_dir()).unwrap(),
            std::fs::canonicalize(&dir).unwrap()
        );
        // Re-checks keep the open store instead of reopening it each time.
        let first = apvm.store.current().expect("store open");
        assert!(apvm.cache_status().is_active());
        let second = apvm.store.current().expect("store open");
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "store was reopened"
        );
    }

    #[tokio::test]
    async fn cache_disabled_opens_no_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::new(dir.path().join("cache")).set_cache_enabled(false);
        let apvm = Apvm::new(config).unwrap();
        assert!(!apvm.cache_active(), "disabled cache must not open a store");
        assert_eq!(apvm.cache_status(), CacheStatus::Disabled);
        assert!(!dir.path().join("cache").exists(), "nothing is created");
    }

    #[tokio::test]
    async fn new_survives_unopenable_cache_dir() {
        // A regular file where a directory is expected: `create_dir_all` on
        // "<file>/cache" fails, so the store cannot open — but Apvm::new must
        // still succeed with caching simply inactive. (The async constructor
        // shares the same `open_cache_store` helper.)
        let file = tempfile::NamedTempFile::new().unwrap();
        let bad_cache_dir = file.path().join("cache");
        let config = Config::new(bad_cache_dir.clone()); // enabled by default
        let apvm = Apvm::new(config).expect("Apvm::new must not fail on a cache-open error");
        assert!(
            !apvm.cache_active(),
            "a broken cache dir must leave the store None"
        );
        // The reason is reported: the path, then the I/O cause (on Unix
        // "cannot access store directory <path>: Not a directory").
        let CacheStatus::Unavailable { details } = apvm.cache_status() else {
            panic!("expected Unavailable, got {:?}", apvm.cache_status());
        };
        let path = bad_cache_dir.display().to_string();
        let cause = details
            .split_once(&format!("{path}: "))
            .map(|(_, cause)| cause);
        assert!(cause.is_some_and(|c| !c.is_empty()), "{details}");
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
