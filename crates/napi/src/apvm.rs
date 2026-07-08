//! Main APVM class exposed to Node.js.
//!
//! Provides the primary interface for building WordPress plugins from
//! JavaScript/TypeScript. All build operations are async and return
//! Promises that resolve with [`JsBuildOutput`].
//!
//! # Architecture
//!
//! ```text
//! ┌────────────────┐     ┌──────────────┐     ┌──────────────┐
//! │  JS Consumer   │────►│   JsApvm     │────►│  apvm_core   │
//! │  (TypeScript)  │     │  (N-API)     │     │   ::Apvm     │
//! └────────────────┘     └──────────────┘     └──────────────┘
//!       Promises            bridges              Rust async
//! ```
//!
//! The `JsApvm` wraps `apvm_core::Apvm` inside an `Arc` so that multiple
//! concurrent build operations can safely share the same instance.

use std::sync::Arc;

use napi::threadsafe_function::ThreadsafeFunction;
use napi_derive::napi;

use crate::config::ApvmConfig;
use crate::error::core_error_to_napi;
use crate::progress::JsProgressReporter;
use crate::types::{BuildOptions, JsBuildEvent, JsBuildOutput, JsReleaseSelector};

/// Main APVM instance for building WordPress plugins.
///
/// Create an instance using `Apvm.create()` or `Apvm.createWithTokenResolution()`,
/// then call the `build()` method (or the convenience methods `buildFromPr()`,
/// `buildFromBranch()`, `buildFromTag()`, `buildFromCommit()`) to produce
/// plugin artifacts.
///
/// The instance is safe to use concurrently — you can fire multiple `build()`
/// calls in parallel and they will run independently on the tokio runtime.
///
/// # TypeScript
///
/// ```typescript
/// import { Apvm } from 'apvm-napi';
///
/// // Minimal — no config needed, cache defaults to ~/.apvm/cache
/// const apvm = await Apvm.create({});
///
/// // Create with explicit cache dir and token
/// const apvm = await Apvm.create({
///   cacheDir: '/var/lib/apvm/cache',
///   githubToken: 'ghp_xxxxxxxxxxxx',
/// });
///
/// // Or with automatic token resolution (tries env vars, gh CLI, etc.)
/// const apvm = await Apvm.createWithTokenResolution({});
///
/// // Build WP Rocket from a PR
/// const output = await apvm.build({
///   project: 'wp-rocket',
///   gitRef: 'pr:456',
///   outputDir: '/tmp/output',
///   onProgress: (err, event) => {
///     if (err || !event) return;
///     console.log(event.type, event.message);
///   },
/// });
/// ```
#[napi]
pub struct Apvm {
    /// Inner APVM instance, wrapped in Arc for safe concurrent access.
    inner: Arc<apvm_core::Apvm>,
}

