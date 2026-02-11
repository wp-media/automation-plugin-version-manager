//! Build command implementation.
//!
//! This module provides the main build command that handles building a project
//! from any git reference (PR, branch, tag, or commit) with automatic detection.

use crate::build::plugins::VersionRequirement;
use crate::build::progress::{BuildEvent, BuildPhase, ProgressReporter};
use crate::build::{BuildResult, BuildRunner};
use crate::error::{Error, Result};
use crate::git::{BuildWorkspace, RefResolver, RefSource, ResolvedRef};
use crate::github::GitHubClient;
use crate::projects::ProjectRegistry;
use apvm_config::Config;
use std::path::Path;

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
    pub fn description(&self) -> String {
        format!(
            "{} @ {}",
            self.resolved_ref.source.description(),
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

        // 3. Create isolated build workspace (auto-cleaned on drop)
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

        // 4. Resolve the git reference
        let resolver = RefResolver::new(self.github, &project_info.owner, &project_info.repo)
            .with_repo_path(repo.path());

        let resolved = resolver.resolve(git_ref).await?;

        // 5. Prepare repository for checkout (clean state)
        // Reset FIRST to avoid checkout failures due to uncommitted changes.
        // See: https://git-scm.com/docs/git-checkout#_description
        // "git checkout refuses to switch branches if there are local modifications"
        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Checkout,
            message: format!("Checking out {}", resolved.source.description()),
        });
        tracing::debug!(
            "Resetting repository and switching to default branch '{}'",
            project_info.default_branch
        );
        repo.reset_hard().await?;
        repo.checkout(&project_info.default_branch).await?;
        repo.reset_hard().await?;
        workspace.pull().await?;

        // 6. Checkout the resolved ref
        repo.checkout(&resolved.git_ref).await?;

        // 7. Get the commit SHA after checkout
        let (commit, commit_short) = repo.get_head_commit_pair().await?;
        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Checkout,
        });

        // 8. Resolve version (AFTER checkout so detect_version sees correct files)
        let resolved_version = self.resolve_version(
            project,
            version,
            builder,
            repo.path(),
        )?;

        tracing::info!(
            "Building {} v{} from {}",
            project,
            resolved_version,
            resolved.source.description()
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
        };

        tracing::debug!("Checked out {} at commit {}", resolved.git_ref, commit_short);

        // 9. Run the build using the project's builder
        let build_context = workspace.to_build_context();
        let mut runner = BuildRunner::with_reporter(build_context, reporter);
        let result = runner
            .execute_build(builder, &resolved_version, variants)
            .await?;

        // 10. Collect artifacts to output_dir BEFORE workspace cleanup
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
                Err(Error::Build(format!(
                    "Project '{}' requires a version. Use: build(\"{}\", Some(\"X.Y.Z\"), ...)",
                    project, project
                )))
            }
            (VersionRequirement::Required, Some(v)) => {
                tracing::debug!("Using required version: {}", v);
                Ok(v.to_string())
            }

            (VersionRequirement::Embedded, Some(v)) => {
                tracing::warn!(
                    "Project '{}' has embedded version; ignoring provided '{}' and detecting from source",
                    project, v
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
            Ok(None) => {
                Err(Error::Build(format!(
                    "Could not auto-detect version for '{}'.\n\n\
                     The builder does not implement version detection, or the \
                     version was not found in the expected location.\n\n\
                     Please provide a version explicitly: build(\"{}\", Some(\"X.Y.Z\"), ...)",
                    project, project
                )))
            }
            Err(e) => {
                Err(Error::Build(format!(
                    "Version detection failed for '{}': {}\n\n\
                     Please provide a version explicitly: build(\"{}\", Some(\"X.Y.Z\"), ...)",
                    project, e, project
                )))
            }
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
        self.execute(project, version, &format!("pr:{pr_number}"), variants, output_dir, reporter)
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
        self.execute(project, version, &format!("branch:{branch}"), variants, output_dir, reporter)
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
        self.execute(project, version, &format!("tag:{tag}"), variants, output_dir, reporter)
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
        self.execute(project, version, &format!("commit:{commit}"), variants, output_dir, reporter)
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

        fn build_commands(&self, _context: &BuildContext, _version: &str, _variants: &[&str]) -> Vec<BuildStep> {
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
        let result = cmd.execute("test-private", Some("1.0.0"), "main", &[], output_dir.path(), &NullReporter).await;

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
        let result = cmd.execute("test-public", Some("1.0.0"), "main", &[], output_dir.path(), &NullReporter).await;

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
}
