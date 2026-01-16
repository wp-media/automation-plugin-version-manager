//! Build script runner.

use std::path::{Path, PathBuf};
use std::process::Output;

use which::which;

use crate::error::{Error, Result};

use super::plugins::{BuildArtifact, Builder, OptionalCommand};
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
    working_dir: PathBuf,
}

impl BuildRunner {
    /// Create a new build runner with the given working directory.
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }

    /// Check if a command is available in PATH.
    pub fn command_exists(cmd: &str) -> bool {
        which(cmd).is_ok()
    }

    /// Verify all required commands are available.
    pub fn check_required_commands(commands: &[&str]) -> Result<()> {
        let missing: Vec<&str> = commands
            .iter()
            .filter(|cmd| !Self::command_exists(cmd))
            .copied()
            .collect();

        if !missing.is_empty() {
            return Err(Error::Build(format!(
                "Missing required commands: {}. Please install them and ensure they are in PATH.",
                missing.join(", ")
            )));
        }

        Ok(())
    }

    /// Install optional commands if missing.
    pub async fn ensure_optional_commands(&self, commands: &[OptionalCommand]) -> Result<()> {
        for cmd in commands {
            if !Self::command_exists(cmd.name) {
                println!("Info: '{}' is not installed. Attempting to install...", cmd.name);
                self.run(cmd.install_cmd).await?;
                println!("Info: '{}' installed successfully.", cmd.name);
            }
        }
        Ok(())
    }

    /// Run a shell command.
    pub async fn run(&self, command: &str) -> Result<BuildOutput> {
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.working_dir)
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
    /// 2. CHECK REQUIRED COMMANDS
    ///    │  └─► Verifies all required CLI tools are available in PATH
    ///    ▼
    /// 3. ENSURE OPTIONAL COMMANDS
    ///    │  └─► Installs optional tools if missing (e.g., composer plugins)
    ///    ▼
    /// 4. CHANGE TO BUILD SUBDIRECTORY (if specified)
    ///    │  └─► Switches working directory to project-specific build folder
    ///    ▼
    /// 5. RUN SETUP COMMANDS
    ///    │  └─► Executes dependency installation (npm install, composer install)
    ///    ▼
    /// 6. PRE-BUILD HOOK
    ///    │  └─► Builder-specific preparation (modify configs, set versions)
    ///    ▼
    /// 7. RUN BUILD COMMANDS
    ///    │  └─► Executes variant-specific build scripts (compile, bundle)
    ///    ▼
    /// 8. BUILD HOOK
    ///    │  └─► Builder-specific mid-build processing
    ///    ▼
    /// 9. POST-BUILD HOOK
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
    /// use crate::build::runner::BuildRunner;
    /// use crate::build::plugins::wp_rocket::WpRocketBuilder;
    ///
    /// async fn build_plugin() -> Result<(), Error> {
    ///     let mut runner = BuildRunner::new(PathBuf::from("/path/to/repo"));
    ///     let builder = WpRocketBuilder::new();
    ///     
    ///     // Build specific variants
    ///     let result = runner.execute_build(&builder, "3.17.4", &["pro", "starter"]).await?;
    ///     println!("Built {} artifacts ({} bytes)", result.artifacts.len(), result.total_size());
    ///     
    ///     // Or build all variants
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
    /// This method mutates `self` because it may change the `working_dir` if the
    /// builder specifies a build subdirectory.
    pub async fn execute_build(
        &mut self,
        builder: &dyn Builder,
        version: &str,
        variants: &[&str],
    ) -> Result<BuildResult> {
        // Validate requested variants
        builder.validate_variants(variants)?;

        // Check required commands
        Self::check_required_commands(&builder.required_commands())?;

        // Install optional commands if needed
        self.ensure_optional_commands(&builder.optional_commands()).await?;

        // Change to build subdirectory if specified
        if let Some(subdir) = builder.build_subdirectory() {
            let new_dir = self.working_dir.join(subdir);
            if !new_dir.exists() {
                return Err(Error::Build(format!(
                    "Build directory '{}' does not exist. Try cloning the repository first.",
                    new_dir.display()
                )));
            }
            self.working_dir = new_dir;
        }

        // Run setup commands
        for cmd in builder.setup_commands() {
            println!("Running: {}", cmd);
            self.run(&cmd).await?;
        }
        // Pre-build hook
        builder.pre_build_hook(&self.working_dir, version, variants)?;

        // Run build commands for specified variants
        for cmd in builder.build_commands(version, variants) {
            println!("Running: {}", cmd);
            self.run(&cmd).await?;
        }
        // Build hook
        builder.build_hook(&self.working_dir, version, variants)?;
        // Post-build hook
        builder.post_build_hook(&self.working_dir, version, variants)?;

        // Collect artifacts
        let build_artifacts = builder.artifacts(version, variants);
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
            self.working_dir.clone(),
            version.to_string(),
            variants_built,
        ))
    }

    /// Collect and verify artifacts after build completes.
    ///
    /// This method takes the artifact definitions from the builder and verifies
    /// that each expected file exists. For each verified file, it reads the size
    /// and creates a `ProducedArtifact` with the full path.
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
    /// # Example
    ///
    /// ```text
    /// Builder defines:    BuildArtifact { source_path: "dist/plugin.zip", ... }
    ///                             ↓
    /// Resolved path:      /path/to/repo/dist/plugin.zip
    ///                             ↓
    /// Verified & sized:   ProducedArtifact { path: ..., size: 1234567 }
    /// ```
    fn collect_artifacts(&self, build_artifacts: &[BuildArtifact]) -> Result<Vec<ProducedArtifact>> {
        let mut artifacts = Vec::with_capacity(build_artifacts.len());

        for artifact in build_artifacts {
            let path = self.working_dir.join(&artifact.source_path);

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

    /// Get the working directory.
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }
}
