//! Build script runner.

use std::process::Output;

use which::which;

use crate::error::{Error, Result};

use super::BuildContext;
use super::plugins::{BuildArtifact, Builder, ToolDependency};
use super::progress::{BuildEvent, BuildPhase, BuildStep, NullReporter, ProgressReporter};
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
pub struct BuildRunner<'r> {
    context: BuildContext,
    reporter: &'r dyn ProgressReporter,
}

impl<'r> BuildRunner<'r> {
    /// Create a new build runner with the given build context.
    ///
    /// Uses [`NullReporter`] by default — all events are discarded.
    /// Use [`with_reporter`](Self::with_reporter) to attach a consumer.
    pub fn new(context: BuildContext) -> BuildRunner<'static> {
        BuildRunner {
            context,
            reporter: &NullReporter,
        }
    }

    /// Create a new build runner with a progress reporter.
    ///
    /// The reporter receives all build events (phase changes, step progress,
    /// command output). Consumers decide how to render them.
    pub fn with_reporter(context: BuildContext, reporter: &'r dyn ProgressReporter) -> Self {
        Self { context, reporter }
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
                    tracing::info!(
                        "Required tool '{}' is not installed. Attempting to install...",
                        dep.name
                    );
                    self.reporter.report(&BuildEvent::StepStarted {
                        step: BuildStep::new(
                            format!("Installing required tool '{}'", dep.name),
                            dep.install_commands.join(" && "),
                        ),
                    });
                    for cmd in &dep.install_commands {
                        self.run(cmd).await?;
                    }
                    self.reporter.report(&BuildEvent::StepCompleted {
                        step: BuildStep::new(
                            format!("Installing required tool '{}'", dep.name),
                            dep.install_commands.join(" && "),
                        ),
                    });
                    tracing::info!("'{}' installed successfully.", dep.name);
                }
                // Required without install commands: collect for batch error
                (true, false) => {
                    missing_required.push(dep.name);
                }
                // Optional with install commands: run them, warn on failure
                (false, true) => {
                    tracing::info!(
                        "Optional tool '{}' is not installed. Attempting to install...",
                        dep.name
                    );
                    self.reporter.report(&BuildEvent::StepStarted {
                        step: BuildStep::new(
                            format!("Installing optional tool '{}'", dep.name),
                            dep.install_commands.join(" && "),
                        ),
                    });
                    let mut ok = true;
                    for cmd in &dep.install_commands {
                        if let Err(e) = self.run(cmd).await {
                            self.reporter.report(&BuildEvent::Warning(format!(
                                "Failed to install optional tool '{}': {}",
                                dep.name, e
                            )));
                            tracing::warn!("Failed to install optional tool '{}': {}", dep.name, e);
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        self.reporter.report(&BuildEvent::StepCompleted {
                            step: BuildStep::new(
                                format!("Installing optional tool '{}'", dep.name),
                                dep.install_commands.join(" && "),
                            ),
                        });
                        tracing::info!("'{}' installed successfully.", dep.name);
                    }
                }
                // Optional without install commands: warn only
                (false, false) => {
                    self.reporter.report(&BuildEvent::Warning(format!(
                        "Optional tool '{}' is not installed. Some features may not work.",
                        dep.name
                    )));
                    tracing::warn!(
                        "Optional tool '{}' is not installed. Some features may not work.",
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
    ///
    /// On Unix, delegates to `sh -c` (POSIX shell).
    /// On Windows, delegates to `cmd /C` (Windows command interpreter).
    ///
    /// Source (Unix):   <https://pubs.opengroup.org/onlinepubs/9699919799/utilities/sh.html>
    /// Source (Windows): <https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/cmd>
    pub async fn run(&self, command: &str) -> Result<BuildOutput> {
        #[cfg(unix)]
        let output = crate::process::output(
            crate::process::command("sh")
                .arg("-c")
                .arg(command)
                .current_dir(self.context.repo_dir()),
        )
        .await?;

        #[cfg(windows)]
        let output = crate::process::output(
            crate::process::command("cmd")
                .args(["/C", command])
                .current_dir(self.context.repo_dir()),
        )
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
        self.reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::DependencyCheck,
            message: "Checking tool dependencies".into(),
        });
        self.ensure_tool_dependencies(&builder.tool_dependencies())
            .await?;
        self.reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::DependencyCheck,
        });

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
        if !builder.setup_commands().is_empty() {
            self.reporter.report(&BuildEvent::PhaseStarted {
                phase: BuildPhase::Setup,
                message: "Running setup commands".into(),
            });
            for step in builder.setup_commands() {
                tracing::debug!("Running setup: {}", step.command);
                self.reporter
                    .report(&BuildEvent::StepStarted { step: step.clone() });
                self.run(&step.command).await?;
                self.reporter.report(&BuildEvent::StepCompleted { step });
            }
            self.reporter.report(&BuildEvent::PhaseCompleted {
                phase: BuildPhase::Setup,
            });
        }

        // Pre-build hook
        self.reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::PreBuild,
            message: "Running pre-build hook".into(),
        });
        builder.pre_build_hook(&self.context, version, variants, self.reporter)?;
        self.reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::PreBuild,
        });

        // Run build commands for specified variants
        self.reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Build,
            message: "Running build commands".into(),
        });
        for step in builder.build_commands(&self.context, version, variants) {
            tracing::debug!("Running build: {}", step.command);
            self.reporter
                .report(&BuildEvent::StepStarted { step: step.clone() });
            self.run(&step.command).await?;
            self.reporter.report(&BuildEvent::StepCompleted { step });
        }
        self.reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Build,
        });

        // Build hook
        self.reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::BuildHook,
            message: "Running build hook".into(),
        });
        builder.build_hook(&self.context, version, variants, self.reporter)?;
        self.reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::BuildHook,
        });

        // Post-build hook
        self.reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::PostBuild,
            message: "Running post-build hook".into(),
        });
        builder.post_build_hook(&self.context, version, variants, self.reporter)?;
        self.reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::PostBuild,
        });

        // Collect artifacts - builder resolves paths internally
        self.reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::CollectArtifacts,
            message: "Collecting build artifacts".into(),
        });
        let build_artifacts = builder.artifacts(&self.context, version, variants)?;
        let artifacts = self.collect_artifacts(&build_artifacts)?;
        self.reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::CollectArtifacts,
        });

        // Determine which variants were built
        let variants_built = if builder.has_variants() {
            if variants.is_empty() {
                builder
                    .variants()
                    .iter()
                    .map(|v| v.id.to_string())
                    .collect()
            } else {
                variants.iter().map(|v| v.to_string()).collect()
            }
        } else {
            vec![]
        };

        tracing::info!(
            "Build completed successfully. {} artifacts produced.",
            artifacts.len()
        );
        self.reporter.report(&BuildEvent::BuildSucceeded {
            artifacts: artifacts.iter().map(|a| a.path.clone()).collect(),
        });

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
    fn collect_artifacts(
        &self,
        build_artifacts: &[BuildArtifact],
    ) -> Result<Vec<ProducedArtifact>> {
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

// The runner shells out through `sh -c`, so these tests are Unix-gated (the
// same assumption the git and end-to-end cache tests make).
#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;
    use crate::build::plugins::BuildVariant;

    /// A tool name that is never on `PATH`, to drive the "missing tool" paths.
    const MISSING_TOOL: &str = "apvm-test-tool-that-does-not-exist";
    /// A second never-installed tool, for the batched-error case.
    const OTHER_MISSING_TOOL: &str = "apvm-test-other-missing-tool";

    /// Reporter that keeps every event, so tests can assert on the timeline.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<BuildEvent>>);

    impl ProgressReporter for Recorder {
        fn report(&self, event: &BuildEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    impl Recorder {
        /// Snapshot of all events received so far.
        fn events(&self) -> Vec<BuildEvent> {
            self.0.lock().unwrap().clone()
        }

        /// The phases that were started, in order.
        fn phases_started(&self) -> Vec<BuildPhase> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    BuildEvent::PhaseStarted { phase, .. } => Some(phase),
                    _ => None,
                })
                .collect()
        }

        /// Messages of every `Warning` event, in order.
        fn warnings(&self) -> Vec<String> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    BuildEvent::Warning(message) => Some(message),
                    _ => None,
                })
                .collect()
        }

        /// Labels of every completed step, in order.
        fn completed_steps(&self) -> Vec<String> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    BuildEvent::StepCompleted { step } => Some(step.label),
                    _ => None,
                })
                .collect()
        }
    }

    /// Fully scriptable builder: every hook records its call (and the repo dir
    /// it saw) so tests can verify ordering and the build-subdirectory switch.
    #[derive(Default)]
    struct ScriptBuilder {
        deps: Vec<ToolDependency>,
        setup: Vec<&'static str>,
        build: Vec<&'static str>,
        artifacts: Vec<BuildArtifact>,
        variants: Vec<&'static str>,
        subdir: Option<&'static str>,
        hook_calls: Mutex<Vec<(&'static str, PathBuf)>>,
    }

    impl ScriptBuilder {
        /// Record that hook `name` ran with `context`.
        fn record_hook(&self, name: &'static str, context: &BuildContext) {
            self.hook_calls
                .lock()
                .unwrap()
                .push((name, context.repo_dir().to_path_buf()));
        }

        /// Names of the hooks that ran, in order.
        fn hooks_run(&self) -> Vec<&'static str> {
            self.hook_calls
                .lock()
                .unwrap()
                .iter()
                .map(|(name, _)| *name)
                .collect()
        }
    }

    impl Builder for ScriptBuilder {
        fn tool_dependencies(&self) -> Vec<ToolDependency> {
            self.deps.clone()
        }
        fn setup_commands(&self) -> Vec<BuildStep> {
            self.setup
                .iter()
                .map(|cmd| BuildStep::new("setup", *cmd))
                .collect()
        }
        fn pre_build_hook(
            &self,
            context: &BuildContext,
            _: &str,
            _: &[&str],
            _: &dyn ProgressReporter,
        ) -> Result<()> {
            self.record_hook("pre", context);
            Ok(())
        }
        fn build_hook(
            &self,
            context: &BuildContext,
            _: &str,
            _: &[&str],
            _: &dyn ProgressReporter,
        ) -> Result<()> {
            self.record_hook("build", context);
            Ok(())
        }
        fn post_build_hook(
            &self,
            context: &BuildContext,
            _: &str,
            _: &[&str],
            _: &dyn ProgressReporter,
        ) -> Result<()> {
            self.record_hook("post", context);
            Ok(())
        }
        fn variants(&self) -> Vec<BuildVariant> {
            self.variants
                .iter()
                .map(|id| BuildVariant {
                    id,
                    name: id,
                    description: id,
                })
                .collect()
        }
        fn build_commands(&self, _: &BuildContext, _: &str, _: &[&str]) -> Vec<BuildStep> {
            self.build
                .iter()
                .map(|cmd| BuildStep::new("build", *cmd))
                .collect()
        }
        fn artifacts(&self, _: &BuildContext, _: &str, _: &[&str]) -> Result<Vec<BuildArtifact>> {
            Ok(self.artifacts.clone())
        }
        fn build_subdirectory(&self) -> Option<&'static str> {
            self.subdir
        }
    }

    /// A single-output artifact at `source_path`, delivered as `plugin.zip`.
    fn artifact(source_path: impl Into<String>) -> BuildArtifact {
        BuildArtifact {
            variant_id: None,
            source_path: source_path.into(),
            target_name: "plugin.zip".to_string(),
        }
    }

    /// A temp workspace containing a `repo/` directory, plus its context.
    fn workspace() -> (TempDir, BuildContext) {
        let workspace = TempDir::new().unwrap();
        let repo = workspace.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let context = BuildContext::new(repo, workspace.path().to_path_buf());
        (workspace, context)
    }

    /// Unwrap a [`Error::Build`] message, failing on any other outcome.
    fn build_error<T: std::fmt::Debug>(result: Result<T>) -> String {
        match result {
            Err(Error::Build(message)) => message,
            other => panic!("expected Error::Build, got {other:?}"),
        }
    }

    /// Resolve symlinks (macOS `/var` → `/private/var`) for path equality.
    fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap()
    }

    // ---- run -----------------------------------------------------------------

    #[tokio::test]
    async fn run_captures_stdout_and_executes_in_the_repo_dir() {
        let (_ws, context) = workspace();
        let runner = BuildRunner::new(context.clone());

        let output = runner.run("echo hello; pwd -P").await.unwrap();

        assert!(output.success);
        assert_eq!(output.exit_code, Some(0));
        let mut lines = output.stdout.lines();
        assert_eq!(lines.next(), Some("hello"));
        assert_eq!(
            lines.next().map(PathBuf::from),
            Some(canonical(context.repo_dir()))
        );
    }

    #[tokio::test]
    async fn run_failure_reports_command_exit_code_and_stderr() {
        let (_ws, context) = workspace();
        let runner = BuildRunner::new(context);

        let message = build_error(runner.run("echo boom >&2; exit 3").await);

        // All three are what a user needs to diagnose a failed build step.
        assert!(message.contains("echo boom >&2; exit 3"), "{message}");
        assert!(message.contains("exit code Some(3)"), "{message}");
        assert!(message.contains("boom"), "{message}");
    }

    #[test]
    fn command_exists_detects_present_and_missing_tools() {
        assert!(BuildRunner::command_exists("sh"));
        assert!(!BuildRunner::command_exists(MISSING_TOOL));
    }

    // ---- ensure_tool_dependencies -------------------------------------------

    #[tokio::test]
    async fn present_tools_never_run_their_install_commands() {
        let (_ws, context) = workspace();
        let recorder = Recorder::default();
        let runner = BuildRunner::with_reporter(context.clone(), &recorder);
        let deps = [ToolDependency::required_with_install(
            "sh",
            vec!["touch installed"],
        )];

        runner.ensure_tool_dependencies(&deps).await.unwrap();

        assert!(!context.repo_dir().join("installed").exists());
        assert!(recorder.events().is_empty(), "{:?}", recorder.events());
    }

    #[tokio::test]
    async fn missing_required_tools_fail_together_in_one_error() {
        let (_ws, context) = workspace();
        let runner = BuildRunner::new(context);
        let deps = [
            ToolDependency::required(MISSING_TOOL),
            ToolDependency::required("sh"),
            ToolDependency::required(OTHER_MISSING_TOOL),
        ];

        let message = build_error(runner.ensure_tool_dependencies(&deps).await);

        // Batched: the user learns about every missing tool at once.
        assert!(
            message.contains(&format!(
                "Missing required commands: {MISSING_TOOL}, {OTHER_MISSING_TOOL}."
            )),
            "{message}"
        );
    }

    #[tokio::test]
    async fn missing_required_tool_runs_install_commands_in_order() {
        let (_ws, context) = workspace();
        let recorder = Recorder::default();
        let runner = BuildRunner::with_reporter(context.clone(), &recorder);
        let deps = [ToolDependency::required_with_install(
            MISSING_TOOL,
            vec!["echo one >> log", "echo two >> log"],
        )];

        runner.ensure_tool_dependencies(&deps).await.unwrap();

        let log = std::fs::read_to_string(context.repo_dir().join("log")).unwrap();
        assert_eq!(log, "one\ntwo\n");
        assert_eq!(
            recorder.completed_steps(),
            [format!("Installing required tool '{MISSING_TOOL}'")]
        );
    }

    #[tokio::test]
    async fn failed_required_install_aborts_without_running_later_commands() {
        let (_ws, context) = workspace();
        let runner = BuildRunner::new(context.clone());
        let deps = [ToolDependency::required_with_install(
            MISSING_TOOL,
            vec!["exit 7", "touch after"],
        )];

        let message = build_error(runner.ensure_tool_dependencies(&deps).await);

        assert!(message.contains("exit code Some(7)"), "{message}");
        assert!(!context.repo_dir().join("after").exists());
    }

    #[tokio::test]
    async fn failed_optional_install_only_warns_and_stops_installing() {
        let (_ws, context) = workspace();
        let recorder = Recorder::default();
        let runner = BuildRunner::with_reporter(context.clone(), &recorder);
        let deps = [ToolDependency::optional_with_install(
            MISSING_TOOL,
            vec!["exit 1", "touch after"],
        )];

        runner.ensure_tool_dependencies(&deps).await.unwrap();

        let warnings = recorder.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with(&format!("Failed to install optional tool '{MISSING_TOOL}'"))
        );
        assert!(!context.repo_dir().join("after").exists());
        assert!(
            recorder.completed_steps().is_empty(),
            "a failed install must not be reported as completed"
        );
    }

    #[tokio::test]
    async fn successful_optional_install_completes_without_warning() {
        let (_ws, context) = workspace();
        let recorder = Recorder::default();
        let runner = BuildRunner::with_reporter(context.clone(), &recorder);
        let deps = [ToolDependency::optional_with_install(
            MISSING_TOOL,
            vec!["touch installed"],
        )];

        runner.ensure_tool_dependencies(&deps).await.unwrap();

        assert!(context.repo_dir().join("installed").exists());
        assert!(recorder.warnings().is_empty());
        assert_eq!(
            recorder.completed_steps(),
            [format!("Installing optional tool '{MISSING_TOOL}'")]
        );
    }

    #[tokio::test]
    async fn missing_optional_tool_without_install_only_warns() {
        let (_ws, context) = workspace();
        let recorder = Recorder::default();
        let runner = BuildRunner::with_reporter(context, &recorder);
        let deps = [ToolDependency::optional(MISSING_TOOL)];

        runner.ensure_tool_dependencies(&deps).await.unwrap();

        assert_eq!(
            recorder.warnings(),
            [format!(
                "Optional tool '{MISSING_TOOL}' is not installed. Some features may not work."
            )]
        );
    }

    // ---- execute_build ------------------------------------------------------

    #[tokio::test]
    async fn execute_build_runs_every_phase_in_order_and_collects_artifacts() {
        let (_ws, context) = workspace();
        let recorder = Recorder::default();
        let builder = ScriptBuilder {
            setup: vec!["echo setup > setup.txt"],
            build: vec!["test -f setup.txt && printf 'zipbytes' > out.zip"],
            artifacts: vec![artifact("out.zip")],
            ..ScriptBuilder::default()
        };
        let mut runner = BuildRunner::with_reporter(context.clone(), &recorder);

        let result = runner.execute_build(&builder, "1.2.3", &[]).await.unwrap();

        assert_eq!(
            recorder.phases_started(),
            [
                BuildPhase::DependencyCheck,
                BuildPhase::Setup,
                BuildPhase::PreBuild,
                BuildPhase::Build,
                BuildPhase::BuildHook,
                BuildPhase::PostBuild,
                BuildPhase::CollectArtifacts,
            ]
        );
        assert_eq!(builder.hooks_run(), ["pre", "build", "post"]);

        let expected = context.repo_dir().join("out.zip");
        assert_eq!(result.version, "1.2.3");
        assert_eq!(result.build_dir, context.repo_dir());
        assert!(result.variants_built.is_empty(), "single-output build");
        assert_eq!(result.artifacts.len(), 1);
        let produced = &result.artifacts[0];
        assert_eq!(produced.path, expected);
        assert_eq!(produced.filename, "plugin.zip");
        assert_eq!(produced.size, "zipbytes".len() as u64);
        assert!(matches!(
            recorder.events().last(),
            Some(BuildEvent::BuildSucceeded { artifacts }) if *artifacts == [expected.clone()]
        ));
    }

    #[tokio::test]
    async fn execute_build_skips_the_setup_phase_without_setup_commands() {
        let (_ws, context) = workspace();
        let recorder = Recorder::default();
        let builder = ScriptBuilder::default();
        let mut runner = BuildRunner::with_reporter(context, &recorder);

        runner.execute_build(&builder, "1.0.0", &[]).await.unwrap();

        assert!(!recorder.phases_started().contains(&BuildPhase::Setup));
    }

    #[tokio::test]
    async fn execute_build_rejects_an_unknown_variant_before_doing_anything() {
        let (_ws, context) = workspace();
        let recorder = Recorder::default();
        let builder = ScriptBuilder {
            variants: vec!["free", "pro"],
            setup: vec!["touch ran"],
            ..ScriptBuilder::default()
        };
        let mut runner = BuildRunner::with_reporter(context.clone(), &recorder);

        let message = build_error(runner.execute_build(&builder, "1.0.0", &["gold"]).await);

        assert_eq!(message, "Unknown variant 'gold'. Available: free, pro");
        assert!(recorder.events().is_empty(), "nothing may start");
        assert!(!context.repo_dir().join("ran").exists());
    }

    #[tokio::test]
    async fn execute_build_switches_into_the_build_subdirectory() {
        let (_ws, context) = workspace();
        let subdir = context.repo_dir().join("plugin");
        std::fs::create_dir(&subdir).unwrap();
        let builder = ScriptBuilder {
            subdir: Some("plugin"),
            setup: vec!["pwd -P > setup-dir"],
            build: vec!["printf x > out.zip"],
            artifacts: vec![artifact("out.zip")],
            ..ScriptBuilder::default()
        };
        let mut runner = BuildRunner::new(context.clone());

        let result = runner.execute_build(&builder, "1.0.0", &[]).await.unwrap();

        // Commands, hooks, artifact resolution and the result all use the
        // subdirectory — not the repo root.
        let setup_dir = std::fs::read_to_string(subdir.join("setup-dir")).unwrap();
        assert_eq!(PathBuf::from(setup_dir.trim()), canonical(&subdir));
        assert!(
            builder
                .hook_calls
                .lock()
                .unwrap()
                .iter()
                .all(|(_, dir)| *dir == subdir)
        );
        assert_eq!(result.artifacts[0].path, subdir.join("out.zip"));
        assert_eq!(result.build_dir, subdir);
        assert_eq!(runner.context().repo_dir(), subdir);
    }

    #[tokio::test]
    async fn execute_build_fails_when_the_build_subdirectory_is_missing() {
        let (_ws, context) = workspace();
        let builder = ScriptBuilder {
            subdir: Some("missing"),
            setup: vec!["touch ran"],
            ..ScriptBuilder::default()
        };
        let mut runner = BuildRunner::new(context.clone());

        let message = build_error(runner.execute_build(&builder, "1.0.0", &[]).await);

        assert!(message.starts_with("Build directory '"), "{message}");
        assert!(message.contains("missing"), "{message}");
        assert!(!context.repo_dir().join("ran").exists());
    }

    #[tokio::test]
    async fn a_failing_build_command_stops_the_pipeline() {
        let (_ws, context) = workspace();
        let recorder = Recorder::default();
        let builder = ScriptBuilder {
            build: vec!["exit 2", "touch after"],
            artifacts: vec![artifact("out.zip")],
            ..ScriptBuilder::default()
        };
        let mut runner = BuildRunner::with_reporter(context.clone(), &recorder);

        let message = build_error(runner.execute_build(&builder, "1.0.0", &[]).await);

        assert!(message.contains("exit code Some(2)"), "{message}");
        assert!(!context.repo_dir().join("after").exists());
        // Only the pre-build hook ran; nothing after the failed step did.
        assert_eq!(builder.hooks_run(), ["pre"]);
        assert!(
            !recorder
                .phases_started()
                .contains(&BuildPhase::CollectArtifacts)
        );
        assert!(
            !recorder
                .events()
                .iter()
                .any(|e| matches!(e, BuildEvent::BuildSucceeded { .. }))
        );
    }

    #[tokio::test]
    async fn a_missing_artifact_fails_the_build_with_its_path() {
        let (_ws, context) = workspace();
        let builder = ScriptBuilder {
            artifacts: vec![artifact("never-built.zip")],
            ..ScriptBuilder::default()
        };
        let mut runner = BuildRunner::new(context.clone());

        let message = build_error(runner.execute_build(&builder, "1.0.0", &[]).await);

        let expected = context.repo_dir().join("never-built.zip");
        assert!(
            message.starts_with(&format!(
                "Expected artifact not found: '{}'",
                expected.display()
            )),
            "{message}"
        );
    }

    #[tokio::test]
    async fn an_absolute_artifact_path_is_used_as_is() {
        // Builders such as WP Rocket package into the workspace, outside the
        // repo; an absolute path must not be re-rooted under `repo_dir`.
        let (ws, context) = workspace();
        let outside = ws.path().join("staged.zip");
        std::fs::write(&outside, b"abc").unwrap();
        let builder = ScriptBuilder {
            artifacts: vec![artifact(outside.to_string_lossy())],
            ..ScriptBuilder::default()
        };
        let mut runner = BuildRunner::new(context);

        let result = runner.execute_build(&builder, "1.0.0", &[]).await.unwrap();

        assert_eq!(result.artifacts[0].path, outside);
        assert_eq!(result.artifacts[0].size, 3);
    }

    #[tokio::test]
    async fn variants_built_lists_every_variant_when_none_requested() {
        let (_ws, context) = workspace();
        let builder = ScriptBuilder {
            variants: vec!["free", "pro"],
            ..ScriptBuilder::default()
        };
        let mut runner = BuildRunner::new(context);

        let all = runner.execute_build(&builder, "1.0.0", &[]).await.unwrap();
        let some = runner
            .execute_build(&builder, "1.0.0", &["pro"])
            .await
            .unwrap();

        assert_eq!(all.variants_built, ["free", "pro"]);
        assert_eq!(some.variants_built, ["pro"]);
    }

    #[tokio::test]
    async fn a_lone_variant_counts_as_single_output() {
        // `has_variants` needs more than one variant; a single one is treated
        // as a variant-less build (empty `variants_built`).
        let (_ws, context) = workspace();
        let builder = ScriptBuilder {
            variants: vec!["only"],
            ..ScriptBuilder::default()
        };
        let mut runner = BuildRunner::new(context);

        let result = runner.execute_build(&builder, "1.0.0", &[]).await.unwrap();

        assert!(result.variants_built.is_empty());
    }

    #[tokio::test]
    async fn dropping_a_running_step_kills_its_process() {
        // A cancelled build (e.g. its Node.js worker exited) must not leave
        // the current build step running.
        use crate::process::testutil::{running, sleeper_script, stops_soon, wait_for_pid};
        let (ws, context) = workspace();
        let pid_file = ws.path().join("pid");
        let runner = BuildRunner::new(context);
        let script = sleeper_script(&pid_file);

        let pending = tokio::spawn(async move { runner.run(&script).await });
        let pid = tokio::task::spawn_blocking(move || wait_for_pid(&pid_file))
            .await
            .unwrap();
        assert!(running(&pid), "precondition: the step is running");

        pending.abort();
        let _ = pending.await;

        assert!(
            stops_soon(&pid),
            "the step {pid} outlived its dropped build"
        );
    }
}
