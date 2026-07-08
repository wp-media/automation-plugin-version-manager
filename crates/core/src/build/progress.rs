//! Universal progress reporting for build operations.
//!
//! This module provides a consumer-agnostic progress system that decouples
//! build event emission from presentation. The library always emits events;
//! consumers decide what to do with them (terminal spinner, websocket push,
//! napi callback to Node.js, REST polling, etc.).
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────┐     ┌──────────────────┐     ┌──────────────────┐
//! │ BuildCommand │────►│ ProgressReporter │────►│    Consumer      │
//! │  / Runner    │     │   (trait object)  │     │ (CLI, napi, ws)  │
//! └──────────────┘     └──────────────────┘     └──────────────────┘
//!      emits               dispatches                renders
//!    BuildEvent            synchronously             however
//! ```
//!
//! # Consumer Examples
//!
//! **CLI with indicatif spinner:**
//! ```ignore
//! let spinner = ProgressBar::new_spinner();
//! let reporter = ClosureReporter::new(move |event| {
//!     if let BuildEvent::StepStarted { step } = &event {
//!         spinner.set_message(step.label.clone());
//!     }
//! });
//! ```
//!
//! **napi (Node.js):**
//! ```ignore
//! let reporter = ClosureReporter::new(move |event| {
//!     callback.call(event, ThreadsafeFunctionCallMode::NonBlocking);
//! });
//! ```
//!
//! **REST polling (store last state):**
//! ```ignore
//! let state = Arc::new(Mutex::new(None));
//! let reporter = ClosureReporter::new({
//!     let state = state.clone();
//!     move |event| { *state.lock().unwrap() = Some(event); }
//! });
//! // GET /build/status → returns state.lock().unwrap().clone()
//! ```

use std::path::PathBuf;

use crate::git::ResolvedRef;

// =============================================================================
// Progress Reporter Trait
// =============================================================================

/// Trait for receiving build progress events.
///
/// Implement this trait to receive build updates. The reporter is called
/// synchronously inline during the build — no threads or channels needed.
///
/// Consumers who need async delivery (e.g., pushing to a websocket) should
/// buffer internally (e.g., via `tokio::sync::mpsc`).
///
/// # Thread Safety
///
/// Reporters must be `Send + Sync` to support async build operations.
/// For mutable state, use interior mutability (`Mutex`, `AtomicUsize`, etc.).
pub trait ProgressReporter: Send + Sync {
    /// Called when a build event occurs.
    ///
    /// This method is called synchronously during the build. Implementations
    /// should return quickly to avoid slowing the build process.
    fn report(&self, event: &BuildEvent);
}

/// A no-op reporter that discards all events.
///
/// Used as the default when no consumer provides a reporter.
/// Zero overhead — all calls are inlined away.
pub struct NullReporter;

impl ProgressReporter for NullReporter {
    #[inline]
    fn report(&self, _event: &BuildEvent) {}
}

/// A reporter backed by a closure.
///
/// This is the most common way to create a reporter for simple consumers.
///
/// # Example
///
/// ```
/// use apvm_core::build::progress::{ClosureReporter, BuildEvent};
///
/// let reporter = ClosureReporter::new(|event| {
///     println!("Event: {:?}", event);
/// });
/// ```
pub struct ClosureReporter<F>
where
    F: Fn(&BuildEvent) + Send + Sync,
{
    callback: F,
}

impl<F> ClosureReporter<F>
where
    F: Fn(&BuildEvent) + Send + Sync,
{
    /// Create a new closure-based reporter.
    pub fn new(callback: F) -> Self {
        Self { callback }
    }
}

impl<F> ProgressReporter for ClosureReporter<F>
where
    F: Fn(&BuildEvent) + Send + Sync,
{
    fn report(&self, event: &BuildEvent) {
        (self.callback)(event);
    }
}

// =============================================================================
// Build Events
// =============================================================================

