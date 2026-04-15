//! Build command implementation.
//!
//! This module provides the main build command that handles building a project
//! from any git reference (PR, branch, tag, or commit) with automatic detection.

use crate::build::plugins::VersionRequirement;
use crate::build::progress::{BuildEvent, BuildPhase, BuildStep, ProgressReporter};
use crate::build::{BuildResult, BuildRunner, ProducedArtifact};
use crate::error::{Error, Result};
use crate::git::{BuildWorkspace, RefResolver, RefSource, ResolvedRef};
use crate::github::GitHubClient;
use crate::github::client::download_asset_owned;
use crate::projects::ProjectRegistry;
use apvm_config::Config;
use std::path::Path;
use tokio::task::JoinSet;

// Re-export storage types for convenience (consumers don't need to add apvm-storage)
pub use apvm_storage::{BuildMetadata, SourceArtifact};

/// Extended build result with git metadata.
///
/// Contains the build artifacts plus all the metadata needed
/// for storage operations (commit SHA, source type, branch name).
///
/// # Storage Integration
///
/// This type provides conversion methods to transform build output into
/// storage-compatible formats. The design follows composition over integration:
/// core builds, consumer decides whether/how to store.
///
/// ```ignore
/// let output = apvm.build("backwpup", "5.6.0", "pr:123", None).await?;
///
/// // Convert for storage (if consumer wants to store)
/// let store = ArtifactStore::new(config.builds_dir);
/// store.store(
///     &output.to_source_artifacts(),
///     &output.to_build_metadata("backwpup"),
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
}

impl BuildOutput {
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
            &self.commit_short
        )
    }

    // =========================================================================
    // Storage Conversion Helpers
    // =========================================================================

    /// Convert to storage metadata.
    ///
    /// Creates a [`BuildMetadata`] struct suitable for passing to
    /// [`ArtifactStore::store()`].
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
    /// for passing to [`ArtifactStore::store()`].
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
    /// let store = ArtifactStore::new(config.builds_dir);
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
    ///     store.store(&new_artifacts, &output.to_build_metadata("backwpup"))?;
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
/// - `branch:main` → Force branch interpretation
/// - `commit:a1b2c3d` → Force commit interpretation
/// - `release:5.6.8` → Download pre-built assets from GitHub Release
///
/// Version-like bare inputs (e.g., `5.6.8`, `v3.21.1`) are automatically
/// checked against the GitHub Release API before cloning, if the project
/// has releases enabled.
pub struct BuildCommand<'a> {
    github: &'a GitHubClient,
    registry: &'a ProjectRegistry,
    config: &'a Config,
}

impl<'a> BuildCommand<'a> {
    /// Create a new build command.
    pub fn new(
        github: &'a GitHubClient,
        registry: &'a ProjectRegistry,
        config: &'a Config,
    ) -> Self {
        Self {
            github,
            registry,
            config,
        }
    }

