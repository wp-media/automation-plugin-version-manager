//! Build script runner.


use std::process::Output;

use which::which;

use crate::error::{Error, Result};

use super::BuildContext;
use super::plugins::{BuildArtifact, Builder, ToolDependency};
use super::result::{BuildResult, ProducedArtifact};

/// Output from a build run.
#[derive(Debug)]
pub struct BuildOutput {
    /// Whether the build succeeded.
    pub success: bool,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Exit code if available.
    pub exit_code: Option<i32>,
}

impl From<Output> for BuildOutput {
    fn from(output: Output) -> Self {
        Self {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
        }
    }
}

/// Runs build scripts.
pub struct BuildRunner {
    context: BuildContext,
}

impl BuildRunner {
    /// Create a new build runner with the given build context.
    pub fn new(context: BuildContext) -> Self {
        Self { context }
    }

    /// Check if a command is available in PATH.
    pub fn command_exists(cmd: &str) -> bool {
        which(cmd).is_ok()
    }

    /// Verify all tool dependencies are available, installing those that have
    /// an install command when missing.
    ///
    /// Each [`ToolDependency`] is processed according to its `required` and
    /// `install_commands` fields:
    ///
    /// | `required` | `install_commands` | Missing behavior                               |
    /// |------------|--------------------|-------------------------------------------------|
    /// | `true`     | non-empty          | Run install commands, fail if any command fails  |
    /// | `true`     | empty              | Fail immediately                                 |
    /// | `false`    | non-empty          | Run install commands, warn if any command fails  |
    /// | `false`    | empty              | Warn and continue                                |
    pub async fn ensure_tool_dependencies(&self, deps: &[ToolDependency]) -> Result<()> {
        let mut missing_required: Vec<&str> = Vec::new();

        for dep in deps {
            if Self::command_exists(dep.name) {
                continue;
            }

            let has_install = !dep.install_commands.is_empty();

            match (dep.required, has_install) {
                // Required with install commands: run them in order, fail if any fails
                (true, true) => {
                    println!(
                        "Info: required tool '{}' is not installed. Attempting to install...",
                        dep.name
                    );
                    for cmd in &dep.install_commands {
                        self.run(cmd).await?;
                    }
                    println!("Info: '{}' installed successfully.", dep.name);
                }
                // Required without install commands: collect for batch error
                (true, false) => {
                    missing_required.push(dep.name);
                }
                // Optional with install commands: run them, warn on failure
                (false, true) => {
                    println!(
                        "Info: optional tool '{}' is not installed. Attempting to install...",
                        dep.name
                    );
                    let mut ok = true;
                    for cmd in &dep.install_commands {
                        if let Err(e) = self.run(cmd).await {
                            println!(
                                "Warning: failed to install optional tool '{}': {}",
                                dep.name, e
                            );
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        println!("Info: '{}' installed successfully.", dep.name);
                    }
                }
                // Optional without install commands: warn only
                (false, false) => {
                    println!(
                        "Warning: optional tool '{}' is not installed. Some features may not work.",
                        dep.name
                    );
                }
            }
        }

        if !missing_required.is_empty() {
            return Err(Error::Build(format!(
                "Missing required commands: {}. Please install them and ensure they are in PATH.",
                missing_required.join(", ")
            )));
        }

        Ok(())
    }

    /// Run a shell command with repo dir as working/current directory.
    pub async fn run(&self, command: &str) -> Result<BuildOutput> {
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(self.context.repo_dir())
            .output()
            .await?;

        let build_output = BuildOutput::from(output);

        if !build_output.success {
            return Err(Error::Build(format!(
                "Command '{}' failed with exit code {:?}: {}",
                command, build_output.exit_code, build_output.stderr
            )));
        }

        Ok(build_output)
    }

    /// Run a full build using a Builder.
    ///
    /// This method orchestrates the complete build process for a project by executing
    /// a well-defined sequence of steps. Each step must complete successfully before
    /// proceeding to the next.
    ///
    /// # Build Sequence
    ///
    /// ```text
    /// ┌─────────────────────────────────────────────────────────────┐
    /// │                    BUILD PROCESS FLOW                       │
    /// └─────────────────────────────────────────────────────────────┘
    ///
    /// 1. VALIDATE VARIANTS
    ///    │  └─► Ensures requested variants are supported by the builder
    ///    ▼
    /// 2. ENSURE TOOL DEPENDENCIES
    ///    │  └─► Checks all tools, auto-installs those with install commands
    ///    ▼
    /// 3. CHANGE TO BUILD SUBDIRECTORY (if specified)
    ///    │  └─► Switches working directory to project-specific build folder
    ///    ▼
    /// 4. RUN SETUP COMMANDS
    ///    │  └─► Executes dependency installation (npm install, composer install)
    ///    ▼
    /// 5. PRE-BUILD HOOK
    ///    │  └─► Builder-specific preparation (modify configs, set versions)
    ///    ▼
    /// 6. RUN BUILD COMMANDS
    ///    │  └─► Executes variant-specific build scripts (compile, bundle)
    ///    ▼
    /// 7. BUILD HOOK
    ///    │  └─► Builder-specific mid-build processing
    ///    ▼
    /// 8. POST-BUILD HOOK
    ///    │  └─► Cleanup, artifact packaging, file organization
    ///    ▼
    /// ✓ BUILD COMPLETE
    /// ```
    ///
    /// # Arguments
    ///
    /// * `builder` - A trait object implementing [`Builder`] that provides project-specific
    ///   build configuration (commands, hooks, variants)
    /// * `version` - The semantic version string for this build (e.g., `"3.17.4"`)
    /// * `variants` - Slice of variant names to build. Pass an empty slice to build all
    ///   variants defined by the builder
    ///
    /// # Returns
    ///
    /// * `Ok(BuildResult)` - All build steps completed successfully with artifact information
    /// * `Err(Error::Build)` - A build step failed with details about the failure
    /// * `Err(Error::Io)` - File system or process execution error
    ///
    /// # Errors
    ///
    /// This function will return an error if:
    /// - An invalid variant is requested (not supported by the builder)
    /// - A required command is missing from PATH
    /// - The build subdirectory doesn't exist
    /// - Any setup, build, or hook command fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use std::path::PathBuf;
    /// use crate::build::BuildContext;
    /// use crate::build::runner::BuildRunner;
    /// use crate::build::plugins::WpRocketBuilder;
    ///
    /// async fn build_plugin() -> Result<(), Error> {
    ///     let context = BuildContext::new(
    ///         PathBuf::from("/tmp/wp-rocket-abc123/wp-rocket"),
    ///         PathBuf::from("/tmp/wp-rocket-abc123"),
    ///     );
    ///     let mut runner = BuildRunner::new(context);
    ///     let builder = WpRocketBuilder;
    ///
    ///     let result = runner.execute_build(&builder, "3.17.4", &[]).await?;
    ///     for artifact in &result.artifacts {
    ///         println!("  - {} ({} bytes)", artifact.filename, artifact.size);
    ///     }
    ///
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Note
    ///
    /// This method mutates `self` because it may change the repo directory
    /// in the build context if the builder specifies a build subdirectory.
    pub async fn execute_build(
        &mut self,
        builder: &dyn Builder,
        version: &str,
        variants: &[&str],
    ) -> Result<BuildResult> {
        // Validate requested variants
        builder.validate_variants(variants)?;

        // Ensure all tool dependencies are met
        self.ensure_tool_dependencies(&builder.tool_dependencies()).await?;

        // Change to build subdirectory if specified
        if let Some(subdir) = builder.build_subdirectory() {
            let new_dir = self.context.repo_dir().join(subdir);
            if !new_dir.exists() {
                return Err(Error::Build(format!(
                    "Build directory '{}' does not exist. Try cloning the repository first.",
                    new_dir.display()
                )));
            }
            self.context.set_repo_dir(new_dir);
        }

        // Run setup commands
        for cmd in builder.setup_commands() {
            println!("Running: {}", cmd);
            self.run(&cmd).await?;
        }
        // Pre-build hook
        builder.pre_build_hook(&self.context, version, variants)?;

        // Run build commands for specified variants
        for cmd in builder.build_commands(&self.context, version, variants) {
            println!("Running: {}", cmd);
            self.run(&cmd).await?;
        }
        // Build hook
        builder.build_hook(&self.context, version, variants)?;
        // Post-build hook
        builder.post_build_hook(&self.context, version, variants)?;

        // Collect artifacts - builder resolves paths internally
        let build_artifacts = builder.artifacts(&self.context, version, variants)?;
        let artifacts = self.collect_artifacts(&build_artifacts)?;

        // Determine which variants were built
        let variants_built = if builder.has_variants() {
            if variants.is_empty() {
                builder.variants().iter().map(|v| v.id.to_string()).collect()
            } else {
                variants.iter().map(|v| v.to_string()).collect()
            }
        } else {
            vec![]
        };

        println!("Build completed successfully. {} artifacts produced.", artifacts.len());

        Ok(BuildResult::new(
            artifacts,
            self.context.repo_dir().to_path_buf(),
            version.to_string(),
            variants_built,
        ))
    }

    /// Collect and verify artifacts after build completes.
    ///
    /// Takes artifact definitions from the builder, resolves their paths,
    /// verifies the files exist, and returns sized `ProducedArtifact` entries.
    ///
    /// # Path Resolution
    ///
    /// - **Absolute** `source_path` — used as-is (for artifacts placed outside
    ///   the repo directory, e.g., in `workspace_dir`)
    /// - **Relative** `source_path` — resolved against `repo_dir`
    ///
    /// # Arguments
    ///
    /// * `build_artifacts` - Artifact definitions from the builder
    ///
    /// # Returns
    ///
    /// * `Ok(Vec<ProducedArtifact>)` - All artifacts collected successfully
    /// * `Err(Error::Build)` - An expected artifact file was not found
    ///
    /// # Examples
    ///
    /// ```text
    /// Relative path:      BuildArtifact { source_path: "dist/plugin.zip", ... }
    ///                             ↓
    /// Resolved:            /tmp/build-abc/my-plugin/dist/plugin.zip
    ///
    /// Absolute path:      BuildArtifact { source_path: "/tmp/build-abc/plugin.zip", ... }
    ///                             ↓
    /// Used as-is:          /tmp/build-abc/plugin.zip
    /// ```
    fn collect_artifacts(&self, build_artifacts: &[BuildArtifact]) -> Result<Vec<ProducedArtifact>> {
        let mut artifacts = Vec::with_capacity(build_artifacts.len());

        for artifact in build_artifacts {
            let source = std::path::Path::new(&artifact.source_path);
            let path = if source.is_absolute() {
                source.to_path_buf()
            } else {
                self.context.repo_dir().join(source)
            };

            if !path.exists() {
                return Err(Error::Build(format!(
                    "Expected artifact not found: '{}'. \
                    The build may have failed silently or the artifact path is incorrect.",
                    path.display()
                )));
            }

            let metadata = std::fs::metadata(&path)?;
            let size = metadata.len();

            artifacts.push(ProducedArtifact::new(
                artifact.variant_id.clone(),
                path,
                artifact.target_name.clone(),
                size,
            ));
        }

        Ok(artifacts)
    }

    /// Get the build context.
    pub fn context(&self) -> &BuildContext {
        &self.context
    }
}
