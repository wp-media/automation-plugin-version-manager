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
use crate::types::{BuildOptions, JsBuildEvent, JsBuildOutput};

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
/// // Minimal — no config needed, uses temp dir for builds
/// const apvm = await Apvm.create({});
///
/// // Create with explicit builds dir and token
/// const apvm = await Apvm.create({
///   buildsDir: '/var/lib/apvm/builds',
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
///   onProgress: (event) => console.log(event.type, event.message),
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
    ///   - `buildsDir` — where artifacts are stored (uses temp dir if omitted)
    ///   - `githubToken` — GitHub PAT for private repos
    ///
    /// # Throws
    ///
    /// - If the GitHub client cannot be initialized (e.g., invalid token format)
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// // Minimal — uses a temp directory for builds
    /// const apvm = await Apvm.create({});
    ///
    /// // With explicit config
    /// const apvm = await Apvm.create({
    ///   buildsDir: '/var/lib/apvm/builds',
    ///   githubToken: 'ghp_xxxxxxxxxxxx',
    /// });
    /// ```
    #[napi(factory)]
    pub async fn create(config: Option<ApvmConfig>) -> napi::Result<Self> {
        let config = config.unwrap_or_default();
        let rust_config: apvm_config::Config = config.into();
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
    ///   - `buildsDir` — where artifacts are stored (uses temp dir if omitted)
    ///   - `githubToken` — if set, skips resolution and uses this token
    ///
    /// # Throws
    ///
    /// - If the GitHub client cannot be initialized
    ///
    /// # TypeScript
    ///
    /// ```typescript
    /// // Minimal — resolves token automatically, uses temp builds dir
    /// const apvm = await Apvm.createWithTokenResolution({});
    ///
    /// // With explicit builds dir
    /// const apvm = await Apvm.createWithTokenResolution({
    ///   buildsDir: '/var/lib/apvm/builds',
    /// });
    ///
    /// console.log('Has token:', apvm.hasToken());
    /// ```
    #[napi(factory)]
    pub async fn create_with_token_resolution(config: Option<ApvmConfig>) -> napi::Result<Self> {
        let config = config.unwrap_or_default();
        let rust_config: apvm_config::Config = config.into();
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
    /// The build runs asynchronously on the tokio runtime and returns a
    /// Promise that resolves with the complete build output.
    ///
    /// # Arguments
    ///
    /// * `options` - Build configuration (project, gitRef, outputDir, etc.)
    /// * `on_progress` - Optional callback for receiving build progress events.
    ///   Called with a [`JsBuildEvent`] object for each build event.
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
    ///   (event) => {
    ///     if (event.type === 'phase_started') {
    ///       console.log(`[${event.phase}] ${event.message}`);
    ///     }
    ///   },
    /// );
    /// ```
    #[napi(
        ts_args_type = "options: BuildOptions, onProgress?: (event: JsBuildEvent) => void"
    )]
    pub async fn build(
        &self,
        options: BuildOptions,
        on_progress: Option<ThreadsafeFunction<JsBuildEvent>>,
    ) -> napi::Result<JsBuildOutput> {
        let apvm = Arc::clone(&self.inner);

        let version_ref = options.version.as_deref().map(String::from);
        let variants_owned: Option<Vec<String>> = options.variants;

        let output = match on_progress {
            Some(callback) => {
                let reporter = JsProgressReporter::new(callback);
                apvm.build(
                    &options.project,
                    version_ref.as_deref(),
                    &options.git_ref,
                    variants_owned
                        .as_ref()
                        .map(|v| v.iter().map(String::as_str).collect::<Vec<&str>>())
                        .as_deref(),
                    &options.output_dir,
                    &reporter,
                )
                .await
                .map_err(core_error_to_napi)?
            }
            None => {
                let reporter = apvm_core::NullReporter;
                apvm.build(
                    &options.project,
                    version_ref.as_deref(),
                    &options.git_ref,
                    variants_owned
                        .as_ref()
                        .map(|v| v.iter().map(String::as_str).collect::<Vec<&str>>())
                        .as_deref(),
                    &options.output_dir,
                    &reporter,
                )
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
    ///   (event) => console.log(event.type),
    /// );
    /// ```
    #[napi(
        ts_args_type = "project: string, prNumber: number, outputDir: string, version?: string, variants?: string[], onProgress?: (event: JsBuildEvent) => void"
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
        ts_args_type = "project: string, branch: string, outputDir: string, version?: string, variants?: string[], onProgress?: (event: JsBuildEvent) => void"
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
        ts_args_type = "project: string, tag: string, outputDir: string, version?: string, variants?: string[], onProgress?: (event: JsBuildEvent) => void"
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
        ts_args_type = "project: string, commit: string, outputDir: string, version?: string, variants?: string[], onProgress?: (event: JsBuildEvent) => void"
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
            },
            on_progress,
        )
        .await
    }
}