    /// Execute the build command with automatic ref detection.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection if builder have it implemented
    /// * `git_ref` - Git reference (PR number, branch, tag, or commit)
    /// * `variants` - Specific variants to build (empty = all)
    /// * `output_dir` - Directory where build artifacts will be placed
    /// * `reporter` - Progress reporter for receiving build events
    ///
    /// # Errors
    ///
    /// Returns [`Error::PrivateRepoNoToken`] if the project's repository is private
    /// and no GitHub token is configured. This check happens early to provide a
    /// clear error message before any git operations are attempted.
    pub async fn execute(
        &self,
        project: &str,
        version: Option<&str>,
        git_ref: &str,
        variants: &[&str],
        output_dir: impl AsRef<Path>,
        reporter: &dyn ProgressReporter,
    ) -> Result<BuildOutput> {
        let output_dir = output_dir.as_ref();

        // 1. Look up project in registry
        let project_info = self.registry.get(project)?;

        // 2. Validate authentication for private repositories (fail-fast)
        //
        // This check happens BEFORE any git operations to provide a clear,
        // actionable error message. Without this, users would see cryptic
        // git errors like "Authentication failed" or "Repository not found".
        if project_info.is_private && self.config.github_token.is_none() {
            return Err(Error::PrivateRepoNoToken {
                repo: format!("{}/{}", project_info.owner, project_info.repo),
            });
        }

        let builder = project_info.builder.as_ref();

        // 3. Early-resolve GitHub refs that don't need a local repo.
        //
        // For PR references (either explicit `pr:123` or bare digits `123`),
        // we verify the PR exists via the GitHub API BEFORE cloning. This
        // avoids wasting time on a full clone when the PR doesn't exist.
        //
        // For release references (`release:TAG`), we fetch the release from
        // the GitHub API and download pre-built assets directly, bypassing
        // the clone → build pipeline entirely.
        //
        // - Explicit `pr:123`: fails immediately if PR is not found.
        // - Bare digits `123` (auto-detect): on failure, continues to clone
        //   and falls back to branch/tag/commit resolution.
        // - Explicit `release:TAG`: fetches release, downloads assets, returns early.
        let early_resolved = self
            .try_early_resolve_github_ref(
                git_ref,
                &project_info.owner,
                &project_info.repo,
                project_info.has_releases,
                reporter,
            )
            .await?;

        // 3b. Handle release refs — download pre-built assets, skip clone/build entirely.
        if let Some(ref resolved) = early_resolved
            && let RefSource::Release(ref tag) = resolved.source
        {
            return self
                .download_release(
                    project_info,
                    tag,
                    git_ref,
                    version,
                    variants,
                    output_dir,
                    reporter,
                )
                .await;
        }

        // 4. Create isolated build workspace (auto-cleaned on drop)
        let workspace = BuildWorkspace::new(
            &project_info.name,
            &project_info.repo_url,
            self.config.github_token.as_deref(),
        )?;

        // Clone the repository into the temp workspace
        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Clone,
            message: format!("Cloning {}", project_info.repo_url),
        });
        workspace.clone_repo().await?;

        // Fetch latest refs (uses token if available)
        workspace.fetch().await?;
        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Clone,
        });

        // Get repository handle for local operations
        let repo = workspace.repository();

        // 5. Resolve the git reference — skip if already resolved early.
        let resolved = match early_resolved {
            Some(r) => {
                tracing::debug!(
                    "Using early-resolved ref: {} → {}",
                    r.source.description(),
                    r.git_ref
                );
                r
            }
            None => {
                let resolver =
                    RefResolver::new(self.github, &project_info.owner, &project_info.repo)
                        .with_repo_path(repo.path());
                resolver.resolve(git_ref).await?
            }
        };

        // 6. Prepare repository for checkout (clean state)
        // Reset FIRST to avoid checkout failures due to uncommitted changes.
        // See: https://git-scm.com/docs/git-checkout#_description
        // "git checkout refuses to switch branches if there are local modifications"
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

        // 7. Checkout the resolved ref
        repo.checkout(&resolved.git_ref).await?;

        // 8. Get the commit SHA after checkout
        let (commit, commit_short) = repo.get_head_commit_pair().await?;
        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Checkout,
        });

        // 9. Resolve version (AFTER checkout so detect_version sees correct files)
        let resolved_version = self.resolve_version(project, version, builder, repo.path())?;

        tracing::info!(
            "Building {} v{} from {}",
            project,
            resolved_version,
            resolved.detailed_description()
        );

        if variants.is_empty() {
            tracing::info!("Building all variants");
        } else {
            tracing::info!("Building variants: {}", variants.join(", "));
        }

        // Determine branch name based on source type
        let branch = match &resolved.source {
            RefSource::PullRequest(_) | RefSource::Branch(_) => resolved.git_ref.clone(),
            RefSource::Tag(tag) => format!("tag/{tag}"),
            RefSource::Commit(sha) => format!("commit/{}", &sha[..7.min(sha.len())]),
            RefSource::Release(tag) => format!("release/{tag}"),
        };

        tracing::debug!(
            "Checked out {} at commit {}",
            resolved.git_ref,
            commit_short
        );

        // 10. Run the build using the project's builder
        let build_context = workspace.to_build_context();
        let mut runner = BuildRunner::with_reporter(build_context, reporter);
        let result = runner
            .execute_build(builder, &resolved_version, variants)
            .await?;

        // 11. Collect artifacts to output_dir BEFORE workspace cleanup
        // The workspace will be automatically deleted when it goes out of scope,
        // so we must move artifacts out first.
        let artifact_paths: Vec<_> = result.artifacts.iter().map(|a| a.path.clone()).collect();
        workspace.collect_artifacts(&artifact_paths, output_dir)?;

        // Update artifact paths to point to output_dir
        let mut result = result;
        for artifact in &mut result.artifacts {
            artifact.path = output_dir.join(&artifact.filename);
        }

        // Workspace is automatically cleaned up here when it goes out of scope
        Ok(BuildOutput {
            result,
            resolved_ref: resolved,
            commit,
            commit_short,
            branch,
        })
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
    /// | `5.6.8` / `v5.6.8` (version-like, `has_releases=true`) | Yes | Return `Ok(None)` (fallback to clone) |
    /// | `branch:main`  | No           | N/A (needs local repo)            |
    /// | `tag:v1.0.0`   | No           | N/A (needs local repo)            |
    /// | `develop`      | No           | N/A (needs local repo)            |
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
                        // Release references are resolved immediately — return as ResolvedRef
                        // for the caller to handle via download_release().
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
                Ok(Some(ResolvedRef {
                    input: trimmed.to_string(),
                    source: RefSource::PullRequest(pr_number),
                    git_ref: pr.head_branch,
                    commit_sha: None,
                }))
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

        // Download assets in parallel
        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::ReleaseDownload,
            message: format!(
                "Downloading {} asset(s) from release '{tag}'",
                matched_assets.len()
            ),
        });

        // Create output dir if it doesn't exist
        std::fs::create_dir_all(output_dir).map_err(|e| {
            Error::Build(format!(
                "Failed to create output directory '{}': {e}",
                output_dir.display()
            ))
        })?;

        // Download assets in parallel using tokio::task::JoinSet.
        //
        // JoinSet::spawn requires `Future + Send + 'static`, so we use the
        // standalone `download_asset_owned` function which takes all data by
        // ownership. Each download runs on its own tokio worker thread.
        //
        // On first error, remaining tasks are aborted (JoinSet is dropped).
        //
        // Sources:
        // - JoinSet::spawn: https://docs.rs/tokio/1.49.0/tokio/task/struct.JoinSet.html#method.spawn
        // - JoinSet::join_next: https://docs.rs/tokio/1.49.0/tokio/task/struct.JoinSet.html#method.join_next
        let token = self.github.token();
        let mut download_tasks: JoinSet<Result<(String, Vec<u8>)>> = JoinSet::new();

        for asset in &matched_assets {
            reporter.report(&BuildEvent::StepStarted {
                step: BuildStep::new(
                    format!("Downloading {}", &asset.name),
                    format!("GET {}", &asset.download_url),
                ),
            });

            download_tasks.spawn(download_asset_owned(
                token.clone(),
                owner.to_string(),
                repo.to_string(),
                (*asset).clone(),
            ));
        }

        // Collect results as they complete. Abort all remaining tasks on
        // first error to avoid wasting bandwidth.
        let mut artifacts = Vec::new();
        let mut variants_built = Vec::new();

        while let Some(join_result) = download_tasks.join_next().await {
            // Handle JoinError (task panic or cancellation).
            let download_result =
                join_result.map_err(|e| Error::Build(format!("Download task failed: {e}")))?;

            // Handle download errors from the HTTP request.
            let (asset_name, bytes) = download_result?;

            let dest = output_dir.join(&asset_name);
            std::fs::write(&dest, &bytes).map_err(|e| {
                Error::Build(format!("Failed to write asset '{}': {e}", dest.display()))
            })?;

            let variant_id = builder.variant_from_release_asset(&asset_name);
            if let Some(ref v) = variant_id
                && !variants_built.contains(v)
            {
                variants_built.push(v.clone());
            }

            reporter.report(&BuildEvent::StepCompleted {
                step: BuildStep::new(
                    format!("Downloaded {asset_name}"),
                    format!("{} bytes", bytes.len()),
                ),
            });

            artifacts.push(ProducedArtifact::new(
                variant_id,
                dest,
                asset_name,
                bytes.len() as u64,
            ));
        }

        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::ReleaseDownload,
        });

        // The version is always derived from the release tag, not the CLI --ver arg.
        // Release assets are pre-built at a specific version embedded in the tag name.
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

        Ok(BuildOutput {
            result,
            resolved_ref,
            commit: format!("release-{tag}"),
            commit_short: tag.to_string(),
            branch: format!("release/{tag}"),
        })
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

    /// Execute a build from a specific PR number.
    ///
    /// This is a convenience method equivalent to `execute(project, version, "pr:{pr_number}", variants, output_dir, reporter)`.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `pr_number` - Pull request number
    /// * `variants` - Specific variants to build (empty = all)
    /// * `output_dir` - Directory where build artifacts will be placed
    /// * `reporter` - Progress reporter for receiving build events
    pub async fn execute_pr(
        &self,
        project: &str,
        version: Option<&str>,
        pr_number: u64,
        variants: &[&str],
        output_dir: impl AsRef<Path>,
        reporter: &dyn ProgressReporter,
    ) -> Result<BuildOutput> {
        self.execute(
            project,
            version,
            &format!("pr:{pr_number}"),
            variants,
            output_dir,
            reporter,
        )
        .await
    }

    /// Execute a build from a specific branch.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `branch` - Branch name
    /// * `variants` - Specific variants to build (empty = all)
    /// * `output_dir` - Directory where build artifacts will be placed
    /// * `reporter` - Progress reporter for receiving build events
    pub async fn execute_branch(
        &self,
        project: &str,
        version: Option<&str>,
        branch: &str,
        variants: &[&str],
        output_dir: impl AsRef<Path>,
        reporter: &dyn ProgressReporter,
    ) -> Result<BuildOutput> {
        self.execute(
            project,
            version,
            &format!("branch:{branch}"),
            variants,
            output_dir,
            reporter,
        )
        .await
    }

    /// Execute a build from a specific tag.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `tag` - Tag name (e.g., "v1.0.0")
    /// * `variants` - Specific variants to build (empty = all)
    /// * `output_dir` - Directory where build artifacts will be placed
    /// * `reporter` - Progress reporter for receiving build events
    pub async fn execute_tag(
        &self,
        project: &str,
        version: Option<&str>,
        tag: &str,
        variants: &[&str],
        output_dir: impl AsRef<Path>,
        reporter: &dyn ProgressReporter,
    ) -> Result<BuildOutput> {
        self.execute(
            project,
            version,
            &format!("tag:{tag}"),
            variants,
            output_dir,
            reporter,
        )
        .await
    }

    /// Execute a build from a specific commit SHA.
    ///
    /// # Arguments
    ///
    /// * `project` - Project name from registry
    /// * `version` - Version to build, or `None` for auto-detection
    /// * `commit` - Commit SHA (minimum 7 characters)
    /// * `variants` - Specific variants to build (empty = all)
    /// * `output_dir` - Directory where build artifacts will be placed
    /// * `reporter` - Progress reporter for receiving build events
    pub async fn execute_commit(
        &self,
        project: &str,
        version: Option<&str>,
        commit: &str,
        variants: &[&str],
        output_dir: impl AsRef<Path>,
        reporter: &dyn ProgressReporter,
    ) -> Result<BuildOutput> {
        self.execute(
            project,
            version,
            &format!("commit:{commit}"),
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
        let config = Config::new(PathBuf::from("/tmp/builds"));

        // GitHub client (anonymous - no token)
        let github = GitHubClient::anonymous().unwrap();

        // Create the command
        let cmd = BuildCommand::new(&github, &registry, &config);

        // Execute should fail IMMEDIATELY with PrivateRepoNoToken error
        let output_dir = TempDir::new().unwrap();
        let result = cmd
            .execute(
                "test-private",
                Some("1.0.0"),
                "main",
                &[],
                output_dir.path(),
                &NullReporter,
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
        let config = Config::new(PathBuf::from("/tmp/builds"));

        // GitHub client (anonymous - no token)
        let github = GitHubClient::anonymous().unwrap();

        // Create the command
        let cmd = BuildCommand::new(&github, &registry, &config);

        // Execute should NOT fail with PrivateRepoNoToken error
        // (it will fail later because the repo doesn't exist, but that's fine)
        let output_dir = TempDir::new().unwrap();
        let result = cmd
            .execute(
                "test-public",
                Some("1.0.0"),
                "main",
                &[],
                output_dir.path(),
                &NullReporter,
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
}