/// Events emitted during a build operation.
///
/// These events form a complete timeline of the build process, from cloning
/// through to artifact collection. Consumers can filter events as needed.
///
/// # Event Flow
///
/// ```text
/// PhaseStarted(Clone) → PhaseCompleted(Clone)
/// PhaseStarted(Checkout) → PhaseCompleted(Checkout)
/// PhaseStarted(DependencyCheck) → PhaseCompleted(DependencyCheck)
/// PhaseStarted(PreBuild) → PhaseCompleted(PreBuild)
/// PhaseStarted(Build) →
///   StepStarted { "Installing dependencies", "composer install ..." }
///   StepCompleted { "Installing dependencies", ... }
///   StepStarted { "Creating archive", "zip -r ..." }
///   StepCompleted { "Creating archive", ... }
/// PhaseCompleted(Build)
/// PhaseStarted(PostBuild) → PhaseCompleted(PostBuild)
/// PhaseStarted(CollectArtifacts) → PhaseCompleted(CollectArtifacts)
/// BuildSucceeded { artifacts }
/// ```
#[derive(Debug, Clone)]
pub enum BuildEvent {
    /// A high-level build phase has started.
    PhaseStarted {
        /// The phase that started.
        phase: BuildPhase,
        /// Human-readable description of what's happening.
        message: String,
    },

    /// A high-level build phase has completed.
    PhaseCompleted {
        /// The phase that completed.
        phase: BuildPhase,
    },

    /// The input reference was resolved to a concrete source.
    ///
    /// Emitted once, early — after the reference is resolved but before the
    /// clone/download and before the cache is consulted — so consumers can
    /// surface *what* is being built (branch, tag, commit, pull request, or
    /// release) without parsing free-text phase messages. For example, the CLI
    /// prints the resolved reference above its progress spinner.
    ReferenceResolved {
        /// The resolved reference: its source kind, the ref that will be
        /// checked out, and (when known this early) the commit SHA.
        resolved: ResolvedRef,
    },

    /// An individual build step has started.
    ///
    /// Steps are finer-grained than phases — each shell command or
    /// hook operation is a step within a phase.
    StepStarted {
        /// The step that started.
        step: BuildStep,
    },

    /// An individual build step has completed.
    StepCompleted {
        /// The step that completed.
        step: BuildStep,
    },

    /// A line of command output was captured.
    ///
    /// Most consumers ignore this; only `--verbose` CLI or debug logging
    /// displays it. Emitted per-line during command execution.
    CommandOutput {
        /// Which output stream the line came from.
        stream: OutputStream,
        /// The output line (without trailing newline).
        line: String,
    },

    /// A non-fatal warning occurred during the build.
    Warning(String),

    /// The build completed successfully.
    BuildSucceeded {
        /// Paths to the produced artifacts.
        artifacts: Vec<PathBuf>,
    },

    /// The build failed.
    BuildFailed {
        /// Human-readable reason for the failure.
        reason: String,
    },
}

// =============================================================================
// Build Phase
// =============================================================================

/// High-level phases of the build process.
///
/// Phases represent major stages in the build lifecycle. Each phase
/// contains zero or more steps (individual commands or operations).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildPhase {
    /// Pre-clone verification of GitHub references (PR existence, etc.).
    ///
    /// This phase runs before cloning to fail fast when an explicit
    /// reference (e.g., `pr:123`) does not exist.
    Preflight,
    /// Consulting the artifact cache for a prior build of the resolved commit.
    ///
    /// Runs before cloning; a cache hit can skip the clone/build entirely.
    Cache,
    /// Downloading pre-built assets from a GitHub Release.
    ///
    /// This phase replaces the entire clone → build pipeline when a release
    /// is available. Assets are downloaded in parallel directly to the output directory.
    ReleaseDownload,
    /// Cloning the git repository.
    Clone,
    /// Checking out the target ref (branch, tag, PR, commit).
    Checkout,
    /// Verifying tool dependencies (npm, composer, etc.).
    DependencyCheck,
    /// Running builder's pre-build hook.
    PreBuild,
    /// Executing setup commands (dependency installation).
    Setup,
    /// Running the main build commands.
    Build,
    /// Running builder's build hook.
    BuildHook,
    /// Running builder's post-build hook.
    PostBuild,
    /// Collecting and verifying build artifacts.
    CollectArtifacts,
}

