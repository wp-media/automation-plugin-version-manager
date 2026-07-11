//! Build command implementation.
//!
//! This module provides the main build command that handles building a project
//! from any git reference (PR, branch, tag, or commit) with automatic detection.

use crate::build::plugins::{VersionOverride, VersionRequirement};
use crate::build::progress::{BuildEvent, BuildPhase, BuildStep, ProgressReporter};
use crate::build::{ArtifactOrigin, BuildResult, BuildRunner, ProducedArtifact};
use crate::commands::cache;
use crate::error::{Error, Result};
use crate::git::{BuildWorkspace, RefResolver, RefSource, RemoteGit, ResolvedRef};
use crate::github::client::download_asset_owned;
use crate::github::{GitHubClient, ReleaseAsset};
use crate::projects::{Project, ProjectRegistry};
use apvm_config::Config;
use apvm_storage::{ArtifactStore, ReleaseMetadata, StoredArtifact};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::task::JoinSet;

// Re-export storage types for convenience (consumers don't need to add apvm-storage)
pub use apvm_storage::{BuildMetadata, SourceArtifact};

/// A request to build a project.
///
/// Bundles everything a build needs so per-invocation options (cache control,
/// version strictness) don't sprawl across positional arguments. Construct
/// with [`BuildRequest::new`] and refine with the chainable setters.
///
/// ```ignore
/// let request = BuildRequest::new("wp-rocket", "pr:456", "/output")
///     .version(Some("3.17.4".to_string()))
///     .no_cache(true);
/// ```
#[derive(Debug, Clone)]
pub struct BuildRequest {
    /// Project name from the registry.
    pub project: String,
    /// Version to build, or `None` to auto-detect / use the builder default.
    pub version: Option<String>,
    /// Git reference: PR number, branch, tag, commit, or release.
    pub git_ref: String,
    /// Variants to build; empty means the builder's default set (all).
    pub variants: Vec<String>,
    /// Directory the produced artifacts are delivered to.
    pub output_dir: PathBuf,
    /// Skip the artifact cache for this build. The build still runs and (unless
    /// caching is globally disabled) still warms the cache. Default `false`.
    pub no_cache: bool,
}

impl BuildRequest {
    /// Create a request with the required fields; all options default off,
    /// no version pin, and the builder's default variant set.
    pub fn new(
        project: impl Into<String>,
        git_ref: impl Into<String>,
        output_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            project: project.into(),
            version: None,
            git_ref: git_ref.into(),
            variants: Vec::new(),
            output_dir: output_dir.into(),
            no_cache: false,
        }
    }

    /// Pin the version to build (`None` clears any pin).
    #[must_use]
    pub fn version(mut self, version: Option<String>) -> Self {
        self.version = version;
        self
    }

    /// Set the variants to build (empty = builder defaults).
    #[must_use]
    pub fn variants(mut self, variants: Vec<String>) -> Self {
        self.variants = variants;
        self
    }

    /// Skip the artifact cache for this build.
    #[must_use]
    pub fn no_cache(mut self, no_cache: bool) -> Self {
        self.no_cache = no_cache;
        self
    }

    /// Borrow the variants as `&str` slices for the builder API.
    fn variant_refs(&self) -> Vec<&str> {
        self.variants.iter().map(String::as_str).collect()
    }
}

/// A request to warm the artifact cache for a project.
///
/// Warming runs the **exact same pipeline** as a [`BuildRequest`] — resolve the
/// ref, then either serve/partially-reuse from the cache, clone + build the
/// missing variants, or download release assets — and stores everything into
/// the cache. The one difference is that it delivers **nothing** to an output
/// directory: its purpose is to populate the cache, not to produce files for a
/// caller.
///
/// That intent is encoded in the type: `WarmRequest` is deliberately a smaller
/// surface than [`BuildRequest`], omitting the two fields that would either
/// be meaningless or actively defeat warming:
///
/// - **no `output_dir`** — warming never writes artifacts anywhere but the
///   cache, so there is no output directory to accept;
/// - **no `no_cache`** — warming *is* a cache operation; bypassing the cache
///   would make it a no-op.
///
/// Construct with [`WarmRequest::new`] and refine with the chainable setters.
///
/// ```ignore
/// use apvm_core::WarmRequest;
///
/// // Warm BackWPup 5.1.0 from PR #123 (only the `free` variant).
/// let request = WarmRequest::new("backwpup", "pr:123")
///     .version(Some("5.1.0".to_string()))
///     .variants(vec!["free".to_string()]);
/// ```
#[derive(Debug, Clone)]
pub struct WarmRequest {
    /// Project name from the registry.
    pub project: String,
    /// Version to warm, or `None` to auto-detect / use the builder default.
    pub version: Option<String>,
    /// Git reference: PR number, branch, tag, commit, or release.
    pub git_ref: String,
    /// Variants to warm; empty means the builder's default set (all).
    pub variants: Vec<String>,
}

impl WarmRequest {
    /// Create a warm request with the required fields; no version pin and the
    /// builder's default variant set.
    pub fn new(project: impl Into<String>, git_ref: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            version: None,
            git_ref: git_ref.into(),
            variants: Vec::new(),
        }
    }

    /// Pin the version to warm (`None` clears any pin).
    #[must_use]
    pub fn version(mut self, version: Option<String>) -> Self {
        self.version = version;
        self
    }

    /// Set the variants to warm (empty = builder defaults).
    #[must_use]
    pub fn variants(mut self, variants: Vec<String>) -> Self {
        self.variants = variants;
        self
    }

    /// Lower a [`WarmRequest`] onto the [`BuildRequest`] the pipeline runs.
    ///
    /// The fields `WarmRequest` deliberately omits are filled with the only
    /// values that make sense for warming — and, crucially, the command is run
    /// with its `warm_cache` flag set, which is what actually guarantees the
    /// output directory is never touched (the `output_dir` below is never read):
    ///
    /// - `output_dir = ""` — never read in warm mode; warming copies nothing to
    ///   an output directory;
    /// - `no_cache = false` — warming must consult and populate the cache.
    pub(crate) fn into_build_request(self) -> BuildRequest {
        BuildRequest {
            project: self.project,
            version: self.version,
            git_ref: self.git_ref,
            variants: self.variants,
            output_dir: PathBuf::new(),
            no_cache: false,
        }
    }
}

/// Extended build result with git metadata.
///
/// Contains the build artifacts plus all the metadata needed
/// for storage operations (commit SHA, source type, branch name).
///
/// # Storage Integration
///
/// This type provides conversion methods to transform build output into
/// storage-compatible formats. The build pipeline uses them itself to warm
/// the artifact cache after a build; they remain public for consumers that
/// maintain their own [`apvm_storage::ArtifactStore`]:
///
/// ```ignore
/// let output = apvm
///     .build(BuildRequest::new("backwpup", "pr:123", "/output"), &NullReporter)
///     .await?;
///
/// // Convert for a consumer-owned store (the built-in cache already did this)
/// let store = ArtifactStore::open("/my/own/store")?;
/// store.store(
///     &output.to_build_metadata("backwpup"),
///     &output.to_source_artifacts(),
/// )?;
/// ```
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
    /// Set when the builder rewrote the source version to the caller's
    /// requested version during this build (WP Rocket / Imagify). `None` when no
    /// override happened: no version was pinned, the artifacts were served from
    /// the cache (no build ran), the pinned version already matched source, or
    /// the builder does not support overrides.
    pub version_override: Option<VersionOverride>,
}

impl BuildOutput {
    /// Whether every delivered artifact came from the cache (no build ran).
    ///
    /// Derived from artifact provenance: `true` only when there is at least
    /// one artifact and all of them have [`ArtifactOrigin::Cache`]. A partial
    /// build (some reused, some freshly built) is therefore `false`.
    pub fn from_cache(&self) -> bool {
        !self.result.artifacts.is_empty()
            && self
                .result
                .artifacts
                .iter()
                .all(|artifact| artifact.origin == ArtifactOrigin::Cache)
    }

    /// Get the source type for storage.
    pub fn source(&self) -> &RefSource {
        &self.resolved_ref.source
    }

    /// Get a human-readable description of what was built.
    ///
    /// For PRs this includes the resolved branch name:
    /// - `"PR #123 (branch: feature/foo) @ a1b2c3d"`
    /// - `"branch 'develop' @ e4f5g6h"`
    pub fn description(&self) -> String {
        format!(
            "{} @ {}",
            self.resolved_ref.detailed_description(),
            self.commit_short
        )
    }

    // =========================================================================
    // Storage Conversion Helpers
    // =========================================================================

    /// Convert to storage metadata.
    ///
    /// Creates a [`BuildMetadata`] struct suitable for passing to
    /// [`apvm_storage::ArtifactStore::store()`].
    ///
    /// # Arguments
    ///
    /// * `project` - Project name (must match what was passed to `build()`)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let output = apvm.build("backwpup", "5.6.0", "pr:123", None).await?;
    /// let metadata = output.to_build_metadata("backwpup");
    ///
    /// assert_eq!(metadata.project, "backwpup");
    /// assert_eq!(metadata.version, "5.6.0");
    /// ```
    pub fn to_build_metadata(&self, project: &str) -> BuildMetadata {
        BuildMetadata::new(
            project.to_string(),
            self.result.version.clone(),
            self.resolved_ref.to_build_source(),
            self.commit.clone(),
            self.branch.clone(),
        )
    }

    /// Convert artifacts to storage format.
    ///
    /// Transforms [`ProducedArtifact`]s into [`SourceArtifact`]s suitable
    /// for passing to [`apvm_storage::ArtifactStore::store()`].
    ///
    /// # Returns
    ///
    /// A vector of [`SourceArtifact`] structs. Returns an empty vector if
    /// no artifacts were produced.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let output = apvm.build("backwpup", "5.6.0", "pr:123", None).await?;
    /// let artifacts = output.to_source_artifacts();
    ///
    /// for artifact in &artifacts {
    ///     println!("  {} -> {}", artifact.path.display(), artifact.target_name);
    /// }
    /// ```
    pub fn to_source_artifacts(&self) -> Vec<SourceArtifact> {
        self.result
            .artifacts
            .iter()
            .map(|a| SourceArtifact {
                variant_id: a.variant_id.clone(),
                path: a.path.clone(),
                target_name: a.filename.clone(),
            })
            .collect()
    }

    /// Check if a build at this commit already exists in storage.
    ///
    /// This is a convenience method for implementing "skip if exists" logic.
    /// Returns `true` if a build with the same commit exists, regardless of
    /// whether all variants are present.
    ///
    /// # Arguments
    ///
    /// * `store` - The artifact store to check
    /// * `project` - Project name
    ///
    /// # Example
    ///
    /// ```ignore
    /// let store = ArtifactStore::open(config.cache_dir)?;
    ///
    /// // Check before building (dry-run or skip logic)
    /// let exists = output.exists_in_store(&store, "backwpup")?;
    /// if exists {
    ///     println!("Build already exists at commit {}", output.commit_short);
    /// }
    /// ```
    pub fn exists_in_store(
        &self,
        store: &apvm_storage::ArtifactStore,
        project: &str,
    ) -> apvm_storage::Result<bool> {
        let existing = store.find_by_commit(project, &self.result.version, &self.commit_short)?;
        Ok(existing.is_some())
    }

    /// Get missing variants that need to be built.
    ///
    /// Compares the variants in this build output against what's already
    /// stored, returning only those that are missing. Useful for incremental
    /// builds where some variants may already exist.
    ///
    /// # Arguments
    ///
    /// * `store` - The artifact store to check
    /// * `project` - Project name
    ///
    /// # Returns
    ///
    /// A vector of variant IDs that are NOT yet stored. Returns all variants
    /// if no build exists for this commit.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let missing = output.missing_variants(&store, "backwpup")?;
    /// if missing.is_empty() {
    ///     println!("All variants already built!");
    /// } else {
    ///     println!("Need to store: {:?}", missing);
    /// }
    /// ```
    pub fn missing_variants(
        &self,
        store: &apvm_storage::ArtifactStore,
        project: &str,
    ) -> apvm_storage::Result<Vec<Option<String>>> {
        let existing =
            store.get_existing_variants(project, &self.result.version, &self.commit_short)?;

        let missing: Vec<Option<String>> = self
            .result
            .artifacts
            .iter()
            .map(|a| a.variant_id.clone())
            .filter(|v| !existing.contains(v))
            .collect();

        Ok(missing)
    }