#[napi]
impl Apvm {
    /// Create a new APVM instance with the given configuration.
    ///
    /// Uses the provided GitHub token directly (if any). For automatic token
    /// resolution from environment variables and `gh` CLI, use
    /// `createWithTokenResolution()` instead.
    ///
    /// This method is async because the underlying HTTP client (octocrab)
    /// requires a Tokio runtime during initialization.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration options. All fields are optional:
    ///   - `cacheDir` — artifact cache base (defaults to `~/.apvm/cache`)
    ///   - `cacheEnabled` — whether the cache is active (defaults to `true`)
    ///   - `githubToken` — GitHub PAT for private repos
    ///
    /// # Throws
    ///
    /// - If the GitHub client cannot be initialized (e.g., invalid token format)
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// // Minimal — cache defaults to ~/.apvm/cache
    /// const apvm = await Apvm.create({});
    ///
    /// // With explicit config
    /// const apvm = await Apvm.create({
    ///   cacheDir: '/var/lib/apvm/cache',
    ///   githubToken: 'ghp_xxxxxxxxxxxx',
    /// });
    /// ```
    #[napi(factory)]
    pub async fn create(config: Option<ApvmConfig>) -> napi::Result<Self> {
        let config = config.unwrap_or_default();
        let rust_config: apvm_config::Config = config.into();
        // APVM_CACHE_DIR (if set) overrides the resolved cache directory, so a
        // single env var can isolate a run (e.g. tests) from ~/.apvm/cache.
        let rust_config = apvm_core::config_io::apply_env_overrides(rust_config);
        // Octocrab (HTTP client) requires a Tokio runtime during
        // initialization, which is why this factory is async.
        let inner = apvm_core::Apvm::new(rust_config).map_err(core_error_to_napi)?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Create a new APVM instance with automatic GitHub token resolution.
    ///
    /// Tries to find a GitHub token from multiple sources (in order):
    ///
    /// 1. The `githubToken` field in the config (if provided)
    /// 2. `GITHUB_TOKEN` environment variable
    /// 3. `GH_TOKEN` environment variable
    /// 4. `gh auth token` command (gh CLI >= 2.17.0)
    /// 5. gh CLI config file (`~/.config/gh/hosts.yml`)
    ///
    /// This method is async because it may need to execute `gh auth token`
    /// as a subprocess.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration options. All fields are optional:
    ///   - `cacheDir` — artifact cache base (defaults to `~/.apvm/cache`)
    ///   - `cacheEnabled` — whether the cache is active (defaults to `true`)
    ///   - `githubToken` — if set, skips resolution and uses this token
    ///
    /// # Throws
    ///
    /// - If the GitHub client cannot be initialized
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// // Minimal — resolves token automatically, cache defaults to ~/.apvm/cache
    /// const apvm = await Apvm.createWithTokenResolution({});
    ///
    /// // With explicit cache dir
    /// const apvm = await Apvm.createWithTokenResolution({
    ///   cacheDir: '/var/lib/apvm/cache',
    /// });
    ///
    /// console.log('Has token:', apvm.hasToken());
    /// ```
    #[napi(factory)]
    pub async fn create_with_token_resolution(config: Option<ApvmConfig>) -> napi::Result<Self> {
        let config = config.unwrap_or_default();
        let rust_config: apvm_config::Config = config.into();
        // APVM_CACHE_DIR (if set) overrides the resolved cache directory.
        let rust_config = apvm_core::config_io::apply_env_overrides(rust_config);
        let inner = apvm_core::Apvm::new_with_token_resolution(rust_config)
            .await
            .map_err(core_error_to_napi)?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Check if a GitHub token is available.
    ///
    /// Returns `true` if a token was found (either from config or resolved
    /// from environment). A token is required for building from private
    /// repositories (e.g., BackWPup).
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// if (!apvm.hasToken()) {
    ///   console.warn('No GitHub token — private repos will fail');
    /// }
    /// ```
    #[napi]
    pub fn has_token(&self) -> bool {
        self.inner.has_token()
    }

    /// Get a human-readable description of where the token came from.
    ///
    /// Returns `null` if no token is available.
    ///
    /// Possible values: `"config file"`, `"GITHUB_TOKEN"`, `"GH_TOKEN"`,
    /// `"gh auth token"`, `"gh config"`.
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// const source = apvm.tokenSource();
    /// if (source) {
    ///   console.log(`Token from: ${source}`);
    /// }
    /// ```
    #[napi]
    pub fn token_source(&self) -> Option<String> {
        self.inner.token_source().map(|s| s.to_string())
    }

    /// List all registered project names.
    ///
    /// Returns the names of projects that can be built (e.g., `["backwpup", "wp-rocket"]`).
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// const projects = apvm.listProjects();
    /// // => ['backwpup', 'wp-rocket']
    /// ```
    #[napi]
    pub fn list_projects(&self) -> Vec<String> {
        self.inner.registry.list().map(|p| p.name.clone()).collect()
    }

    /// Build a project from any git reference.
    ///
    /// This is the primary build method. It accepts a flexible `gitRef` string
    /// that supports automatic detection of the reference type, or explicit
    /// prefixes for disambiguation.
    ///
    /// Supported prefixes:
    /// - `pr:123` — Build from Pull Request
    /// - `tag:v1.0.0` — Build from tag
    /// - `tag:latest-stable` — Build from the latest stable tag (no alpha/beta/rc)
    /// - `tag:previous-stable` — Build from the previous stable tag
    /// - `tag:latest` — Build from the very latest tag (including prereleases)
    /// - `tag:previous-latest` — Build from the tag before the very latest
    /// - `branch:develop` — Build from branch
    /// - `commit:a1b2c3d` — Build from commit
    /// - `release:5.6.8` — Download pre-built GitHub Release assets
    /// - `release:latest-stable` — Download the latest stable release (non-prerelease, non-draft)
    /// - `release:previous-stable` — Download the previous stable release
    /// - `release:latest` — Download the very latest non-draft release (including prereleases)
    /// - `release:previous-latest` — Download the previous non-draft release
    ///
    /// The build runs asynchronously on the tokio runtime and returns a
    /// Promise that resolves with the complete build output.
    ///
    /// # Arguments
    ///
    /// * `options` - Build configuration (project, gitRef, outputDir, etc.)
    /// * `on_progress` - Optional callback for receiving build progress events.
    ///   Called with Node.js error-first callback shape:
    ///   `(err, event) => void`, where `event` is a [`JsBuildEvent`].
    ///
    /// # Throws
    ///
    /// - `InvalidArg` if the project is not found
    /// - `InvalidArg` if a private repo has no token
    /// - `GenericFailure` for git, build, or I/O errors
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// // Minimal (WP Rocket from a PR)
    /// const output = await apvm.build({
    ///   project: 'wp-rocket',
    ///   gitRef: 'pr:456',
    ///   outputDir: '/tmp/output',
    /// });
    ///
    /// // Full (BackWPup with version, variants, and progress)
    /// const output = await apvm.build(
    ///   {
    ///     project: 'backwpup',
    ///     gitRef: 'pr:123',
    ///     version: '5.1.0',
    ///     variants: ['pro'],
    ///     outputDir: '/tmp/output',
    ///   },
    ///   (err, event) => {
    ///     if (err || !event) return;
    ///     if (event.type === 'phase_started') {
    ///       console.log(`[${event.phase}] ${event.message}`);
    ///     }
    ///   },
    /// );
    /// ```
    #[napi(
        ts_args_type = "options: BuildOptions, onProgress?: (err: Error | null, event: JsBuildEvent) => void"
    )]
    pub async fn build(
        &self,
        options: BuildOptions,
        on_progress: Option<ThreadsafeFunction<JsBuildEvent>>,
    ) -> napi::Result<JsBuildOutput> {
        let apvm = Arc::clone(&self.inner);

        // Assemble the core build request, including per-call cache overrides.
        let request =
            apvm_core::BuildRequest::new(options.project, options.git_ref, options.output_dir)
                .version(options.version)
                .variants(options.variants.unwrap_or_default())
                .no_cache(options.no_cache.unwrap_or(false))
                .strict_version(options.strict_version.unwrap_or(false));

        let output = match on_progress {
            Some(callback) => {
                let reporter = JsProgressReporter::new(callback);
                apvm.build(request, &reporter)
                    .await
                    .map_err(core_error_to_napi)?
            }
            None => {
                let reporter = apvm_core::NullReporter;
                apvm.build(request, &reporter)
                    .await
                    .map_err(core_error_to_napi)?
            }
        };

        Ok(JsBuildOutput::from(output))
    }