impl std::fmt::Display for BuildPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preflight => write!(f, "Verifying reference"),
            Self::Cache => write!(f, "Checking cache"),
            Self::ReleaseDownload => write!(f, "Downloading release assets"),
            Self::Clone => write!(f, "Cloning repository"),
            Self::Checkout => write!(f, "Checking out ref"),
            Self::DependencyCheck => write!(f, "Checking dependencies"),
            Self::PreBuild => write!(f, "Pre-build"),
            Self::Setup => write!(f, "Setup"),
            Self::Build => write!(f, "Building"),
            Self::BuildHook => write!(f, "Build hook"),
            Self::PostBuild => write!(f, "Post-build"),
            Self::CollectArtifacts => write!(f, "Collecting artifacts"),
        }
    }
}

// =============================================================================
// Build Step
// =============================================================================

/// A single build step within a phase.
///
/// Steps carry semantic labels from the builder alongside the raw commands,
/// bridging the gap between what the runner executes and what the user sees.
///
/// # Example
///
/// ```
/// use apvm_core::build::progress::BuildStep;
///
/// let step = BuildStep::new(
///     "Installing production dependencies",
///     "composer install --no-dev --no-scripts",
/// );
/// assert_eq!(step.label, "Installing production dependencies");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildStep {
    /// Human-readable description of what this step does.
    ///
    /// Displayed to users (e.g., "Installing production dependencies").
    /// The builder provides this — the runner doesn't know what commands mean.
    pub label: String,

    /// The actual shell command being executed.
    ///
    /// Used for verbose/debug output. Not shown in normal mode.
    pub command: String,
}

impl BuildStep {
    /// Create a new build step.
    ///
    /// # Arguments
    ///
    /// * `label` - Human-readable description (shown to users)
    /// * `command` - The shell command (shown in verbose/debug mode)
    pub fn new(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            command: command.into(),
        }
    }
}

impl std::fmt::Display for BuildStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label)
    }
}

// =============================================================================
// Output Stream
// =============================================================================

/// Which output stream a command line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl std::fmt::Display for OutputStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdout => write!(f, "stdout"),
            Self::Stderr => write!(f, "stderr"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_null_reporter_accepts_all_events() {
        let reporter = NullReporter;
        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Preflight,
            message: "Verifying...".into(),
        });
        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Preflight,
        });
        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Clone,
            message: "Cloning...".into(),
        });
        reporter.report(&BuildEvent::BuildSucceeded { artifacts: vec![] });
        // No panic = success
    }

    #[test]
    fn test_closure_reporter_receives_events() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = count.clone();

        let reporter = ClosureReporter::new(move |_event| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });

        reporter.report(&BuildEvent::PhaseStarted {
            phase: BuildPhase::Build,
            message: "Building...".into(),
        });
        reporter.report(&BuildEvent::PhaseCompleted {
            phase: BuildPhase::Build,
        });

        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_build_step_display() {
        let step = BuildStep::new("Installing deps", "npm install");
        assert_eq!(step.to_string(), "Installing deps");
        assert_eq!(step.label, "Installing deps");
        assert_eq!(step.command, "npm install");
    }

    #[test]
    fn test_build_phase_display() {
        assert_eq!(BuildPhase::Preflight.to_string(), "Verifying reference");
        assert_eq!(BuildPhase::Cache.to_string(), "Checking cache");
        assert_eq!(BuildPhase::Clone.to_string(), "Cloning repository");
        assert_eq!(BuildPhase::Build.to_string(), "Building");
        assert_eq!(
            BuildPhase::CollectArtifacts.to_string(),
            "Collecting artifacts"
        );
    }

    #[test]
    fn test_output_stream_display() {
        assert_eq!(OutputStream::Stdout.to_string(), "stdout");
        assert_eq!(OutputStream::Stderr.to_string(), "stderr");
    }

    #[test]
    fn test_closure_reporter_captures_event_data() {
        use std::sync::{Arc, Mutex};

        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();

        let reporter = ClosureReporter::new(move |event| {
            if let BuildEvent::StepStarted { step } = event {
                events_clone.lock().unwrap().push(step.label.clone());
            }
        });

        reporter.report(&BuildEvent::StepStarted {
            step: BuildStep::new("Step 1", "cmd1"),
        });
        reporter.report(&BuildEvent::StepStarted {
            step: BuildStep::new("Step 2", "cmd2"),
        });
        // Non-step event should be ignored by this reporter
        reporter.report(&BuildEvent::Warning("something".into()));

        let captured = events.lock().unwrap();
        assert_eq!(*captured, vec!["Step 1", "Step 2"]);
    }
}