    /// Filter artifacts to only those not yet stored.
    ///
    /// Returns [`SourceArtifact`]s for variants that don't exist in storage.
    /// This enables incremental storage where only new variants are copied.
    ///
    /// # Arguments
    ///
    /// * `store` - The artifact store to check
    /// * `project` - Project name
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Only store what's new (deduplication)
    /// let new_artifacts = output.to_source_artifacts_filtered(&store, "backwpup")?;
    /// if !new_artifacts.is_empty() {
    ///     store.store(&output.to_build_metadata("backwpup"), &new_artifacts)?;
    /// }
    /// ```
    pub fn to_source_artifacts_filtered(
        &self,
        store: &apvm_storage::ArtifactStore,
        project: &str,
    ) -> apvm_storage::Result<Vec<SourceArtifact>> {
        let existing =
            store.get_existing_variants(project, &self.result.version, &self.commit_short)?;

        let filtered: Vec<SourceArtifact> = self
            .result
            .artifacts
            .iter()
            .filter(|a| !existing.contains(&a.variant_id))
            .map(|a| SourceArtifact {
                variant_id: a.variant_id.clone(),
                path: a.path.clone(),
                target_name: a.filename.clone(),
            })
            .collect();

        Ok(filtered)
    }
}

/// Extract a clean version string from a release tag name.
///
/// Strips common prefixes (`v`, `V`, `release-`, `release/`) used by
/// GitHub Release tags to get a bare version string suitable for storage
/// and display.
///
/// # Sources
///
/// - [`str::strip_prefix`](https://doc.rust-lang.org/std/primitive.str.html#method.strip_prefix)
///   (stable since Rust 1.45).
fn version_from_tag(tag: &str) -> String {
    tag.strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .or_else(|| tag.strip_prefix("release-"))
        .or_else(|| tag.strip_prefix("release/"))
        .unwrap_or(tag)
        .to_string()
}

/// Check whether an input string looks like a version tag.
///
/// This heuristic decides whether to try the GitHub Release API **before**
/// cloning, avoiding an expensive clone when a pre-built release exists.
///
/// # Rules
///
/// 1. Optional prefix: `v`, `V`, `release-`, `release/`
/// 2. After the prefix: only ASCII digits and dots
/// 3. At least 2 dot-separated segments (e.g., `1.0`)
/// 4. No empty segments, no leading/trailing dots
///
/// # Sources
///
/// - [`char::is_ascii_digit`](https://doc.rust-lang.org/std/primitive.char.html#method.is_ascii_digit)
///   (stable since Rust 1.24).
fn looks_like_version_tag(input: &str) -> bool {
    let version_part = input
        .strip_prefix('v')
        .or_else(|| input.strip_prefix('V'))
        .or_else(|| input.strip_prefix("release-"))
        .or_else(|| input.strip_prefix("release/"))
        .unwrap_or(input);

    if !version_part.contains('.') {
        return false;
    }

    if !version_part.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return false;
    }

    if version_part.starts_with('.') || version_part.ends_with('.') {
        return false;
    }

    version_part.split('.').all(|s| !s.is_empty()) && version_part.split('.').count() >= 2
}

/// Check whether a `release:` value is a recognized keyword rather than
/// a literal tag name.
///
/// The four recognized keywords are:
/// - `latest-stable` — latest non-prerelease, non-draft release
/// - `previous-stable` — second non-prerelease, non-draft release
/// - `latest` — very latest non-draft release (including prereleases)
/// - `previous-latest` — second non-draft release (including prereleases)
///
/// Comparison is case-insensitive.
fn is_release_keyword(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "latest-stable" | "previous-stable" | "latest" | "previous-latest"
    )
}

/// Return `version` only if the artifact store would accept it as a version
/// string (non-empty, ≤ 64 chars, `[A-Za-z0-9._+-]`).
///
/// Release rows carry the version as optional metadata; a version derived
/// from an exotic tag (spaces, unicode, …) is dropped rather than allowed to
/// fail the whole cache-warm for the release's assets.
fn storable_version(version: &str) -> Option<String> {
    let valid = !version.is_empty()
        && version.len() <= 64
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
    valid.then(|| version.to_string())
}

/// Human-readable branch label derived from the resolved source, used for
/// storage metadata and display. Mirrors the `RefSource` → branch mapping so
/// the fast-path (cache) and full-build paths agree.
fn branch_label(resolved: &ResolvedRef) -> String {
    match &resolved.source {
        RefSource::PullRequest(_) | RefSource::Branch(_) => resolved.git_ref.clone(),
        RefSource::Tag(tag) => format!("tag/{tag}"),
        RefSource::Commit(sha) => format!("commit/{}", &sha[..7.min(sha.len())]),
        RefSource::Release(tag) => format!("release/{tag}"),
    }
}

/// Distinct variant ids present among `artifacts`, in first-seen order.
/// Variant-less (single-output) artifacts contribute nothing.
fn variants_built(artifacts: &[ProducedArtifact]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for artifact in artifacts {
        if let Some(variant) = &artifact.variant_id
            && !seen.contains(variant)
        {
            seen.push(variant.clone());
        }
    }
    seen
}

