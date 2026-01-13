//! Build script runner.

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::error::{Error, Result};

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
                "Command '{}' failed with exit code {:?}",
                command, build_output.exit_code
            )));
        }

        Ok(build_output)
    }

    /// Get the working directory.
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }
}