    /// Build a project from a pull request number.
    ///
    /// Convenience method equivalent to `build({ gitRef: 'pr:{prNumber}', ... })`.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name (`"backwpup"` or `"wp-rocket"`)
    /// * `pr_number` - Pull request number
    /// * `output_dir` - Directory for build artifacts
    /// * `version` - Optional version (required for BackWPup)
    /// * `variants` - Optional variant filter
    /// * `on_progress` - Optional progress callback
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// const output = await apvm.buildFromPr(
    ///   'wp-rocket', 456, '/tmp/output',
    ///   undefined, undefined,
    ///   (err, event) => {
    ///     if (err || !event) return;
    ///     console.log(event.type);
    ///   },
    /// );
    /// ```
    #[napi(
        ts_args_type = "project: string, prNumber: number, outputDir: string, version?: string, variants?: string[], onProgress?: (err: Error | null, event: JsBuildEvent) => void"
    )]
    pub async fn build_from_pr(
        &self,
        project: String,
        pr_number: u32,
        output_dir: String,
        version: Option<String>,
        variants: Option<Vec<String>>,
        on_progress: Option<ThreadsafeFunction<JsBuildEvent>>,
    ) -> napi::Result<JsBuildOutput> {
        self.build(
            BuildOptions {
                project,
                git_ref: format!("pr:{pr_number}"),
                version,
                variants,
                output_dir,
                no_cache: None,
                strict_version: None,
            },
            on_progress,
        )
        .await
    }

    /// Build a project from a branch name.
    ///
    /// Convenience method equivalent to `build({ gitRef: 'branch:{branch}', ... })`.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name (`"backwpup"` or `"wp-rocket"`)
    /// * `branch` - Branch name (e.g., `"develop"`, `"feature/my-feature"`)
    /// * `output_dir` - Directory for build artifacts
    /// * `version` - Optional version (required for BackWPup)
    /// * `variants` - Optional variant filter
    /// * `on_progress` - Optional progress callback
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// const output = await apvm.buildFromBranch(
    ///   'wp-rocket', 'develop', '/tmp/output',
    /// );
    /// ```
    #[napi(
        ts_args_type = "project: string, branch: string, outputDir: string, version?: string, variants?: string[], onProgress?: (err: Error | null, event: JsBuildEvent) => void"
    )]
    pub async fn build_from_branch(
        &self,
        project: String,
        branch: String,
        output_dir: String,
        version: Option<String>,
        variants: Option<Vec<String>>,
        on_progress: Option<ThreadsafeFunction<JsBuildEvent>>,
    ) -> napi::Result<JsBuildOutput> {
        self.build(
            BuildOptions {
                project,
                git_ref: format!("branch:{branch}"),
                version,
                variants,
                output_dir,
                no_cache: None,
                strict_version: None,
            },
            on_progress,
        )
        .await
    }

    /// Build a project from a tag.
    ///
    /// Convenience method equivalent to `build({ gitRef: 'tag:{tag}', ... })`.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name (`"backwpup"` or `"wp-rocket"`)
    /// * `tag` - Tag name (e.g., `"v1.0.0"`, `"3.17.4"`)
    /// * `output_dir` - Directory for build artifacts
    /// * `version` - Optional version (required for BackWPup)
    /// * `variants` - Optional variant filter
    /// * `on_progress` - Optional progress callback
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// const output = await apvm.buildFromTag(
    ///   'wp-rocket', 'v3.17.4', '/tmp/output',
    /// );
    /// ```
    #[napi(
        ts_args_type = "project: string, tag: string, outputDir: string, version?: string, variants?: string[], onProgress?: (err: Error | null, event: JsBuildEvent) => void"
    )]
    pub async fn build_from_tag(
        &self,
        project: String,
        tag: String,
        output_dir: String,
        version: Option<String>,
        variants: Option<Vec<String>>,
        on_progress: Option<ThreadsafeFunction<JsBuildEvent>>,
    ) -> napi::Result<JsBuildOutput> {
        self.build(
            BuildOptions {
                project,
                git_ref: format!("tag:{tag}"),
                version,
                variants,
                output_dir,
                no_cache: None,
                strict_version: None,
            },
            on_progress,
        )
        .await
    }

    /// Build a project from a specific commit SHA.
    ///
    /// Convenience method equivalent to `build({ gitRef: 'commit:{commit}', ... })`.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name (`"backwpup"` or `"wp-rocket"`)
    /// * `commit` - Commit SHA (minimum 7 characters)
    /// * `output_dir` - Directory for build artifacts
    /// * `version` - Optional version (required for BackWPup)
    /// * `variants` - Optional variant filter
    /// * `on_progress` - Optional progress callback
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// const output = await apvm.buildFromCommit(
    ///   'wp-rocket', 'a1b2c3d', '/tmp/output',
    /// );
    /// ```
    #[napi(
        ts_args_type = "project: string, commit: string, outputDir: string, version?: string, variants?: string[], onProgress?: (err: Error | null, event: JsBuildEvent) => void"
    )]
    pub async fn build_from_commit(
        &self,
        project: String,
        commit: String,
        output_dir: String,
        version: Option<String>,
        variants: Option<Vec<String>>,
        on_progress: Option<ThreadsafeFunction<JsBuildEvent>>,
    ) -> napi::Result<JsBuildOutput> {
        self.build(
            BuildOptions {
                project,
                git_ref: format!("commit:{commit}"),
                version,
                variants,
                output_dir,
                no_cache: None,
                strict_version: None,
            },
            on_progress,
        )
        .await
    }

    /// Download pre-built assets from a specific GitHub Release.
    ///
    /// This bypasses the clone → build pipeline entirely — release assets
    /// are downloaded directly from GitHub. The version is derived from
    /// the release tag automatically.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name (`"backwpup"` or `"wp-rocket"`)
    /// * `tag` - Release tag (e.g., `"5.6.8"`, `"v5.6.8"`)
    /// * `output_dir` - Directory for downloaded assets
    /// * `variants` - Optional variant filter
    /// * `on_progress` - Optional progress callback
    ///
    /// # Throws
    ///
    /// - `InvalidArg` if the project is not found
    /// - `InvalidArg` if a private repo has no token
    /// - `GenericFailure` if the release or assets are not found
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// const output = await apvm.downloadRelease(
    ///   'backwpup', '5.6.8', '/tmp/output',
    /// );
    ///
    /// // With variants and progress
    /// const output = await apvm.downloadRelease(
    ///   'backwpup', '5.6.8', '/tmp/output',
    ///   ['free', 'pro-en'],
    ///   (err, event) => {
    ///     if (err || !event) return;
    ///     console.log(event.type, event.message);
    ///   },
    /// );
    /// ```
    #[napi(
        ts_args_type = "project: string, tag: string, outputDir: string, variants?: string[], onProgress?: (err: Error | null, event: JsBuildEvent) => void"
    )]
    pub async fn download_release(
        &self,
        project: String,
        tag: String,
        output_dir: String,
        variants: Option<Vec<String>>,
        on_progress: Option<ThreadsafeFunction<JsBuildEvent>>,
    ) -> napi::Result<JsBuildOutput> {
        self.build(
            BuildOptions {
                project,
                git_ref: format!("release:{tag}"),
                version: None,
                variants,
                output_dir,
                no_cache: None,
                strict_version: None,
            },
            on_progress,
        )
        .await
    }

    /// Download pre-built assets using a release selector keyword.
    ///
    /// Instead of specifying an exact tag, use a [`JsReleaseSelector`] to
    /// dynamically resolve the latest or previous release from the GitHub
    /// Releases API. Drafts are always excluded.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name (`"backwpup"` or `"wp-rocket"`)
    /// * `selector` - Which release to download (see [`JsReleaseSelector`])
    /// * `output_dir` - Directory for downloaded assets
    /// * `variants` - Optional variant filter
    /// * `on_progress` - Optional progress callback
    ///
    /// # Throws
    ///
    /// - `InvalidArg` if the project is not found
    /// - `InvalidArg` if a private repo has no token
    /// - `GenericFailure` if no matching release is found
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// import { JsReleaseSelector } from 'apvm-napi';
    ///
    /// // Download the latest stable release
    /// const output = await apvm.downloadReleaseBySelector(
    ///   'backwpup',
    ///   JsReleaseSelector.LatestStable,
    ///   '/tmp/output',
    /// );
    ///
    /// // Download the very latest release (including prereleases)
    /// const output = await apvm.downloadReleaseBySelector(
    ///   'backwpup',
    ///   JsReleaseSelector.Latest,
    ///   '/tmp/output',
    ///   undefined,
    ///   (err, event) => {
    ///     if (err || !event) return;
    ///     console.log(event.type, event.message);
    ///   },
    /// );
    /// ```
    #[napi(
        ts_args_type = "project: string, selector: JsReleaseSelector, outputDir: string, variants?: string[], onProgress?: (err: Error | null, event: JsBuildEvent) => void"
    )]
    pub async fn download_release_by_selector(
        &self,
        project: String,
        selector: JsReleaseSelector,
        output_dir: String,
        variants: Option<Vec<String>>,
        on_progress: Option<ThreadsafeFunction<JsBuildEvent>>,
    ) -> napi::Result<JsBuildOutput> {
        let keyword = match selector {
            JsReleaseSelector::LatestStable => "latest-stable",
            JsReleaseSelector::PreviousStable => "previous-stable",
            JsReleaseSelector::Latest => "latest",
            JsReleaseSelector::PreviousLatest => "previous-latest",
        };
        self.build(
            BuildOptions {
                project,
                git_ref: format!("release:{keyword}"),
                version: None,
                variants,
                output_dir,
                no_cache: None,
                strict_version: None,
            },
            on_progress,
        )
        .await
    }
}
