//! Build script runner.

use std::path::{Path, PathBuf};
use std::process::Output;

use which::which;

use crate::error::{Error, Result};

use super::plugins::{Builder, OptionalCommand};

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
    /// # Arguments
    /// * `builder` - The project-specific builder
    /// * `version` - Version string for the build
    /// * `variants` - Specific variants to build (empty = all variants)
    pub async fn execute_build(
        &mut self,
        builder: &dyn Builder,
        version: &str,
        variants: &[&str],
    ) -> Result<()> {
        // Validate requested variants
        builder.validate_variants(variants).map_err(Error::Build)?;

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

        // Run build commands for specified variants
        for cmd in builder.build_commands(version, variants) {
            println!("Running: {}", cmd);
            self.run(&cmd).await?;
        }

        println!("Build completed successfully.");
        Ok(())
    }

    /// Get the working directory.
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }
}