/// Detect a version from already-fetched source files by materializing them
/// into a temporary directory that mirrors their repo-relative layout, then
/// delegating to the builder's own
/// [`detect_version`](crate::build::plugins::Builder::detect_version).
///
/// Reusing `detect_version` (rather than re-implementing parsing) guarantees
/// the pre-clone detection agrees with the post-checkout resolution. Returns
/// `None` on any I/O failure or when no version is found — the caller then
/// clones, so a miss is always safe.
fn detect_version_from_files(
    builder: &dyn crate::build::plugins::Builder,
    files: &[(&str, Vec<u8>)],
) -> Option<String> {
    let tmp = tempfile::tempdir().ok()?;
    for (path, bytes) in files {
        let dest = tmp.path().join(path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        std::fs::write(&dest, bytes).ok()?;
    }
    builder.detect_version(tmp.path()).ok().flatten()
}

/// Re-point delivered artifacts at their canonical in-cache paths.
///
/// Warm-cache runs keep no output directory, so once the artifacts are stored,
/// their source paths — a soon-to-be-deleted workspace for freshly built
/// variants, a throwaway temp dir for freshly downloaded release assets — are
/// swapped for the stable locations inside the cache. Origins are left
/// untouched, so the result still distinguishes what was reused from what was
/// freshly built/downloaded. Matching is by filename, which is unique within a
/// stored build or release.
fn repoint_to_cache(artifacts: &mut [ProducedArtifact], stored: &[StoredArtifact]) {
    for artifact in artifacts.iter_mut() {
        if let Some(found) = stored.iter().find(|s| s.filename == artifact.filename) {
            artifact.path = found.path.clone();
        }
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
/// - `tag:latest-stable` → Build from the latest stable tag (no alpha/beta/rc)
/// - `tag:previous-stable` → Build from the previous stable tag
/// - `tag:latest` → Build from the very latest tag (including prereleases)
/// - `tag:previous-latest` → Build from the tag before the very latest
/// - `branch:main` → Force branch interpretation
/// - `commit:a1b2c3d` → Force commit interpretation
/// - `release:5.6.8` → Download pre-built assets from GitHub Release
/// - `release:latest-stable` → Download the latest stable release (non-prerelease, non-draft)
/// - `release:previous-stable` → Download the previous stable release
/// - `release:latest` → Download the very latest non-draft release (including prereleases)
/// - `release:previous-latest` → Download the previous non-draft release
///
/// Version-like bare inputs (e.g., `5.6.8`, `v3.21.1`) are automatically
/// checked against the GitHub Release API before cloning, if the project
/// has releases enabled.
pub struct BuildCommand<'a> {
    github: &'a GitHubClient,
    registry: &'a ProjectRegistry,
    config: &'a Config,
    /// Shared artifact cache. `None` disables caching for this command
    /// (store unavailable or caching disabled by config). Held as an `Arc`
    /// so it can be cloned into `spawn_blocking` closures for cache I/O.
    store: Option<Arc<ArtifactStore>>,
}

impl<'a> BuildCommand<'a> {
    /// Create a new build command.
    ///
    /// `store` is the shared artifact cache (`None` to disable caching). The
    /// per-invocation `--no-cache` override lives on the [`BuildRequest`].
    pub fn new(
        github: &'a GitHubClient,
        registry: &'a ProjectRegistry,
        config: &'a Config,
        store: Option<Arc<ArtifactStore>>,
    ) -> Self {
        Self {
            github,
            registry,
            config,
            store,
        }
    }

    /// Whether the cache should be consulted for this request: a store is
    /// available and the caller did not pass `--no-cache`.
    fn caching_enabled(&self, request: &BuildRequest) -> bool {
        self.store.is_some() && !request.no_cache
    }

    /// Execute the build described by `request`, with automatic ref detection.
    ///
    /// Orchestrates the phases and delegates the work (all private helpers):
    /// 1. Registry lookup + platform-support check + private-repo auth check
    ///    (all fail-fast, before any network or git work).
    /// 2. Early GitHub resolution (`try_early_resolve_github_ref`).
    /// 3. Release refs short-circuit to `download_release`.
    /// 4. Reference resolution (`resolve_reference`).
    /// 5. Pre-clone cache fast path (`try_cache_fast_path`) — a full hit
    ///    returns here without cloning.
    /// 6. Clone → checkout → build → collect (`run_clone_build`), which also
    ///    does post-checkout partial reuse and best-effort cache warming.
    ///
    /// # Warm mode
    ///
    /// When `warm_cache` is `true` the pipeline runs identically but delivers
    /// **nothing** to `request.output_dir` (which is empty and ignored): the
    /// fast path / partial reuse copy nothing, freshly built or downloaded
    /// artifacts are only stored into the cache, and the returned artifacts are
    /// re-pointed at their canonical in-cache paths. `build()` always passes
    /// `false`; only `warm_cache()` passes `true`, so the delivering behavior of
    /// `build()` can never be turned off through a [`BuildRequest`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::PrivateRepoNoToken`] if the project's repository is
    /// private and no GitHub token is configured. This check happens early to
    /// provide a clear error message before any git operations are attempted.
    pub async fn execute(
        &self,
        request: BuildRequest,
        reporter: &dyn ProgressReporter,
        warm_cache: bool,
    ) -> Result<BuildOutput> {
        // 1. Look up project in registry.
        let project_info = self.registry.get(&request.project)?;

        // Fail fast if this project cannot be built on the current platform,
        // BEFORE any network or git work. An unsupported host (e.g. Imagify on
        // Windows, whose packaging script needs a Unix toolchain) gets an
        // instant, actionable error instead of failing deep inside the build.
        project_info.builder.ensure_platform_supported()?;

        // 2. Validate authentication for private repositories (fail-fast),
        // BEFORE any git operation, so users get a clear, actionable error
        // instead of a cryptic "Authentication failed" from git.
        if project_info.is_private && self.config.github_token.is_none() {
            return Err(Error::PrivateRepoNoToken {
                repo: format!("{}/{}", project_info.owner, project_info.repo),
            });
        }

        // Warming needs a cache to warm. If the store is unavailable (disabled
        // by config, or it could not be opened) the pipeline still runs to
        // completion but populates nothing — warn so the caller isn't misled
        // into thinking the cache is now primed.
        if warm_cache && self.store.is_none() {
            reporter.report(&BuildEvent::Warning(
                "cache warming was requested, but the artifact cache is disabled or unavailable; \
                 nothing will be cached"
                    .to_string(),
            ));
        }

        // 3. Early-resolve GitHub refs that don't need a local repo (PRs,
        // releases, version-as-release). See `try_early_resolve_github_ref`.
        let early_resolved = self
            .try_early_resolve_github_ref(
                &request.git_ref,
                &project_info.owner,
                &project_info.repo,
                project_info.has_releases,
                reporter,
            )
            .await?;

        // 3b. Release refs download pre-built assets, skipping clone/build.
        if let Some(resolved) = &early_resolved
            && let RefSource::Release(tag) = &resolved.source
        {
            let variants = request.variant_refs();
            return self
                .download_release(
                    project_info,
                    tag,
                    &request.git_ref,
                    request.version.as_deref(),
                    &variants,
                    request.output_dir.as_path(),
                    request.no_cache,
                    warm_cache,
                    reporter,
                )
                .await;
        }

        // 4. Resolve the reference to a concrete commit before cloning.
        let resolved = self
            .resolve_reference(early_resolved, project_info, &request.git_ref, reporter)
            .await?;

        // Surface *what* was resolved (branch/tag/commit/PR) before any clone
        // or cache work, so consumers can display it up front.
        reporter.report(&BuildEvent::ReferenceResolved {
            resolved: resolved.clone(),
        });

        // 5. Pre-clone cache fast path (cases A/B/C). On a full hit the
        // artifacts are already delivered (or, when warming, already fully
        // cached) — no clone, no build.
        if let Some(output) = self
            .try_cache_fast_path(&request, project_info, &resolved, reporter, warm_cache)
            .await
        {
            return Ok(output);
        }

        // 6. Clone → checkout → build → collect (with post-checkout partial
        // reuse and best-effort cache warming inside).
        self.run_clone_build(&request, project_info, resolved, reporter, warm_cache)
            .await
    }

    /// Pre-clone cache fast path (behavior-matrix cases A/B).
    ///
    /// Returns `Some(output)` only on a **full** hit — every requested variant
    /// present for the resolved commit **at the requested version**. In a normal
    /// build the artifacts are copied into the output directory; when warming
    /// (`warm_cache`) nothing is copied — a full hit means the commit is already
    /// fully cached, so the returned artifacts simply reference the cache.
    ///
    /// Returns `None` (build normally) when caching is off/`--no-cache`, the
    /// commit SHA isn't known pre-clone, the lookup misses / errors, or **the
    /// version to build cannot be determined before checkout**. A hit requires
    /// an exact `(commit, version)` match, so the version must be known here.
    /// When it isn't predictable statically (a source-versioned builder with no
    /// `--ver`), [`detect_version_pre_clone`](Self::detect_version_pre_clone)
    /// fetches the source version file(s) at the commit and reads it without
    /// cloning; only if that also fails is the fast path skipped so the build
    /// clones and the post-checkout reuse path decides the hit instead.
    async fn try_cache_fast_path(
        &self,
        request: &BuildRequest,
        project_info: &Project,
        resolved: &ResolvedRef,
        reporter: &dyn ProgressReporter,
        warm_cache: bool,
    ) -> Option<BuildOutput> {
        let store = match &self.store {
            Some(store) if !request.no_cache => Arc::clone(store),
            _ => return None,
        };
        // The cache key needs the commit before cloning; without it the
        // post-checkout reuse path still applies once HEAD is known.
        let commit = resolved.commit_sha.clone()?;
        let builder = project_info.builder.as_ref();
        let keys = cache::variant_keys(builder, &request.variant_refs());
        // A hit requires an exact version match. When the version can't be
        // predicted statically (a source-versioned builder with no `--ver`),
        // learn it cheaply by fetching the source version file(s) at this
        // commit — no clone. If even that can't determine it, skip the fast
        // path and let the post-checkout reuse decide against the authoritative
        // source version.
        let predicted = match cache::predicted_version(builder, request.version.as_deref()) {
            Some(version) => version,
            None => {
                self.detect_version_pre_clone(project_info, &commit, reporter)
                    .await?
            }
        };

        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Cache,
            message: format!(
                "Checking cache for commit {}",
                commit.chars().take(7).collect::<String>()
            ),
        });
        let hit = cache::fast_path_lookup_and_copy(
            store,
            request.project.clone(),
            commit,
            Some(predicted),
            keys,
            request.output_dir.clone(),
            !warm_cache,
        )
        .await;
        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Cache,
        });

        let hit = hit?;

        let artifact_paths = hit.artifacts.iter().map(|a| a.path.clone()).collect();
        reporter.report(&BuildEvent::BuildSucceeded {
            artifacts: artifact_paths,
        });

        let variants_built = variants_built(&hit.artifacts);
        let result = BuildResult::new(
            hit.artifacts,
            request.output_dir.clone(),
            hit.version,
            variants_built,
        );
        Some(BuildOutput {
            result,
            resolved_ref: resolved.clone(),
            commit_short: hit.commit.chars().take(7).collect(),
            commit: hit.commit,
            branch: branch_label(resolved),
            // A cache hit never builds, so no source rewrite happens.
            version_override: None,
        })
    }

    /// Detect the version **before cloning** by fetching the builder's version
    /// source file(s) at `commit` and running the builder's own
    /// [`detect_version`](crate::build::plugins::Builder::detect_version) over
    /// them.
    ///
    /// This lets a source-versioned builder (WP Rocket / Imagify, whose version
    /// cannot be predicted statically) still take the pre-clone cache fast path.
    /// A commit is immutable, so the version read here is exactly the one a full
    /// build of that commit would produce — the cache key is correct, and no
    /// source override is needed later (source already carries this version).
    ///
    /// Best-effort: returns `None` when the builder declares no version files,
    /// or on any fetch/parse failure (missing file, network/auth error), and the
    /// caller falls back to cloning. It never errors — a detection miss must
    /// never break a build.
    async fn detect_version_pre_clone(
        &self,
        project_info: &Project,
        commit: &str,
        reporter: &dyn ProgressReporter,
    ) -> Option<String> {
        let files = project_info.builder.version_source_files();
        if files.is_empty() {
            return None;
        }

        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Preflight,
            message: format!(
                "Detecting version at {}",
                commit.chars().take(7).collect::<String>()
            ),
        });
        let detected = self
            .fetch_and_detect_version(project_info, commit, &files)
            .await;
        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Preflight,
        });

        if let Some(version) = &detected {
            tracing::info!(%version, %commit, "detected version before cloning");
        }
        detected
    }

    /// Fetch `files` at `commit` and detect the version from them. Split from
    /// [`detect_version_pre_clone`](Self::detect_version_pre_clone) so the fetch
    /// (network) and the parsing (pure, unit-tested via
    /// [`detect_version_from_files`]) stay separable.
    async fn fetch_and_detect_version(
        &self,
        project_info: &Project,
        commit: &str,
        files: &[&str],
    ) -> Option<String> {
        let mut fetched: Vec<(&str, Vec<u8>)> = Vec::with_capacity(files.len());
        for path in files {
            match self
                .github
                .get_file_content(&project_info.owner, &project_info.repo, path, commit)
                .await
            {
                Ok(Some(bytes)) => fetched.push((path, bytes)),
                Ok(None) => {
                    tracing::debug!(%path, %commit, "version file absent; cannot detect pre-clone");
                    return None;
                }
                Err(e) => {
                    tracing::debug!(error = %e, %path, "pre-clone version fetch failed; will clone");
                    return None;
                }
            }
        }
        detect_version_from_files(project_info.builder.as_ref(), &fetched)
    }

    /// Resolve a git reference to a concrete commit **before** cloning.
    ///
    /// Reuses the early GitHub resolution when present; otherwise runs the
    /// remote-enabled [`RefResolver`] (`git ls-remote` + commits API) so an
    /// unresolvable reference fails here — before any repository is cloned —
    /// and a resolvable one carries its commit SHA, ready for a cache lookup.
    async fn resolve_reference(
        &self,
        early_resolved: Option<ResolvedRef>,
        project_info: &Project,
        git_ref: &str,
        reporter: &dyn ProgressReporter,
    ) -> Result<ResolvedRef> {
        match early_resolved {
            Some(resolved) => {
                tracing::debug!(
                    "Using early-resolved ref: {} → {}",
                    resolved.source.description(),
                    resolved.git_ref
                );
                Ok(resolved)
            }
            None => {
                reporter.report(&BuildEvent::PhaseStarted {
                    phase: BuildPhase::Preflight,
                    message: format!("Resolving '{git_ref}' against remote"),
                });
                let remote =
                    RemoteGit::new(&project_info.repo_url, self.config.github_token.as_deref());
                let resolver =
                    RefResolver::new(self.github, &project_info.owner, &project_info.repo)
                        .with_remote(remote);
                let result = resolver.resolve(git_ref).await;
                reporter.report(&BuildEvent::PhaseCompleted {
                    phase: BuildPhase::Preflight,
                });
                let resolved = result?;
                tracing::info!(
                    "Resolved '{git_ref}' to {} without cloning{}",
                    resolved.detailed_description(),
                    resolved
                        .commit_sha
                        .as_deref()
                        .map(|sha| format!(" (commit {})", sha.chars().take(7).collect::<String>()))
                        .unwrap_or_default()
                );
                Ok(resolved)
            }
        }
    }

    /// Clone the repository, check out the resolved ref, build, and collect
    /// artifacts into the request's output directory.
    ///
    /// Reached when the pre-clone fast path did not fully hit. Once the
    /// authoritative version + commit are known (post-checkout), it performs
    /// **partial reuse**: requested variants already cached are copied from the
    /// store (origin [`ArtifactOrigin::Cache`]) and only the missing ones are
    /// built (origin [`ArtifactOrigin::Built`]). The full delivered set is then
    /// stored best-effort. The workspace is a temporary directory that is
    /// auto-cleaned when it drops, so artifacts are collected to the output
    /// directory before it goes out of scope.
    ///
    /// When `warm_cache` is set nothing is collected to an output directory:
    /// reused variants reference the cache in place, freshly built variants stay
    /// in the (still-live) workspace only long enough for the best-effort warm
    /// to copy them into the cache, and the returned artifacts are then
    /// re-pointed at their canonical in-cache paths.
    async fn run_clone_build(
        &self,
        request: &BuildRequest,
        project_info: &Project,
        resolved: ResolvedRef,
        reporter: &dyn ProgressReporter,
        warm_cache: bool,
    ) -> Result<BuildOutput> {
        let output_dir = request.output_dir.as_path();
        let builder = project_info.builder.as_ref();
        let variants = request.variant_refs();

        // Create isolated build workspace (auto-cleaned on drop) and clone.
        let workspace = BuildWorkspace::new(
            &project_info.name,
            &project_info.repo_url,
            self.config.github_token.as_deref(),
        )?;

        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Clone,
            message: format!("Cloning {}", project_info.repo_url),
        });
        workspace.clone_repo().await?;
        workspace.fetch().await?;
        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Clone,
        });

        let repo = workspace.repository();

        // Prepare repository for checkout (clean state). Reset FIRST to avoid
        // checkout failures due to uncommitted changes.
        // https://git-scm.com/docs/git-checkout#_description
        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Checkout,
            message: format!("Checking out {}", resolved.detailed_description()),
        });
        tracing::debug!(
            "Resetting repository and switching to default branch '{}'",
            project_info.default_branch
        );
        repo.reset_hard().await?;
        repo.checkout(&project_info.default_branch).await?;
        repo.reset_hard().await?;
        workspace.pull().await?;

        // Checkout the resolved ref.
        repo.checkout(&resolved.git_ref).await?;

        // Commit SHA after checkout — authoritative record of what was built.
        let (commit, commit_short) = repo.get_head_commit_pair().await?;
        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Checkout,
        });

        // Resolve version AFTER checkout so detect_version sees the right files.
        let resolved_version = self.resolve_version(
            &request.project,
            request.version.as_deref(),
            builder,
            repo.path(),
        )?;

        tracing::info!(
            "Building {} v{} from {}",
            request.project,
            resolved_version,
            resolved.detailed_description()
        );
        if variants.is_empty() {
            tracing::info!("Building all variants");
        } else {
            tracing::info!("Building variants: {}", variants.join(", "));
        }

        let branch = branch_label(&resolved);
        tracing::debug!(
            "Checked out {} at commit {}",
            resolved.git_ref,
            commit_short
        );

        // Post-checkout partial reuse (behavior-matrix cases D/E/F). Now that
        // the authoritative version + commit are known, copy whichever
        // requested variants are already cached and build only the rest.
        let caching = self.caching_enabled(request);
        let keys = cache::variant_keys(builder, &variants);
        let (reused, to_build) = if let (true, Some(store)) = (caching, self.store.clone()) {
            reporter.report(&BuildEvent::PhaseStarted {
                phase: BuildPhase::Cache,
                message: format!("Checking cache for v{resolved_version} @ {commit_short}"),
            });
            let outcome = cache::reuse_and_copy(
                store,
                request.project.clone(),
                resolved_version.clone(),
                commit.clone(),
                keys.clone(),
                output_dir.to_path_buf(),
                !warm_cache,
            )
            .await;
            reporter.report(&BuildEvent::PhaseCompleted {
                phase: BuildPhase::Cache,
            });
            (outcome.reused, outcome.to_build)
        } else {
            (Vec::new(), keys)
        };

        // Records a source version rewrite when it happens in the build path.
        let mut version_override: Option<VersionOverride> = None;

        // Build only the variants not served from the cache (if any).
        let mut artifacts = reused;
        if to_build.is_empty() {
            // Everything was reused — skip the builder entirely.
            tracing::info!(
                "All {} requested variant(s) reused from cache",
                artifacts.len()
            );
            let paths = artifacts.iter().map(|a| a.path.clone()).collect();
            reporter.report(&BuildEvent::BuildSucceeded { artifacts: paths });
        } else {
            if !artifacts.is_empty() {
                tracing::info!(
                    "Reusing {} cached variant(s); building {} more",
                    artifacts.len(),
                    to_build.len()
                );
            }
            // For a single-output builder the lone key is `None`; passing an
            // empty variant slice builds that one output. For a multi-variant
            // builder these are the concrete variant ids still needed.
            let build_variant_strs: Vec<&str> =
                to_build.iter().filter_map(|k| k.as_deref()).collect();

            // When the caller pinned a version and we are about to build,
            // rewrite it into the checked-out source (before packaging) so the
            // artifact carries the requested version instead of the source's.
            // A no-op for builders without an override or when the version
            // already matches; a hard error if it was requested but could not
            // be applied, so a mislabeled artifact is never shipped.
            if request.version.is_some()
                && let Some(applied) =
                    builder.apply_version_override(repo.path(), &resolved_version)?
            {
                reporter.report(&BuildEvent::Warning(format!(
                    "Overrode {} version {} → {} ({}) to match the requested --ver",
                    applied.file,
                    applied.from,
                    applied.to,
                    applied.sites.join(", ")
                )));
                version_override = Some(applied);
            }

            let build_context = workspace.to_build_context();
            let mut runner = BuildRunner::with_reporter(build_context, reporter);
            let built = runner
                .execute_build(builder, &resolved_version, &build_variant_strs)
                .await?;

            if warm_cache {
                // Warm: deliver nothing. The freshly built files stay in the
                // (still-live) workspace so the best-effort warm below can copy
                // them into the cache; their paths are re-pointed to the cache
                // afterwards. Origin stays `Built`.
                artifacts.extend(built.artifacts);
            } else {
                // Collect built artifacts to output_dir BEFORE workspace cleanup,
                // then repoint their paths and merge (they keep origin `Built`).
                let built_paths: Vec<_> = built.artifacts.iter().map(|a| a.path.clone()).collect();
                workspace.collect_artifacts(&built_paths, output_dir)?;
                for mut artifact in built.artifacts {
                    artifact.path = output_dir.join(&artifact.filename);
                    artifacts.push(artifact);
                }
            }
        }

        // Workspace is automatically cleaned up when it goes out of scope.
        let variants_built = variants_built(&artifacts);
        let mut output = BuildOutput {
            result: BuildResult::new(
                artifacts,
                output_dir.to_path_buf(),
                resolved_version,
                variants_built,
            ),
            resolved_ref: resolved,
            commit,
            commit_short,
            branch,
            version_override,
        };

        // Best-effort warm: record the full delivered set under its
        // authoritative (version, commit). Gated on store presence only — NOT
        // on `caching` — so `--no-cache` (which skips *reading* the cache)
        // still writes, keeping the cache fresh. Only a store that could not be
        // opened / disabled config (`store == None`) skips warming. Idempotent:
        // reused files are recognized, not recopied; never fails the build.
        //
        // In warm mode this store is the whole point (built artifacts are still
        // in the workspace and copied in here), and the returned build lets us
        // re-point the delivered artifacts at their stable in-cache paths before
        // the workspace is dropped.
        if let Some(store) = self.store.clone() {
            let stored = cache::store_build(
                store,
                output.to_build_metadata(&request.project),
                output.to_source_artifacts(),
            )
            .await;
            if warm_cache && let Some(stored) = stored {
                repoint_to_cache(&mut output.result.artifacts, &stored.artifacts);
            }
        }

        Ok(output)
    }

    /// Attempt to resolve GitHub-hosted refs before cloning.
    ///
    /// This performs early validation for references that can be resolved
    /// without a local repository, avoiding expensive clones when possible.
    ///
    /// # Behavior by Input Type
    ///
    /// | Input          | Early check? | On failure                        |
    /// |----------------|--------------|-----------------------------------|
    /// | `pr:123`       | Yes          | Return error immediately          |
    /// | `123` (bare)   | Yes          | Return `Ok(None)` (fallback)      |
    /// | `release:TAG`  | Yes          | Immediate `ResolvedRef`           |
    /// | `release:latest-stable` | Yes | Resolve via GitHub API            |
    /// | `release:previous-stable` | Yes | Resolve via GitHub API          |
    /// | `release:latest` | Yes        | Resolve via GitHub API            |
    /// | `release:previous-latest` | Yes | Resolve via GitHub API          |
    /// | `5.6.8` / `v5.6.8` (version-like, `has_releases=true`) | Yes | Return `Ok(None)` (fallback) |
    /// | `branch:main`, `tag:v1.0.0`, `tag:latest-stable`, `commit:SHA`, bare names | Not here | Resolved clone-free by the remote-enabled [`RefResolver`] in [`Self::execute`] step 4 |
    ///
    /// Inputs this method returns `Ok(None)` for are still resolved
    /// **before any clone**: the caller runs them through a
    /// [`RefResolver`] configured with a [`RemoteGit`] (`git ls-remote` +
    /// GitHub commits API), so an unresolvable reference fails early.
    ///
    /// # Arguments
    ///
    /// * `git_ref` - The raw git reference string from user input
    /// * `owner` - Repository owner (e.g., "wp-media")
    /// * `repo` - Repository name (e.g., "backwpup-pro")
    /// * `has_releases` - Whether the project supports GitHub Releases
    /// * `reporter` - Progress reporter for build events
    ///
    /// # Returns
    ///
    /// - `Ok(Some(resolved))` — Ref was resolved (PR, release, or version-like release match)
    /// - `Ok(None)` — Not resolvable early; needs local repo
    /// - `Err(...)` — Explicit prefix and lookup failed (e.g., `pr:123` not found)
    async fn try_early_resolve_github_ref(
        &self,
        git_ref: &str,
        owner: &str,
        repo: &str,
        has_releases: bool,
        reporter: &dyn ProgressReporter,
    ) -> Result<Option<ResolvedRef>> {
        let trimmed = git_ref.trim();

        // Determine whether this is an explicit `pr:` prefix or bare digits.
        // Anything else (branch:, tag:, commit:, or non-digit text) is not
        // a candidate for early GitHub resolution.
        let (pr_number, is_explicit) = match trimmed.split_once(':') {
            Some((prefix, value)) if !value.is_empty() => {
                match prefix.to_lowercase().as_str() {
                    "pr" => {
                        let num = value
                            .parse::<u64>()
                            .map_err(|_| Error::Git(format!("Invalid PR number: '{value}'")))?;
                        (num, true)
                    }
                    "release" => {
                        // Special keywords: resolve the concrete release tag
                        // via the GitHub API, then return as a normal Release ref.
                        if is_release_keyword(value) {
                            return self
                                .resolve_release_keyword(trimmed, value, owner, repo, reporter)
                                .await;
                        }

                        // Regular release:TAG — return immediately for the
                        // caller to handle via download_release().
                        return Ok(Some(ResolvedRef {
                            input: trimmed.to_string(),
                            source: RefSource::Release(value.to_string()),
                            git_ref: value.to_string(),
                            commit_sha: None,
                        }));
                    }
                    _ => {
                        // Other explicit prefix (branch:, tag:, commit:) — skip
                        return Ok(None);
                    }
                }
            }
            _ => {
                // No prefix — check if bare digits (potential PR number)
                if trimmed.chars().all(|c| c.is_ascii_digit()) && !trimmed.is_empty() {
                    match trimmed.parse::<u64>() {
                        Ok(num) => (num, false),
                        Err(_) => return Ok(None),
                    }
                } else if has_releases && looks_like_version_tag(trimmed) {
                    // Input looks like a version tag (e.g., "v5.6.8", "5.6.8")
                    // and the project has releases enabled. Try the GitHub
                    // Release API BEFORE cloning to avoid the expensive
                    // clone → build pipeline when a pre-built release exists.
                    return self
                        .try_resolve_version_as_release(trimmed, owner, repo, reporter)
                        .await;
                } else {
                    return Ok(None);
                }
            }
        };

        // At this point we have a PR number to look up.
        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Preflight,
            message: format!("Verifying PR #{pr_number} exists"),
        });

        let pr_result = self.github.get_pull_request(owner, repo, pr_number).await;

        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Preflight,
        });

        match pr_result {
            Ok(pr) => {
                tracing::info!(
                    "PR #{} found: '{}' (branch: {})",
                    pr_number,
                    pr.title,
                    pr.head_branch
                );
                Ok(Some(ResolvedRef::from_pull_request(&pr, trimmed)))
            }
            Err(e) => {
                if is_explicit {
                    // Explicit `pr:123` — the user specifically requested this PR,
                    // so a failure is a hard error. No clone should happen.
                    Err(Error::Git(format!("Failed to fetch PR #{pr_number}: {e}")))
                } else {
                    // Bare digits (auto-detect) — PR not found is not fatal,
                    // the resolver will try branch/tag/commit after cloning.
                    tracing::debug!(
                        "Early PR #{pr_number} lookup failed ({}), will retry after clone",
                        e
                    );
                    Ok(None)
                }
            }
        }
    }

    /// Attempt to match a version-like input to a GitHub Release.
    ///
    /// Tries the exact input first, then a `v`-prefixed or `v`-stripped
    /// variant. This handles the common case where the user types `5.6.8`
    /// but the GitHub tag is `v5.6.8`, or vice versa.
    ///
    /// Returns `Ok(None)` if no release matches — the caller falls through
    /// to clone → resolve → build.
    ///
    /// # Arguments
    ///
    /// * `input` - The version-like string (e.g., `"5.6.8"`, `"v3.21.1"`)
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `reporter` - Progress reporter for build events
    ///
    /// # Sources
    ///
    /// - [`GitHubClient::get_release_by_tag`] returns `Ok(None)` on 404,
    ///   `Err` on network/auth errors.
    async fn try_resolve_version_as_release(
        &self,
        input: &str,
        owner: &str,
        repo: &str,
        reporter: &dyn ProgressReporter,
    ) -> Result<Option<ResolvedRef>> {
        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Preflight,
            message: format!("Checking for release matching '{input}'"),
        });

        // 1. Try exact match (e.g., input="v5.6.8" → tag "v5.6.8")
        match self.github.get_release_by_tag(owner, repo, input).await {
            Ok(Some(_release)) => {
                tracing::info!("Found GitHub Release for tag '{input}'");
                reporter.report(&BuildEvent::PhaseCompleted {
                    phase: BuildPhase::Preflight,
                });
                return Ok(Some(ResolvedRef {
                    input: input.to_string(),
                    source: RefSource::Release(input.to_string()),
                    git_ref: input.to_string(),
                    commit_sha: None,
                }));
            }
            Ok(None) => {
                tracing::debug!("No release for exact tag '{input}', trying variant");
            }
            Err(e) => {
                // API error (network, auth, rate-limit) — don't block the
                // build pipeline, fall through to clone.
                tracing::debug!("Release lookup for '{input}' failed: {e}, will try clone");
                reporter.report(&BuildEvent::PhaseCompleted {
                    phase: BuildPhase::Preflight,
                });
                return Ok(None);
            }
        }

        // 2. Try variant: if input starts with 'v'/'V', strip it; otherwise add 'v'.
        //    This covers the common mismatch where users type "5.6.8" but the
        //    GitHub Release tag is "v5.6.8", or vice versa.
        let variant = if input.starts_with('v') || input.starts_with('V') {
            input[1..].to_string()
        } else {
            format!("v{input}")
        };

        match self.github.get_release_by_tag(owner, repo, &variant).await {
            Ok(Some(_release)) => {
                tracing::info!("Found GitHub Release for tag '{variant}' (input was '{input}')");
                reporter.report(&BuildEvent::PhaseCompleted {
                    phase: BuildPhase::Preflight,
                });
                Ok(Some(ResolvedRef {
                    input: input.to_string(),
                    source: RefSource::Release(variant.clone()),
                    git_ref: variant,
                    commit_sha: None,
                }))
            }
            Ok(None) => {
                tracing::debug!("No release for '{input}' or '{variant}', will clone and build");
                reporter.report(&BuildEvent::PhaseCompleted {
                    phase: BuildPhase::Preflight,
                });
                Ok(None)
            }
            Err(e) => {
                tracing::debug!("Release lookup for '{variant}' failed: {e}, will try clone");
                reporter.report(&BuildEvent::PhaseCompleted {
                    phase: BuildPhase::Preflight,
                });
                Ok(None)
            }
        }
    }

    /// Resolve a `release:` keyword to a concrete release tag via the GitHub API.
    ///
    /// Recognized keywords:
    /// - `latest-stable` — latest non-prerelease, non-draft release (via
    ///   GitHub's "Get the latest release" endpoint).
    /// - `previous-stable` — the second non-prerelease, non-draft release.
    /// - `latest` — very latest non-draft release (may be a prerelease).
    /// - `previous-latest` — the second non-draft release.
    ///
    /// # Sources
    ///
    /// - GitHub "Get the latest release" (non-prerelease, non-draft):
    ///   <https://docs.github.com/en/rest/releases/releases#get-the-latest-release>
    /// - GitHub "List releases" (ordered by `created_at` desc):
    ///   <https://docs.github.com/en/rest/releases/releases#list-releases>
    async fn resolve_release_keyword(
        &self,
        user_input: &str,
        keyword: &str,
        owner: &str,
        repo: &str,
        reporter: &dyn ProgressReporter,
    ) -> Result<Option<ResolvedRef>> {
        let label = keyword.to_ascii_lowercase();

        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Preflight,
            message: format!("Resolving release:{label} from {owner}/{repo}"),
        });

        let release_opt = match label.as_str() {
            "latest-stable" => self.github.get_latest_stable_release(owner, repo).await?,
            "previous-stable" => self.github.get_previous_stable_release(owner, repo).await?,
            "latest" => self.github.get_latest_release_any(owner, repo).await?,
            "previous-latest" => self.github.get_previous_release_any(owner, repo).await?,
            _ => {
                reporter.report(&BuildEvent::PhaseCompleted {
                    phase: BuildPhase::Preflight,
                });
                return Err(Error::Git(format!(
                    "Unknown release keyword '{keyword}'. \
                     Valid keywords: latest-stable, previous-stable, latest, previous-latest"
                )));
            }
        };

        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Preflight,
        });

        let release = release_opt.ok_or_else(|| Error::ReleaseNotFound {
            tag: keyword.to_string(),
            repo: format!("{owner}/{repo}"),
        })?;

        tracing::info!(
            "Resolved 'release:{keyword}' to release tag '{}'",
            release.tag_name
        );

        Ok(Some(ResolvedRef {
            input: user_input.to_string(),
            source: RefSource::Release(release.tag_name.clone()),
            git_ref: release.tag_name,
            commit_sha: None,
        }))
    }

    /// Download pre-built assets from a GitHub Release.
    ///
    /// This bypasses the entire clone → build pipeline: the release's
    /// zip assets are downloaded in parallel directly to `output_dir`.
    ///
    /// The version is always derived from the release tag (via [`version_from_tag`]),
    /// not from the CLI `--ver` argument. If `--ver` was provided, a warning is
    /// emitted because the release's artifacts are pre-built at a fixed version.
    ///
    /// # Arguments
    ///
    /// * `project_info` - Project information from registry
    /// * `tag` - Release tag name (e.g., `"v5.6.8"`)
    /// * `user_input` - The user's original input string (e.g., `"5.6.8"`, `"release:v5.6.8"`)
    /// * `version` - User-provided version override (will be ignored with a warning)
    /// * `variants` - Requested variants (empty = all matching assets)
    /// * `output_dir` - Directory where downloaded assets will be placed
    ///   (ignored when `warm_cache` is set — nothing is delivered)
    /// * `no_cache` - Skip *reading* the cache (the download still warms it)
    /// * `warm_cache` - Warm-only: deliver nothing; cached assets are referenced
    ///   in place and freshly downloaded ones go to a temp dir just long enough
    ///   to be copied into the cache
    /// * `reporter` - Progress reporter for receiving build events
    #[allow(clippy::too_many_arguments)]
    async fn download_release(
        &self,
        project_info: &crate::projects::Project,
        tag: &str,
        user_input: &str,
        version: Option<&str>,
        variants: &[&str],
        output_dir: &Path,
        no_cache: bool,
        warm_cache: bool,
        reporter: &dyn ProgressReporter,
    ) -> Result<BuildOutput> {
        let owner = &project_info.owner;
        let repo = &project_info.repo;
        let builder = project_info.builder.as_ref();

        // Validate that the project supports releases
        if !project_info.has_releases {
            return Err(Error::ReleasesNotAvailable {
                project: project_info.name.clone(),
                tag: tag.to_string(),
            });
        }

        // Surface the resolved release before downloading, mirroring the
        // clone/build path's ReferenceResolved event so consumers can display
        // what is being fetched.
        reporter.report(&BuildEvent::ReferenceResolved {
            resolved: ResolvedRef {
                input: user_input.to_string(),
                source: RefSource::Release(tag.to_string()),
                git_ref: tag.to_string(),
                commit_sha: None,
            },
        });

        // Fetch the release from GitHub
        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Preflight,
            message: format!("Fetching release '{tag}' from {owner}/{repo}"),
        });

        let release = self
            .github
            .get_release_by_tag(owner, repo, tag)
            .await?
            .ok_or_else(|| Error::ReleaseNotFound {
                tag: tag.to_string(),
                repo: format!("{owner}/{repo}"),
            })?;

        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Preflight,
        });

        // Emit info about pre-release / draft status
        if release.draft {
            reporter.report(&BuildEvent::Warning(format!(
                "Release '{tag}' is a DRAFT release"
            )));
        }
        if release.prerelease {
            reporter.report(&BuildEvent::Warning(format!(
                "Release '{tag}' is a PRE-RELEASE"
            )));
        }

        // Filter assets: only those matching the builder + requested variants
        let matched_assets: Vec<_> = release
            .assets
            .iter()
            .filter(|a| builder.matches_release_asset(&a.name))
            .filter(|a| {
                if variants.is_empty() {
                    return true;
                }
                match builder.variant_from_release_asset(&a.name) {
                    Some(v) => variants.contains(&v.as_str()),
                    None => true,
                }
            })
            .collect();

        if matched_assets.is_empty() {
            let available = release
                .assets
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::NoMatchingReleaseAssets {
                tag: tag.to_string(),
                repo: format!("{owner}/{repo}"),
                available,
            });
        }

        // The requested asset filenames (order preserved for stable output).
        let requested_names: Vec<String> = matched_assets.iter().map(|a| a.name.clone()).collect();

        // Consult the release cache: copy any already-cached assets and learn
        // which still need downloading (full hit, partial, or full miss —
        // mirroring the build cache).
        let caching = self.store.is_some() && !no_cache;
        let (cached, to_download_names) = if let (true, Some(store)) = (caching, self.store.clone())
        {
            reporter.report(&BuildEvent::PhaseStarted {
                phase: BuildPhase::Cache,
                message: format!("Checking cache for release '{tag}'"),
            });
            let outcome = cache::reuse_release_assets(
                store,
                project_info.name.clone(),
                tag.to_string(),
                requested_names.clone(),
                output_dir.to_path_buf(),
                !warm_cache,
            )
            .await;
            reporter.report(&BuildEvent::PhaseCompleted {
                phase: BuildPhase::Cache,
            });
            (outcome.cached, outcome.to_download)
        } else {
            (Vec::new(), requested_names)
        };

        // Warming delivers nothing to the output directory: freshly downloaded
        // assets land in a throwaway temp dir just long enough for the cache to
        // copy them in (the temp dir lives until this function returns, after
        // the warm below). A normal download writes straight to the output dir,
        // creating it first (the no-cache path skips the reuse helper that would
        // otherwise create it).
        let warm_tmp = if warm_cache {
            Some(
                tempfile::Builder::new()
                    .prefix("apvm-warm-")
                    .tempdir()
                    .map_err(Error::Io)?,
            )
        } else {
            std::fs::create_dir_all(output_dir).map_err(|e| {
                Error::Build(format!(
                    "Failed to create output directory '{}': {e}",
                    output_dir.display()
                ))
            })?;
            None
        };
        let download_dir: &Path = match &warm_tmp {
            Some(tmp) => tmp.path(),
            None => output_dir,
        };

        // Assets still needing a download.
        let to_download: Vec<&ReleaseAsset> = matched_assets
            .iter()
            .copied()
            .filter(|a| to_download_names.contains(&a.name))
            .collect();

        let mut artifacts: Vec<ProducedArtifact> = Vec::with_capacity(matched_assets.len());

        // Cache-origin artifacts: copied into output_dir by the reuse helper
        // for a normal download, or referenced in place in the cache when
        // warming. The variant id is derived from the filename so provenance
        // matches the download path exactly.
        for asset in cached {
            let variant_id = builder.variant_from_release_asset(&asset.filename);
            artifacts.push(
                ProducedArtifact::new(variant_id, asset.path, asset.filename, asset.size)
                    .with_origin(ArtifactOrigin::Cache),
            );
        }

        // Download the remaining assets in parallel (if any).
        if !to_download.is_empty() {
            reporter.report(&BuildEvent::PhaseStarted {
                phase: BuildPhase::ReleaseDownload,
                message: format!(
                    "Downloading {} asset(s) from release '{tag}'",
                    to_download.len()
                ),
            });

            // JoinSet::spawn requires `Future + Send + 'static`, so we use the
            // standalone `download_asset_owned` (takes all data by ownership).
            // On first error, remaining tasks are aborted (JoinSet is dropped).
            let token = self.github.token();
            let mut download_tasks: JoinSet<Result<(String, Vec<u8>)>> = JoinSet::new();
            for asset in &to_download {
                reporter.report(&BuildEvent::StepStarted {
                    step: BuildStep::new(
                        format!("Downloading {}", asset.name),
                        format!("GET {}", asset.download_url),
                    ),
                });
                download_tasks.spawn(download_asset_owned(
                    token.clone(),
                    owner.to_string(),
                    repo.to_string(),
                    (*asset).clone(),
                ));
            }

            while let Some(join_result) = download_tasks.join_next().await {
                // JoinError (task panic/cancellation).
                let download_result =
                    join_result.map_err(|e| Error::Build(format!("Download task failed: {e}")))?;
                // Download errors from the HTTP request.
                let (asset_name, bytes) = download_result?;

                // Warming writes to the temp dir; a normal download to the
                // output dir (both are `download_dir`).
                let dest = download_dir.join(&asset_name);
                std::fs::write(&dest, &bytes).map_err(|e| {
                    Error::Build(format!("Failed to write asset '{}': {e}", dest.display()))
                })?;

                reporter.report(&BuildEvent::StepCompleted {
                    step: BuildStep::new(
                        format!("Downloaded {asset_name}"),
                        format!("{} bytes", bytes.len()),
                    ),
                });

                let variant_id = builder.variant_from_release_asset(&asset_name);
                artifacts.push(
                    ProducedArtifact::new(variant_id, dest, asset_name, bytes.len() as u64)
                        .with_origin(ArtifactOrigin::Downloaded),
                );
            }

            reporter.report(&BuildEvent::PhaseCompleted {
                phase: BuildPhase::ReleaseDownload,
            });
        }

        // The version is always derived from the release tag, not the CLI --ver
        // arg. Release assets are pre-built at a fixed version in the tag name.
        let resolved_version = version_from_tag(tag);
        if let Some(provided) = version {
            reporter.report(&BuildEvent::Warning(format!(
                "Ignoring provided version '{provided}': \
                 release '{tag}' already defines version '{resolved_version}'"
            )));
        }

        let resolved_ref = ResolvedRef {
            input: user_input.to_string(),
            source: RefSource::Release(tag.to_string()),
            git_ref: tag.to_string(),
            commit_sha: None,
        };

        let variants_built = variants_built(&artifacts);
        let result = BuildResult::new(
            artifacts,
            output_dir.to_path_buf(),
            resolved_version,
            variants_built,
        );

        let filenames: Vec<_> = result.artifacts.iter().map(|a| a.path.clone()).collect();
        reporter.report(&BuildEvent::BuildSucceeded {
            artifacts: filenames,
        });

        let mut output = BuildOutput {
            result,
            resolved_ref,
            commit: format!("release-{tag}"),
            commit_short: tag.to_string(),
            branch: format!("release/{tag}"),
            // Release assets are pre-built; no source is checked out or rewritten.
            version_override: None,
        };

        // Best-effort warm: cache the full delivered asset set under this tag.
        // Gated on store presence only (like the build path) so `--no-cache`
        // still refreshes the cache; only a disabled/unopened store skips it.
        //
        // In warm mode this is the whole point (freshly downloaded assets live
        // in the temp dir and are copied in here), and the returned release lets
        // us re-point the delivered artifacts at their stable in-cache paths
        // before the temp dir is dropped.
        if let Some(store) = self.store.clone() {
            let mut metadata = ReleaseMetadata::new(&project_info.name, tag);
            // The version is informational on release rows; an exotic tag can
            // derive a string the store would reject, which must not prevent
            // the assets themselves from being cached.
            metadata.version = storable_version(&output.result.version);
            metadata.prerelease = release.prerelease;
            metadata.draft = release.draft;
            // `variant_id` is left `None`: release assets are cached by
            // filename (the variant is encoded in the filename, e.g.
            // `backwpup-pro-en-5.6.8.zip`), and `store_release` ignores it. The
            // delivered `output` artifacts still carry the resolved variant for
            // provenance display.
            let assets: Vec<SourceArtifact> = output
                .result
                .artifacts
                .iter()
                .map(|a| SourceArtifact {
                    variant_id: None,
                    path: a.path.clone(),
                    target_name: a.filename.clone(),
                })
                .collect();
            let stored = cache::store_release(store, metadata, assets).await;
            if warm_cache && let Some(stored) = stored {
                repoint_to_cache(&mut output.result.artifacts, &stored.assets);
            }
        }

        Ok(output)
    }

    /// Resolve the version based on the builder's [`VersionRequirement`].
    fn resolve_version(
        &self,
        project: &str,
        version: Option<&str>,
        builder: &dyn crate::build::plugins::Builder,
        working_dir: &std::path::Path,
    ) -> Result<String> {
        let requirement = builder.version_requirement();

        match (requirement, version) {
            (VersionRequirement::Required, None) => {
                // No explicit version provided — try the builder's default before erroring.
                // This covers builders like BackWPup that define a development version
                // (e.g., "9.99.99") so CLI users don't have to pass --ver every time
                // for clone-build flows, while download_release() still sees None
                // and correctly skips the "ignoring provided version" warning.
                if let Some(default) = builder.default_version() {
                    tracing::debug!(
                        "Using builder default version '{}' for '{}'",
                        default,
                        project
                    );
                    Ok(default.to_string())
                } else {
                    Err(Error::Build(format!(
                        "Project '{}' requires a version. Use: build(\"{}\", Some(\"X.Y.Z\"), ...)",
                        project, project
                    )))
                }
            }
            (VersionRequirement::Required, Some(v)) => {
                tracing::debug!("Using required version: {}", v);
                Ok(v.to_string())
            }

            (VersionRequirement::Embedded, Some(v)) => {
                tracing::warn!(
                    "Project '{}' has embedded version; ignoring provided '{}' and detecting from source",
                    project,
                    v
                );
                self.detect_or_error(project, builder, working_dir)
            }
            (VersionRequirement::Embedded, None) => {
                tracing::debug!("Detecting embedded version for '{}'", project);
                self.detect_or_error(project, builder, working_dir)
            }

            (VersionRequirement::Optional, Some(v)) => {
                tracing::debug!("Using provided version: {}", v);
                Ok(v.to_string())
            }
            (VersionRequirement::Optional, None) => {
                tracing::debug!("Auto-detecting version for '{}'", project);
                self.detect_or_error(project, builder, working_dir)
            }
        }
    }

    /// Try to detect version from source files, or return an error.
    fn detect_or_error(
        &self,
        project: &str,
        builder: &dyn crate::build::plugins::Builder,
        working_dir: &std::path::Path,
    ) -> Result<String> {
        match builder.detect_version(working_dir) {
            Ok(Some(detected)) => {
                tracing::info!("Auto-detected version: {}", detected);
                Ok(detected)
            }
            Ok(None) => Err(Error::Build(format!(
                "Could not auto-detect version for '{}'.\n\n\
                     The builder does not implement version detection, or the \
                     version was not found in the expected location.\n\n\
                     Please provide a version explicitly: build(\"{}\", Some(\"X.Y.Z\"), ...)",
                project, project
            ))),
            Err(e) => Err(Error::Build(format!(
                "Version detection failed for '{}': {}\n\n\
                     Please provide a version explicitly: build(\"{}\", Some(\"X.Y.Z\"), ...)",
                project, e, project
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::BuildContext;
    use crate::build::plugins::{BuildArtifact, Builder, VersionRequirement};
    use crate::build::progress::{BuildStep, NullReporter};
    use crate::projects::Project;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Minimal builder for testing purposes.
    struct TestBuilder;

    impl Builder for TestBuilder {
        fn version_requirement(&self) -> VersionRequirement {
            VersionRequirement::Required
        }

        fn setup_commands(&self) -> Vec<BuildStep> {
            vec![]
        }

        fn build_commands(
            &self,
            _context: &BuildContext,
            _version: &str,
            _variants: &[&str],
        ) -> Vec<BuildStep> {
            vec![]
        }

        fn artifacts(
            &self,
            _context: &BuildContext,
            _version: &str,
            _variants: &[&str],
        ) -> crate::Result<Vec<BuildArtifact>> {
            Ok(vec![])
        }
    }

    /// Single-output builder that, unlike [`TestBuilder`], advertises a default
    /// version — used to exercise the "no `--ver` but a builder default exists"
    /// (case B) path.
    struct DefaultVersionBuilder;

    impl Builder for DefaultVersionBuilder {
        fn version_requirement(&self) -> VersionRequirement {
            VersionRequirement::Required
        }
        fn default_version(&self) -> Option<&'static str> {
            Some("9.99.99")
        }
        fn setup_commands(&self) -> Vec<BuildStep> {
            vec![]
        }
        fn build_commands(&self, _: &BuildContext, _: &str, _: &[&str]) -> Vec<BuildStep> {
            vec![]
        }
        fn artifacts(
            &self,
            _: &BuildContext,
            _: &str,
            _: &[&str],
        ) -> crate::Result<Vec<BuildArtifact>> {
            Ok(vec![])
        }
    }

    /// Optional-version builder with no default (like WP Rocket / Imagify):
    /// with no `--ver`, the version comes from source and is unknown before
    /// checkout, so [`cache::predicted_version`] returns `None` and the
    /// pre-clone fast path is skipped.
    struct OptionalNoDefaultBuilder;

    impl Builder for OptionalNoDefaultBuilder {
        fn version_requirement(&self) -> VersionRequirement {
            VersionRequirement::Optional
        }
        fn setup_commands(&self) -> Vec<BuildStep> {
            vec![]
        }
        fn build_commands(&self, _: &BuildContext, _: &str, _: &[&str]) -> Vec<BuildStep> {
            vec![]
        }
        fn artifacts(
            &self,
            _: &BuildContext,
            _: &str,
            _: &[&str],
        ) -> crate::Result<Vec<BuildArtifact>> {
            Ok(vec![])
        }
    }

    // =========================================================================
    // Fast-path integration: `try_cache_fast_path` end-to-end against a real
    // (temp) store, no git/GitHub. Exercises variant-key derivation, lookup,
    // copy, BuildOutput assembly, and the exact-version gating.
    // =========================================================================

    const TEST_SHA: &str = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";

    /// Seed a single-output build for `project` at `(version, TEST_SHA)` into a
    /// fresh temp store. The source file is copied into the store, so the
    /// returned store owns its own copy.
    fn seed_single_output(
        project: &str,
        version: &str,
    ) -> (TempDir, std::sync::Arc<apvm_storage::ArtifactStore>) {
        let cache = TempDir::new().unwrap();
        let store = apvm_storage::ArtifactStore::open(cache.path()).unwrap();
        let src = TempDir::new().unwrap();
        let file = src.path().join("plugin.zip");
        std::fs::write(&file, b"cached-plugin-bytes").unwrap();
        store
            .store(
                &BuildMetadata::new(
                    project,
                    version,
                    apvm_storage::BuildSource::Commit(TEST_SHA.to_string()),
                    TEST_SHA,
                    None,
                ),
                &[SourceArtifact {
                    variant_id: None,
                    path: file,
                    target_name: "plugin.zip".to_string(),
                }],
            )
            .unwrap();
        (cache, std::sync::Arc::new(store))
    }

    fn single_output_project(name: &str, builder: Box<dyn Builder>) -> Project {
        Project {
            name: name.to_string(),
            repo_url: "https://github.com/test/repo.git".to_string(),
            owner: "test".to_string(),
            repo: "repo".to_string(),
            default_branch: "main".to_string(),
            is_private: false,
            has_releases: false,
            builder,
        }
    }

    fn resolved_commit() -> ResolvedRef {
        ResolvedRef {
            input: format!("commit:{TEST_SHA}"),
            source: RefSource::Commit(TEST_SHA.to_string()),
            git_ref: TEST_SHA.to_string(),
            commit_sha: Some(TEST_SHA.to_string()),
        }
    }

    #[tokio::test]
    async fn fast_path_full_hit_delivers_from_cache() {
        let (cache, store) = seed_single_output("test", "1.0.0");
        let mut registry = ProjectRegistry::new();
        registry.register(single_output_project("test", Box::new(TestBuilder)));
        let config = Config::new(cache.path().to_path_buf());
        let github = GitHubClient::anonymous().unwrap();
        let cmd = BuildCommand::new(&github, &registry, &config, Some(store));

        let out = TempDir::new().unwrap();
        let request = BuildRequest::new("test", format!("commit:{TEST_SHA}"), out.path())
            .version(Some("1.0.0".to_string()));
        let resolved = resolved_commit();
        let project_info = registry.get("test").unwrap();

        let output = cmd
            .try_cache_fast_path(&request, project_info, &resolved, &NullReporter, false)
            .await
            .expect("expected a full cache hit");

        assert!(output.from_cache(), "all artifacts should be cache-origin");
        assert_eq!(output.result.artifacts.len(), 1);
        assert_eq!(output.result.artifacts[0].origin, ArtifactOrigin::Cache);
        assert_eq!(output.result.version, "1.0.0");
        assert!(out.path().join("plugin.zip").is_file());
    }

    #[tokio::test]
    async fn fast_path_skipped_when_no_cache_flag_set() {
        let (cache, store) = seed_single_output("test", "1.0.0");
        let mut registry = ProjectRegistry::new();
        registry.register(single_output_project("test", Box::new(TestBuilder)));
        let config = Config::new(cache.path().to_path_buf());
        let github = GitHubClient::anonymous().unwrap();
        let cmd = BuildCommand::new(&github, &registry, &config, Some(store));

        let out = TempDir::new().unwrap();
        let request = BuildRequest::new("test", format!("commit:{TEST_SHA}"), out.path())
            .version(Some("1.0.0".to_string()))
            .no_cache(true);
        let resolved = resolved_commit();
        let project_info = registry.get("test").unwrap();

        let output = cmd
            .try_cache_fast_path(&request, project_info, &resolved, &NullReporter, false)
            .await;
        assert!(output.is_none(), "--no-cache must bypass the fast path");
    }

    #[tokio::test]
    async fn fast_path_no_pin_with_builder_default_misses_a_different_version() {
        // No `--ver`, but the builder has a default (9.99.99). The cache holds
        // 5.6.0, so the predicted-and-required version (9.99.99) does not match:
        // the fast path misses and the caller rebuilds at the default version.
        let (cache, store) = seed_single_output("test", "5.6.0");
        let mut registry = ProjectRegistry::new();
        registry.register(single_output_project(
            "test",
            Box::new(DefaultVersionBuilder),
        ));
        let config = Config::new(cache.path().to_path_buf());
        let github = GitHubClient::anonymous().unwrap();
        let cmd = BuildCommand::new(&github, &registry, &config, Some(store));

        let out = TempDir::new().unwrap();
        // No `.version(...)` → the builder default (9.99.99) is what would build.
        let request = BuildRequest::new("test", format!("commit:{TEST_SHA}"), out.path());
        let resolved = resolved_commit();
        let project_info = registry.get("test").unwrap();

        let output = cmd
            .try_cache_fast_path(&request, project_info, &resolved, &NullReporter, false)
            .await;

        assert!(
            output.is_none(),
            "cached 5.6.0 must not satisfy a default-9.99.99 build"
        );
        assert!(
            std::fs::read_dir(out.path()).unwrap().next().is_none(),
            "a version mismatch must not deliver anything"
        );
    }

    #[tokio::test]
    async fn fast_path_no_predictable_version_skips_the_fast_path() {
        // An optional-version builder with no default and no `--ver`: the
        // version cannot be known before checkout, so the fast path is skipped
        // entirely (the caller clones and lets post-checkout reuse decide),
        // even though the commit IS cached.
        let (cache, store) = seed_single_output("test", "1.0.0");
        let mut registry = ProjectRegistry::new();
        registry.register(single_output_project(
            "test",
            Box::new(OptionalNoDefaultBuilder),
        ));
        let config = Config::new(cache.path().to_path_buf());
        let github = GitHubClient::anonymous().unwrap();
        let cmd = BuildCommand::new(&github, &registry, &config, Some(store));

        let out = TempDir::new().unwrap();
        let request = BuildRequest::new("test", format!("commit:{TEST_SHA}"), out.path());
        let resolved = resolved_commit();
        let project_info = registry.get("test").unwrap();

        let output = cmd
            .try_cache_fast_path(&request, project_info, &resolved, &NullReporter, false)
            .await;
        assert!(
            output.is_none(),
            "an unpredictable version must skip the pre-clone fast path"
        );
    }

    #[tokio::test]
    async fn fast_path_pinned_wrong_version_misses() {
        // User pinned 2.0.0, cache holds 1.0.0 → miss (a different cached
        // version is never served), so the caller rebuilds at 2.0.0.
        let (cache, store) = seed_single_output("test", "1.0.0");
        let mut registry = ProjectRegistry::new();
        registry.register(single_output_project("test", Box::new(TestBuilder)));
        let config = Config::new(cache.path().to_path_buf());
        let github = GitHubClient::anonymous().unwrap();
        let cmd = BuildCommand::new(&github, &registry, &config, Some(store));

        let out = TempDir::new().unwrap();
        let request = BuildRequest::new("test", format!("commit:{TEST_SHA}"), out.path())
            .version(Some("2.0.0".to_string()));
        let resolved = resolved_commit();
        let project_info = registry.get("test").unwrap();

        let output = cmd
            .try_cache_fast_path(&request, project_info, &resolved, &NullReporter, false)
            .await;
        assert!(
            output.is_none(),
            "pinned 2.0.0 but cache holds 1.0.0 ⇒ miss"
        );
        assert!(
            std::fs::read_dir(out.path()).unwrap().next().is_none(),
            "a version mismatch must not deliver anything"
        );
    }

    #[tokio::test]
    async fn fast_path_warm_hit_references_cache_without_output() {
        // A warm-mode full hit: the commit is already cached, so the artifacts
        // reference the cache and NOTHING is written to the output directory.
        let (cache, store) = seed_single_output("test", "1.0.0");
        let mut registry = ProjectRegistry::new();
        registry.register(single_output_project("test", Box::new(TestBuilder)));
        let config = Config::new(cache.path().to_path_buf());
        let github = GitHubClient::anonymous().unwrap();
        let cmd = BuildCommand::new(&github, &registry, &config, Some(store));

        let out = TempDir::new().unwrap();
        let request = BuildRequest::new("test", format!("commit:{TEST_SHA}"), out.path())
            .version(Some("1.0.0".to_string()));
        let resolved = resolved_commit();
        let project_info = registry.get("test").unwrap();

        let output = cmd
            .try_cache_fast_path(&request, project_info, &resolved, &NullReporter, true)
            .await
            .expect("expected a full cache hit");

        assert!(output.from_cache());
        assert_eq!(output.result.artifacts.len(), 1);
        assert_eq!(output.result.artifacts[0].origin, ArtifactOrigin::Cache);
        // The artifact references the in-cache file, not the output directory.
        assert!(
            output.result.artifacts[0].path.starts_with(cache.path()),
            "warm artifact must reference the in-cache file"
        );
        assert!(output.result.artifacts[0].path.is_file());
        // Nothing was written to the output directory.
        assert!(
            std::fs::read_dir(out.path()).unwrap().next().is_none(),
            "warm fast-path must not write to the output directory"
        );
    }

    // =========================================================================
    // Pre-clone version detection: parse fetched source files with the real
    // builder's `detect_version`, and the empty-files short-circuit (no network).
    // =========================================================================

    #[test]
    fn detect_version_from_files_reads_the_plugin_header() {
        // The real WP Rocket builder reads `wp-rocket.php`; feeding it that file
        // (as it would be fetched at a commit) yields the header version — the
        // same result a full checkout would produce.
        let php = "<?php\n/**\n * Plugin Name: WP Rocket\n * Version: 3.17.4\n */\n";
        let files = vec![("wp-rocket.php", php.as_bytes().to_vec())];
        let version = detect_version_from_files(&crate::build::plugins::WpRocketBuilder, &files);
        assert_eq!(version.as_deref(), Some("3.17.4"));
    }

    #[test]
    fn detect_version_from_files_none_when_header_missing() {
        // A file present but with no version header → no detection (the caller
        // then clones), never a panic or a bogus version.
        let files = vec![("wp-rocket.php", b"<?php // no header\n".to_vec())];
        let version = detect_version_from_files(&crate::build::plugins::WpRocketBuilder, &files);
        assert_eq!(version, None);
    }

    #[test]
    fn detect_version_from_files_none_when_wrong_file_supplied() {
        // Supplying an unrelated file (not the one `detect_version` reads) must
        // not yield a version — the builder looks for `wp-rocket.php`.
        let files = vec![("README.md", b"# unrelated\nVersion: 9.9.9\n".to_vec())];
        let version = detect_version_from_files(&crate::build::plugins::WpRocketBuilder, &files);
        assert_eq!(version, None);
    }

    #[tokio::test]
    async fn detect_version_pre_clone_short_circuits_without_version_files() {
        // A builder that declares no version source files never touches the
        // network — detection returns None immediately and the caller clones.
        let mut registry = ProjectRegistry::new();
        registry.register(single_output_project("test", Box::new(TestBuilder)));
        let config = Config::new(PathBuf::from("/tmp/cache"));
        let github = GitHubClient::anonymous().unwrap();
        let cmd = BuildCommand::new(&github, &registry, &config, None);
        let project_info = registry.get("test").unwrap();

        let detected = cmd
            .detect_version_pre_clone(project_info, TEST_SHA, &NullReporter)
            .await;
        assert!(
            detected.is_none(),
            "no version_source_files ⇒ no detection, no network call"
        );
    }

    #[test]
    fn warm_request_into_build_request_sets_fixed_defaults() {
        let request = WarmRequest::new("backwpup", "pr:123")
            .version(Some("5.1.0".to_string()))
            .variants(vec!["free".to_string()])
            .into_build_request();

        // Carried-over fields.
        assert_eq!(request.project, "backwpup");
        assert_eq!(request.git_ref, "pr:123");
        assert_eq!(request.version.as_deref(), Some("5.1.0"));
        assert_eq!(request.variants, vec!["free".to_string()]);

        // The two fields `WarmRequest` omits get their only sensible warm
        // values — this is what makes warming impossible to misconfigure.
        assert_eq!(request.output_dir, PathBuf::new(), "no output directory");
        assert!(
            !request.no_cache,
            "warming must consult + populate the cache"
        );
    }

    #[tokio::test]
    async fn test_private_repo_fails_without_token() {
        // Setup: Create registry with a PRIVATE project
        let mut registry = ProjectRegistry::new();
        registry.register(Project {
            name: "test-private".to_string(),
            repo_url: "https://github.com/test/private-repo.git".to_string(),
            owner: "test".to_string(),
            repo: "private-repo".to_string(),
            default_branch: "main".to_string(),
            is_private: true,
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        // Config WITHOUT token
        let config = Config::new(PathBuf::from("/tmp/cache"));

        // GitHub client (anonymous - no token)
        let github = GitHubClient::anonymous().unwrap();

        // Create the command
        let cmd = BuildCommand::new(&github, &registry, &config, None);

        // Execute should fail IMMEDIATELY with PrivateRepoNoToken error
        let output_dir = TempDir::new().unwrap();
        let result = cmd
            .execute(
                BuildRequest::new("test-private", "main", output_dir.path())
                    .version(Some("1.0.0".to_string())),
                &NullReporter,
                false,
            )
            .await;

        // Verify it's the correct error type
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_string = err.to_string();

        assert!(
            err_string.contains("private and requires a GitHub token"),
            "Expected PrivateRepoNoToken error, got: {}",
            err_string
        );
        assert!(
            err_string.contains("test/private-repo"),
            "Expected repo name in error, got: {}",
            err_string
        );
    }

    #[tokio::test]
    async fn test_public_repo_does_not_require_token() {
        // Setup: Create registry with a PUBLIC project
        let mut registry = ProjectRegistry::new();
        registry.register(Project {
            name: "test-public".to_string(),
            repo_url: "https://github.com/test/public-repo.git".to_string(),
            owner: "test".to_string(),
            repo: "public-repo".to_string(),
            default_branch: "main".to_string(),
            is_private: false, // PUBLIC repo
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        // Config WITHOUT token
        let config = Config::new(PathBuf::from("/tmp/cache"));

        // GitHub client (anonymous - no token)
        let github = GitHubClient::anonymous().unwrap();

        // Create the command
        let cmd = BuildCommand::new(&github, &registry, &config, None);

        // Execute should NOT fail with PrivateRepoNoToken error
        // (it will fail later because the repo doesn't exist, but that's fine)
        let output_dir = TempDir::new().unwrap();
        let result = cmd
            .execute(
                BuildRequest::new("test-public", "main", output_dir.path())
                    .version(Some("1.0.0".to_string())),
                &NullReporter,
                false,
            )
            .await;

        // The error should NOT be PrivateRepoNoToken
        if let Err(e) = result {
            let err_string = e.to_string();
            assert!(
                !err_string.contains("private and requires a GitHub token"),
                "Public repo should not trigger PrivateRepoNoToken error, got: {}",
                err_string
            );
        }
        // If it somehow succeeds (shouldn't with fake repo), that's also fine
    }

    // =========================================================================
    // version_from_tag tests
    // =========================================================================

    #[test]
    fn test_version_from_tag_strips_lowercase_v() {
        assert_eq!(version_from_tag("v5.6.8"), "5.6.8");
        assert_eq!(version_from_tag("v3.21.1"), "3.21.1");
        assert_eq!(version_from_tag("v0.1.0"), "0.1.0");
    }

    #[test]
    fn test_version_from_tag_strips_uppercase_v() {
        assert_eq!(version_from_tag("V5.6.8"), "5.6.8");
        assert_eq!(version_from_tag("V3.21.1"), "3.21.1");
    }

    #[test]
    fn test_version_from_tag_strips_release_dash() {
        assert_eq!(version_from_tag("release-1.0.0"), "1.0.0");
        assert_eq!(version_from_tag("release-2.3.4"), "2.3.4");
    }

    #[test]
    fn test_version_from_tag_strips_release_slash() {
        assert_eq!(version_from_tag("release/1.0.0"), "1.0.0");
        assert_eq!(version_from_tag("release/2.3.4"), "2.3.4");
    }

    #[test]
    fn test_version_from_tag_leaves_bare_version() {
        assert_eq!(version_from_tag("5.6.8"), "5.6.8");
        assert_eq!(version_from_tag("1.0"), "1.0");
        assert_eq!(version_from_tag("3.22.3.1"), "3.22.3.1");
    }

    #[test]
    fn test_version_from_tag_non_version_strings() {
        // These don't have a known prefix, so they are returned as-is
        assert_eq!(version_from_tag("develop"), "develop");
        assert_eq!(version_from_tag("main"), "main");
        assert_eq!(version_from_tag("feature/v2"), "feature/v2");
    }

    // =========================================================================
    // storable_version tests
    // =========================================================================

    #[test]
    fn test_storable_version_accepts_store_safe_strings() {
        assert_eq!(storable_version("5.6.8"), Some("5.6.8".to_string()));
        assert_eq!(
            storable_version("5.6.8-beta.1+build_2"),
            Some("5.6.8-beta.1+build_2".to_string())
        );
        assert_eq!(storable_version("nightly"), Some("nightly".to_string()));
    }

    #[test]
    fn test_storable_version_drops_store_unsafe_strings() {
        assert_eq!(storable_version(""), None);
        assert_eq!(storable_version("5.6.8 (beta)"), None); // spaces/parens
        assert_eq!(storable_version("feature/v2"), None); // slash
        assert_eq!(storable_version(&"9".repeat(65)), None); // too long
    }

    // =========================================================================
    // looks_like_version_tag tests
    // =========================================================================

    #[test]
    fn test_looks_like_version_tag_bare_versions() {
        assert!(looks_like_version_tag("5.6.8"));
        assert!(looks_like_version_tag("1.0"));
        assert!(looks_like_version_tag("3.22.3.1"));
        assert!(looks_like_version_tag("0.0.1"));
        assert!(looks_like_version_tag("10.20.30"));
    }

    #[test]
    fn test_looks_like_version_tag_with_v_prefix() {
        assert!(looks_like_version_tag("v5.6.8"));
        assert!(looks_like_version_tag("V3.21.1"));
        assert!(looks_like_version_tag("v0.1.0"));
        assert!(looks_like_version_tag("V10.20.30"));
    }

    #[test]
    fn test_looks_like_version_tag_with_release_prefix() {
        assert!(looks_like_version_tag("release-1.0.0"));
        assert!(looks_like_version_tag("release/2.0.0"));
    }

    #[test]
    fn test_looks_like_version_tag_rejects_non_version() {
        assert!(!looks_like_version_tag("develop"));
        assert!(!looks_like_version_tag("main"));
        assert!(!looks_like_version_tag("feature/foo"));
        assert!(!looks_like_version_tag("abc1234"));
        assert!(!looks_like_version_tag("123"));
        assert!(!looks_like_version_tag("v1"));
        assert!(!looks_like_version_tag(""));
    }

    #[test]
    fn test_looks_like_version_tag_rejects_malformed() {
        // Leading/trailing dots
        assert!(!looks_like_version_tag(".1.0"));
        assert!(!looks_like_version_tag("1.0."));
        assert!(!looks_like_version_tag("v.1.0"));
        // Empty segments
        assert!(!looks_like_version_tag("1..0"));
        // Non-digit characters in version part
        assert!(!looks_like_version_tag("1.0.0-beta"));
        assert!(!looks_like_version_tag("v1.0.0-rc1"));
        assert!(!looks_like_version_tag("1.0.0a"));
    }

    #[test]
    fn test_looks_like_version_tag_needs_at_least_two_segments() {
        assert!(looks_like_version_tag("1.0"));
        assert!(looks_like_version_tag("1.0.0"));
        assert!(looks_like_version_tag("1.0.0.0"));
        // Single segment is not a version
        assert!(!looks_like_version_tag("v1"));
        assert!(!looks_like_version_tag("123"));
    }

    // =========================================================================
    // is_release_keyword tests
    // =========================================================================

    #[test]
    fn test_is_release_keyword_recognized() {
        assert!(is_release_keyword("latest-stable"));
        assert!(is_release_keyword("previous-stable"));
        assert!(is_release_keyword("latest"));
        assert!(is_release_keyword("previous-latest"));
    }

    #[test]
    fn test_is_release_keyword_case_insensitive() {
        assert!(is_release_keyword("Latest-Stable"));
        assert!(is_release_keyword("LATEST"));
        assert!(is_release_keyword("Previous-Latest"));
        assert!(is_release_keyword("PREVIOUS-STABLE"));
    }

    #[test]
    fn test_is_release_keyword_rejects_non_keywords() {
        assert!(!is_release_keyword("v5.6.8"));
        assert!(!is_release_keyword("5.6.8"));
        assert!(!is_release_keyword("stable"));
        assert!(!is_release_keyword("previous"));
        assert!(!is_release_keyword(""));
        assert!(!is_release_keyword("latest-"));
        assert!(!is_release_keyword("some-tag"));
    }

    // =========================================================================
    // BuildOutput tests
    // =========================================================================

    use crate::git::{RefSource, ResolvedRef};

    /// Helper: create a minimal BuildOutput for testing.
    fn make_build_output(
        artifacts: Vec<ProducedArtifact>,
        version: &str,
        source: RefSource,
        commit: &str,
        branch: &str,
    ) -> BuildOutput {
        BuildOutput {
            result: crate::build::BuildResult::new(
                artifacts,
                std::path::PathBuf::from("/build"),
                version.to_string(),
                vec![],
            ),
            resolved_ref: ResolvedRef {
                input: "test-input".to_string(),
                source,
                git_ref: branch.to_string(),
                commit_sha: Some(commit.to_string()),
            },
            commit: commit.to_string(),
            commit_short: commit[..7].to_string(),
            branch: branch.to_string(),
            version_override: None,
        }
    }

    #[test]
    fn test_build_output_to_build_metadata() {
        let output = make_build_output(
            vec![],
            "3.17.4",
            RefSource::PullRequest(99),
            "abc1234567890",
            "develop",
        );

        let meta = output.to_build_metadata("wp-rocket");
        assert_eq!(meta.project, "wp-rocket");
        assert_eq!(meta.version, "3.17.4");
        assert_eq!(meta.commit, "abc1234567890");
        assert_eq!(meta.branch.as_deref(), Some("develop"));
    }

    // =========================================================================
    // BuildOutput::from_cache — derived from artifact provenance
    // =========================================================================

    fn artifact(name: &str, origin: ArtifactOrigin) -> ProducedArtifact {
        ProducedArtifact::new(None, PathBuf::from(format!("/{name}")), name.to_string(), 1)
            .with_origin(origin)
    }

    #[test]
    fn from_cache_false_when_all_built() {
        let output = make_build_output(
            vec![artifact("a.zip", ArtifactOrigin::Built)],
            "1.0.0",
            RefSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );
        assert!(!output.from_cache());
    }

    #[test]
    fn from_cache_true_when_all_cached() {
        let output = make_build_output(
            vec![
                artifact("a.zip", ArtifactOrigin::Cache),
                artifact("b.zip", ArtifactOrigin::Cache),
            ],
            "1.0.0",
            RefSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );
        assert!(output.from_cache());
    }

    #[test]
    fn from_cache_false_when_mixed_partial() {
        // A partial build (some reused, some freshly built) is not "from cache".
        let output = make_build_output(
            vec![
                artifact("a.zip", ArtifactOrigin::Cache),
                artifact("b.zip", ArtifactOrigin::Built),
            ],
            "1.0.0",
            RefSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );
        assert!(!output.from_cache());
    }

    #[test]
    fn from_cache_false_when_no_artifacts() {
        let output = make_build_output(
            vec![],
            "1.0.0",
            RefSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );
        assert!(!output.from_cache());
    }

    #[test]
    fn test_build_output_to_source_artifacts() {
        let output = make_build_output(
            vec![
                ProducedArtifact::new(
                    Some("free".into()),
                    std::path::PathBuf::from("/build/free.zip"),
                    "free.zip".into(),
                    100,
                ),
                ProducedArtifact::new(
                    Some("pro".into()),
                    std::path::PathBuf::from("/build/pro.zip"),
                    "pro.zip".into(),
                    200,
                ),
            ],
            "5.1.0",
            RefSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );

        let artifacts = output.to_source_artifacts();
        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].variant_id, Some("free".to_string()));
        assert_eq!(artifacts[0].target_name, "free.zip");
        assert_eq!(artifacts[1].variant_id, Some("pro".to_string()));
    }

    #[test]
    fn test_build_output_source() {
        let output = make_build_output(
            vec![],
            "3.17.4",
            RefSource::PullRequest(42),
            "abc1234567890",
            "develop",
        );
        assert_eq!(*output.source(), RefSource::PullRequest(42));
    }

    #[test]
    fn test_build_output_description_pr() {
        let output = make_build_output(
            vec![],
            "3.17.4",
            RefSource::PullRequest(42),
            "abc1234567890",
            "feature/foo",
        );
        let desc = output.description();
        assert!(desc.contains("PR #42"));
        assert!(desc.contains("abc1234"));
    }

    #[test]
    fn test_build_output_description_branch() {
        let output = make_build_output(
            vec![],
            "3.17.4",
            RefSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );
        let desc = output.description();
        assert!(desc.contains("develop"));
        assert!(desc.contains("abc1234"));
    }

    #[test]
    fn test_build_output_exists_in_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = apvm_storage::ArtifactStore::open(dir.path()).unwrap();

        let output = make_build_output(
            vec![],
            "3.17.4",
            RefSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );

        // Nothing stored yet
        assert!(!output.exists_in_store(&store, "wp-rocket").unwrap());
    }

    #[test]
    fn test_build_output_missing_variants_empty_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = apvm_storage::ArtifactStore::open(dir.path()).unwrap();

        let output = make_build_output(
            vec![ProducedArtifact::new(
                Some("free".into()),
                std::path::PathBuf::from("/free.zip"),
                "free.zip".into(),
                100,
            )],
            "5.1.0",
            RefSource::Branch("develop".into()),
            "abc1234567890",
            "develop",
        );

        let missing = output.missing_variants(&store, "backwpup").unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], Some("free".to_string()));
    }
}
