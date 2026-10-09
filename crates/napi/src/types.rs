//! JavaScript-compatible type definitions for APVM build operations.
//!
//! This module provides N-API-compatible representations of all core types
//! needed for build operations: events, phases, steps, outputs, and artifacts.
//!
//! # Type Mapping Strategy
//!
//! | Rust Type          | JS Representation       | Rationale                          |
//! |--------------------|-------------------------|------------------------------------|
//! | `BuildPhase`       | String enum             | Ergonomic in TS switch statements  |
//! | `OutputStream`     | String enum             | Only two variants                  |
//! | `BuildStep`        | Plain object            | Simple data transfer               |
//! | `ProducedArtifact` | Plain object            | Simple data transfer               |
//! | `BuildResult`      | Plain object            | Simple data transfer               |
//! | `BuildOutput`      | Plain object            | Immutable result data              |
//! | `RefSource`        | Tagged union (object)   | Discriminated union pattern in TS  |
//! | `ResolvedRef`      | Plain object            | Simple data transfer               |
//! | `BuildEvent`       | Tagged union (object)   | Discriminated union pattern in TS  |

use napi_derive::napi;

// =============================================================================
// Build Phase (String Enum)
// =============================================================================

/// High-level phases of the build process.
///
/// Each build goes through these phases in order. Use this to track
/// overall build progress in your UI.
///
/// Members are plain enumerable properties, like a compiled TypeScript
/// enum: `Object.values()` lists every value in declaration order.
///
/// # TypeScript
///
/// ```typescript
/// switch (event.phase) {
///   case 'Clone': console.log('Cloning repository...'); break;
///   case 'Build': console.log('Building plugin...'); break;
///   case 'CollectArtifacts': console.log('Almost done...'); break;
/// }
/// ```
#[napi(string_enum)]
pub enum JsBuildPhase {
    /// Pre-clone verification of GitHub references (PR existence, etc.).
    Preflight,
    /// Consulting the artifact cache for a prior build of the resolved commit.
    Cache,
    /// Downloading pre-built assets from a GitHub Release.
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

impl From<apvm_core::BuildPhase> for JsBuildPhase {
    fn from(phase: apvm_core::BuildPhase) -> Self {
        match phase {
            apvm_core::BuildPhase::Preflight => Self::Preflight,
            apvm_core::BuildPhase::Cache => Self::Cache,
            apvm_core::BuildPhase::ReleaseDownload => Self::ReleaseDownload,
            apvm_core::BuildPhase::Clone => Self::Clone,
            apvm_core::BuildPhase::Checkout => Self::Checkout,
            apvm_core::BuildPhase::DependencyCheck => Self::DependencyCheck,
            apvm_core::BuildPhase::PreBuild => Self::PreBuild,
            apvm_core::BuildPhase::Setup => Self::Setup,
            apvm_core::BuildPhase::Build => Self::Build,
            apvm_core::BuildPhase::BuildHook => Self::BuildHook,
            apvm_core::BuildPhase::PostBuild => Self::PostBuild,
            apvm_core::BuildPhase::CollectArtifacts => Self::CollectArtifacts,
        }
    }
}

// =============================================================================
// Output Stream (String Enum)
// =============================================================================

/// Which output stream a command line came from.
///
/// Used in `CommandOutput` events to distinguish between stdout and stderr.
///
/// Members are plain enumerable properties, like a compiled TypeScript
/// enum: `Object.values()` lists every value in declaration order.
#[napi(string_enum)]
pub enum JsOutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl From<apvm_core::build::progress::OutputStream> for JsOutputStream {
    fn from(stream: apvm_core::build::progress::OutputStream) -> Self {
        match stream {
            apvm_core::build::progress::OutputStream::Stdout => Self::Stdout,
            apvm_core::build::progress::OutputStream::Stderr => Self::Stderr,
        }
    }
}

// =============================================================================
// Release Selector (String Enum)
// =============================================================================

/// Selects which GitHub Release to download.
///
/// Use with [`Apvm.downloadReleaseBySelector()`] to dynamically resolve
/// the latest or previous release without knowing the exact tag.
/// Drafts are always excluded.
///
/// Members are plain enumerable properties, like a compiled TypeScript
/// enum: `Object.values()` lists every value in declaration order.
///
/// | Variant          | Resolves to                                                |
/// |------------------|------------------------------------------------------------|
/// | `LatestStable`   | Latest non-prerelease, non-draft release                   |
/// | `PreviousStable` | Previous non-prerelease, non-draft release                 |
/// | `Latest`         | Very latest non-draft release (including prereleases)      |
/// | `PreviousLatest` | Previous non-draft release                                 |
///
/// References:
/// - <https://docs.github.com/en/rest/releases/releases#get-the-latest-release>
/// - <https://docs.github.com/en/rest/releases/releases#list-releases>
///
/// # TypeScript
///
/// ```typescript
/// import { Apvm, JsReleaseSelector } from 'apvm-napi';
///
/// const output = await apvm.downloadReleaseBySelector(
///   'backwpup',
///   JsReleaseSelector.LatestStable,
///   '/tmp/output',
/// );
/// ```
#[napi(string_enum)]
pub enum JsReleaseSelector {
    /// Latest stable release (non-prerelease, non-draft).
    LatestStable,
    /// Previous stable release.
    PreviousStable,
    /// Very latest non-draft release (including prereleases).
    Latest,
    /// Previous non-draft release.
    PreviousLatest,
}

// =============================================================================
// Build Step (Plain Object)
// =============================================================================

/// A single build step within a phase.
///
/// Steps represent individual shell commands or operations. The `label`
/// is a human-readable description suitable for display, while `command`
/// contains the actual shell command (useful for debug/verbose logging).
///
/// # TypeScript
///
/// ```typescript
/// console.log(`Running: ${step.label}`);
/// // verbose mode:
/// console.log(`  $ ${step.command}`);
/// ```
#[napi(object)]
pub struct JsBuildStep {
    /// Human-readable description of what this step does.
    ///
    /// Example: `"Installing production dependencies"`
    pub label: String,
    /// The actual shell command being executed.
    ///
    /// Example: `"composer install --no-dev --no-scripts"`
    pub command: String,
}

impl From<&apvm_core::BuildStep> for JsBuildStep {
    fn from(step: &apvm_core::BuildStep) -> Self {
        Self {
            label: step.label.clone(),
            command: step.command.clone(),
        }
    }
}

// =============================================================================
// Ref Source (Tagged Union)
// =============================================================================

/// The type of git reference that was resolved.
///
/// This uses a discriminated union pattern — check the `type` field
/// to determine the variant, then read `value` for the associated data.
///
/// # TypeScript
///
/// ```typescript
/// switch (source.type) {
///   case 'pull_request': console.log(`PR #${source.value}`); break;
///   case 'branch': console.log(`Branch: ${source.value}`); break;
///   case 'tag': console.log(`Tag: ${source.value}`); break;
///   case 'commit': console.log(`Commit: ${source.value}`); break;
///   case 'release': console.log(`Release: ${source.value}`); break;
/// }
/// ```
#[napi(object)]
pub struct JsRefSource {
    /// The type of reference: `"pull_request"`, `"branch"`, `"tag"`, `"commit"`, or `"release"`.
    #[napi(js_name = "type")]
    pub kind: String,
    /// The value associated with the reference type.
    ///
    /// - For `pull_request`: the PR number as a string (e.g., `"123"`)
    /// - For `branch`: the branch name (e.g., `"develop"`)
    /// - For `tag`: the tag name (e.g., `"v1.0.0"`)
    /// - For `commit`: the commit SHA (e.g., `"a1b2c3d"`)
    /// - For `release`: the release tag (e.g., `"5.6.8"`)
    pub value: String,
    /// A human-readable description (e.g., `"PR #123"`, `"branch 'develop'"`)
    pub description: String,
}

impl From<&apvm_core::RefSource> for JsRefSource {
    fn from(source: &apvm_core::RefSource) -> Self {
        let (kind, value) = match source {
            apvm_core::RefSource::PullRequest(n) => ("pull_request".to_string(), n.to_string()),
            apvm_core::RefSource::Branch(s) => ("branch".to_string(), s.clone()),
            apvm_core::RefSource::Tag(s) => ("tag".to_string(), s.clone()),
            apvm_core::RefSource::Commit(s) => ("commit".to_string(), s.clone()),
            apvm_core::RefSource::Release(s) => ("release".to_string(), s.clone()),
        };
        Self {
            kind,
            value,
            description: source.description(),
        }
    }
}

// =============================================================================
// Resolved Ref (Plain Object)
// =============================================================================

/// A resolved git reference with full metadata.
///
/// Returned as part of [`JsBuildOutput`] to describe exactly what was built.
#[napi(object)]
pub struct JsResolvedRef {
    /// The original input from the user (e.g., `"pr:123"`, `"develop"`).
    pub input: String,
    /// The detected source type and value.
    pub source: JsRefSource,
    /// The git ref that was checked out (branch name, tag, or commit SHA).
    pub git_ref: String,
    /// Full commit SHA, if resolved.
    pub commit_sha: Option<String>,
}

impl From<&apvm_core::ResolvedRef> for JsResolvedRef {
    fn from(resolved: &apvm_core::ResolvedRef) -> Self {
        Self {
            input: resolved.input.clone(),
            source: JsRefSource::from(&resolved.source),
            git_ref: resolved.git_ref.clone(),
            commit_sha: resolved.commit_sha.clone(),
        }
    }
}

// =============================================================================
// Produced Artifact (Plain Object)
// =============================================================================

/// A single artifact produced by a build.
///
/// Represents a built file (typically a zip archive) that can be stored
/// or distributed.
///
/// # TypeScript
///
/// ```typescript
/// for (const artifact of output.result.artifacts) {
///   console.log(`${artifact.filename} (${artifact.size} bytes)`);
///   if (artifact.variantId) {
///     console.log(`  Variant: ${artifact.variantId}`);
///   }
/// }
/// ```
#[napi(object)]
pub struct JsProducedArtifact {
    /// Variant ID this artifact belongs to.
    ///
    /// `null` for single-variant projects (e.g., WP Rocket).
    /// Set to variant name for multi-variant projects (e.g., `"pro"`, `"free"`).
    pub variant_id: Option<String>,
    /// Full path to the artifact file on disk.
    pub path: String,
    /// Target filename (how it should be named in storage).
    pub filename: String,
    /// File size in bytes.
    ///
    /// Note: Represented as a JavaScript `number`. Files larger than 2^53 bytes
    /// (8 PB) would lose precision, which is not a practical concern.
    pub size: f64,
    /// Where this artifact came from: `"cache"`, `"built"`, or `"downloaded"`.
    ///
    /// Lets JS consumers report provenance per artifact — including a partial
    /// build where some variants are reused from cache and others are built.
    pub origin: String,
}

impl From<&apvm_core::build::ProducedArtifact> for JsProducedArtifact {
    fn from(artifact: &apvm_core::build::ProducedArtifact) -> Self {
        Self {
            variant_id: artifact.variant_id.clone(),
            path: artifact.path.to_string_lossy().to_string(),
            filename: artifact.filename.clone(),
            size: artifact.size as f64,
            origin: artifact.origin.to_string(),
        }
    }
}

// =============================================================================
// Build Result (Plain Object)
// =============================================================================

/// Result of a successful build operation.
///
/// Contains all produced artifacts, the resolved version, and build context.
#[napi(object)]
pub struct JsBuildResult {
    /// The artifacts produced by the build.
    pub artifacts: Vec<JsProducedArtifact>,
    /// The directory where the build was performed.
    pub build_dir: String,
    /// The version that was built (e.g., `"5.1.0"`, `"3.17.4"`).
    pub version: String,
    /// The variants that were built (empty array if no variants).
    ///
    /// Example: `["pro", "free"]` for BackWPup, `[]` for WP Rocket.
    pub variants_built: Vec<String>,
    /// Total size of all artifacts in bytes.
    pub total_size: f64,
    /// Number of artifacts produced.
    pub artifact_count: u32,
}

impl From<&apvm_core::build::BuildResult> for JsBuildResult {
    fn from(result: &apvm_core::build::BuildResult) -> Self {
        Self {
            artifacts: result
                .artifacts
                .iter()
                .map(JsProducedArtifact::from)
                .collect(),
            build_dir: result.build_dir.to_string_lossy().to_string(),
            version: result.version.clone(),
            variants_built: result.variants_built.clone(),
            total_size: result.total_size() as f64,
            artifact_count: result.artifacts.len() as u32,
        }
    }
}

// =============================================================================
// Version Override (Plain Object)
// =============================================================================

/// A version override applied to a plugin's source before building.
///
/// Present on [`JsBuildOutput::version_override`] when a build rewrote the
/// plugin's source version to the requested version (WP Rocket / Imagify). Lets
/// JS consumers surface that the delivered artifact carries a version different
/// from the one in source.
///
/// # TypeScript
///
/// ```typescript
/// if (output.versionOverride) {
///   const o = output.versionOverride;
///   console.log(`Overrode ${o.from} → ${o.to} in ${o.file} (${o.sites.join(', ')})`);
/// }
/// ```
#[napi(object)]
pub struct JsVersionOverride {
    /// Filename of the plugin file that was rewritten (e.g. `"wp-rocket.php"`).
    pub file: String,
    /// The version found in source before the override.
    pub from: String,
    /// The version written in its place (the caller's requested version).
    pub to: String,
    /// Human-readable labels of the declarations rewritten in `file`,
    /// e.g. `["Version: header", "WP_ROCKET_VERSION"]`.
    pub sites: Vec<String>,
}

impl From<&apvm_core::VersionOverride> for JsVersionOverride {
    fn from(value: &apvm_core::VersionOverride) -> Self {
        Self {
            file: value.file.clone(),
            from: value.from.clone(),
            to: value.to.clone(),
            sites: value.sites.clone(),
        }
    }
}

// =============================================================================
// Build Output (Plain Object)
// =============================================================================

/// Complete output from a build operation.
///
/// This is the main return type from all `Apvm.build*()` methods. It contains
/// the build artifacts, the resolved git reference, and all metadata needed
/// to identify and store the build.
///
/// # TypeScript
///
/// ```typescript
/// const output = await apvm.build({ project: 'wp-rocket', gitRef: 'pr:456', outputDir: '/tmp' });
///
/// console.log(`Built: ${output.description}`);
/// console.log(`Version: ${output.result.version}`);
/// console.log(`Commit: ${output.commitShort}`);
/// console.log(`Artifacts: ${output.result.artifacts.length}`);
///
/// for (const artifact of output.result.artifacts) {
///   console.log(`  ${artifact.filename} (${artifact.size} bytes)`);
/// }
/// ```
#[napi(object)]
pub struct JsBuildOutput {
    /// The build result containing artifacts and version info.
    pub result: JsBuildResult,
    /// The resolved git reference with full metadata.
    pub resolved_ref: JsResolvedRef,
    /// Full commit SHA of what was built.
    pub commit: String,
    /// Short commit SHA (7 characters).
    pub commit_short: String,
    /// Branch name that was checked out.
    pub branch: String,
    /// Human-readable description of what was built.
    ///
    /// Example: `"PR #123 @ a1b2c3d"`, `"branch 'develop' @ e4f5g6h"`
    pub description: String,
    /// `true` when every artifact was served from the cache (no build ran).
    ///
    /// Derived from per-artifact provenance; a partial build (some reused,
    /// some freshly built) is `false`. See each artifact's `origin`.
    pub from_cache: bool,
    /// Set when the build rewrote the plugin's source version to the requested
    /// version (WP Rocket / Imagify). `null` when no override happened: no
    /// version was pinned, artifacts came from the cache, the pinned version
    /// already matched source, or the plugin does not support overrides.
    pub version_override: Option<JsVersionOverride>,
}

impl From<apvm_core::BuildOutput> for JsBuildOutput {
    fn from(output: apvm_core::BuildOutput) -> Self {
        let description = output.description();
        let from_cache = output.from_cache();
        Self {
            result: JsBuildResult::from(&output.result),
            resolved_ref: JsResolvedRef::from(&output.resolved_ref),
            commit: output.commit.clone(),
            commit_short: output.commit_short.clone(),
            branch: output.branch.clone(),
            description,
            from_cache,
            version_override: output
                .version_override
                .as_ref()
                .map(JsVersionOverride::from),
        }
    }
}

// =============================================================================
// Build Event (Tagged Union)
// =============================================================================

/// A build progress event.
///
/// Events use a discriminated union pattern — check the `type` field to
/// determine the event variant, then access the corresponding fields.
///
/// # Event Types
///
/// | `type`             | Fields Available                              |
/// |--------------------|-----------------------------------------------|
/// | `reference_resolved` | `resolvedRef`                               |
/// | `phase_started`    | `phase`, `message`                            |
/// | `phase_completed`  | `phase`                                       |
/// | `step_started`     | `step`                                        |
/// | `step_completed`   | `step`                                        |
/// | `command_output`   | `stream`, `line`                              |
/// | `warning`          | `message`                                     |
/// | `build_succeeded`  | `artifacts` (list of file paths)              |
/// | `build_failed`     | `reason`                                      |
///
/// # TypeScript
///
/// ```typescript
/// apvm.build({
///   project: 'wp-rocket',
///   gitRef: 'pr:456',
///   outputDir: '/tmp',
///   onProgress: (event) => {
///     switch (event.type) {
///       case 'reference_resolved':
///         console.log(`Building ${event.resolvedRef?.source.description}`);
///         break;
///       case 'phase_started':
///         console.log(`[${event.phase}] ${event.message}`);
///         break;
///       case 'step_started':
///         console.log(`  Running: ${event.step?.label}`);
///         break;
///       case 'command_output':
///         if (verbose) console.log(`    [${event.stream}] ${event.line}`);
///         break;
///       case 'build_succeeded':
///         console.log(`Done! ${event.artifacts?.length} artifacts`);
///         break;
///       case 'build_failed':
///         console.error(`FAILED: ${event.reason}`);
///         break;
///     }
///   },
/// });
/// ```
#[napi(object)]
pub struct JsBuildEvent {
    /// Event type discriminator.
    ///
    /// One of: `"reference_resolved"`, `"phase_started"`, `"phase_completed"`,
    /// `"step_started"`, `"step_completed"`, `"command_output"`, `"warning"`,
    /// `"build_succeeded"`, `"build_failed"`.
    #[napi(js_name = "type")]
    pub kind: String,

    /// Build phase (present for `phase_started` and `phase_completed` events).
    pub phase: Option<JsBuildPhase>,

    /// Human-readable message (present for `phase_started` and `warning` events).
    pub message: Option<String>,

    /// Build step details (present for `step_started` and `step_completed` events).
    pub step: Option<JsBuildStep>,

    /// Output stream type (present for `command_output` events).
    pub stream: Option<JsOutputStream>,

    /// Command output line (present for `command_output` events).
    pub line: Option<String>,

    /// Artifact file paths (present for `build_succeeded` events).
    pub artifacts: Option<Vec<String>>,

    /// Failure reason (present for `build_failed` events).
    pub reason: Option<String>,

    /// The resolved reference (present for `reference_resolved` events).
    ///
    /// Describes what the input git ref resolved to — its source kind
    /// (`branch`/`tag`/`commit`/`pull_request`/`release`), the ref that will be
    /// checked out, and the commit SHA when known this early. Lets consumers
    /// display *what* is being built as soon as it is known, before the
    /// clone/download.
    pub resolved_ref: Option<JsResolvedRef>,
}

impl From<&apvm_core::BuildEvent> for JsBuildEvent {
    fn from(event: &apvm_core::BuildEvent) -> Self {
        match event {
            apvm_core::BuildEvent::ReferenceResolved { resolved } => Self {
                kind: "reference_resolved".to_string(),
                phase: None,
                message: None,
                step: None,
                stream: None,
                line: None,
                artifacts: None,
                reason: None,
                resolved_ref: Some(JsResolvedRef::from(resolved)),
            },
            apvm_core::BuildEvent::PhaseStarted { phase, message } => Self {
                kind: "phase_started".to_string(),
                phase: Some(JsBuildPhase::from(*phase)),
                message: Some(message.clone()),
                step: None,
                stream: None,
                line: None,
                artifacts: None,
                reason: None,
                resolved_ref: None,
            },
            apvm_core::BuildEvent::PhaseCompleted { phase } => Self {
                kind: "phase_completed".to_string(),
                phase: Some(JsBuildPhase::from(*phase)),
                message: None,
                step: None,
                stream: None,
                line: None,
                artifacts: None,
                reason: None,
                resolved_ref: None,
            },
            apvm_core::BuildEvent::StepStarted { step } => Self {
                kind: "step_started".to_string(),
                phase: None,
                message: None,
                step: Some(JsBuildStep::from(step)),
                stream: None,
                line: None,
                artifacts: None,
                reason: None,
                resolved_ref: None,
            },
            apvm_core::BuildEvent::StepCompleted { step } => Self {
                kind: "step_completed".to_string(),
                phase: None,
                message: None,
                step: Some(JsBuildStep::from(step)),
                stream: None,
                line: None,
                artifacts: None,
                reason: None,
                resolved_ref: None,
            },
            apvm_core::BuildEvent::CommandOutput { stream, line } => Self {
                kind: "command_output".to_string(),
                phase: None,
                message: None,
                step: None,
                stream: Some(JsOutputStream::from(*stream)),
                line: Some(line.clone()),
                artifacts: None,
                reason: None,
                resolved_ref: None,
            },
            apvm_core::BuildEvent::Warning(msg) => Self {
                kind: "warning".to_string(),
                phase: None,
                message: Some(msg.clone()),
                step: None,
                stream: None,
                line: None,
                artifacts: None,
                reason: None,
                resolved_ref: None,
            },
            apvm_core::BuildEvent::BuildSucceeded { artifacts } => Self {
                kind: "build_succeeded".to_string(),
                phase: None,
                message: None,
                step: None,
                stream: None,
                line: None,
                artifacts: Some(
                    artifacts
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                ),
                reason: None,
                resolved_ref: None,
            },
            apvm_core::BuildEvent::BuildFailed { reason } => Self {
                kind: "build_failed".to_string(),
                phase: None,
                message: None,
                step: None,
                stream: None,
                line: None,
                artifacts: None,
                reason: Some(reason.clone()),
                resolved_ref: None,
            },
        }
    }
}

// =============================================================================
// Build Options (Input Object)
// =============================================================================

/// Options for a build operation.
///
/// Pass this object to any `Apvm.build*()` method to configure the build.
///
/// # Required Fields
///
/// - `project` — Project name (`"backwpup"` or `"wp-rocket"`)
/// - `outputDir` — Directory where artifacts will be placed
///
/// # TypeScript
///
/// ```typescript
/// // Minimal (WP Rocket, version auto-detected)
/// await apvm.build({
///   project: 'wp-rocket',
///   gitRef: 'pr:456',
///   outputDir: '/tmp/output',
/// });
///
/// // Full options (BackWPup, version required), with progress events
/// await apvm.build(
///   {
///     project: 'backwpup',
///     gitRef: 'pr:123',
///     version: '5.1.0',
///     variants: ['pro', 'free'],
///     outputDir: '/tmp/output',
///   },
///   (err, event) => console.log(err ?? event),
/// );
/// ```
#[napi(object)]
pub struct BuildOptions {
    /// The project to build.
    ///
    /// Must be a registered project name: `"backwpup"`, `"wp-rocket"`, or
    /// `"imagify"`. Call `listProjects()` for the authoritative list.
    pub project: String,

    /// Git reference to build from.
    ///
    /// Supports automatic detection or explicit prefixes:
    /// - `"123"` → PR #123 (or branch if PR doesn't exist)
    /// - `"pr:123"` → Force PR interpretation
    /// - `"branch:develop"` → Force branch interpretation
    /// - `"tag:v1.0.0"` → Force tag interpretation
    /// - `"tag:latest-stable"` → Build from the latest stable tag (no alpha/beta/rc)
    /// - `"tag:previous-stable"` → Build from the previous stable tag
    /// - `"tag:latest"` → Build from the very latest tag (including prereleases)
    /// - `"tag:previous-latest"` → Build from the tag before the very latest
    /// - `"commit:a1b2c3d"` → Force commit interpretation
    /// - `"release:5.6.8"` → Download from GitHub Release
    /// - `"release:latest-stable"` → Download the latest stable release (non-prerelease, non-draft)
    /// - `"release:previous-stable"` → Download the previous stable release
    /// - `"release:latest"` → Download the very latest non-draft release (including prereleases)
    /// - `"release:previous-latest"` → Download the previous non-draft release
    /// - `"develop"` → Branch name
    /// - `"v1.0.0"` → Tag (if exists) or branch
    /// - `"5.6.8"` → GitHub Release (if project has releases), else tag/branch
    pub git_ref: String,

    /// Version to build.
    ///
    /// - **BackWPup**: Required (e.g., `"5.1.0"`)
    /// - **WP Rocket / Imagify**: Optional — auto-detected from source when
    ///   omitted, or, when provided and the build path is taken, rewritten into
    ///   the plugin's source so the artifact carries the requested version
    ///   (reported via [`JsBuildOutput::version_override`]).
    pub version: Option<String>,

    /// Specific variants to build.
    ///
    /// When `null` or omitted, default variants are built.
    ///
    /// - **BackWPup**: Supports `["free", "pro-de", "pro-en"]`
    /// - **WP Rocket**: Has no variants (this field is ignored)
    pub variants: Option<Vec<String>>,

    /// Directory where build artifacts will be placed.
    ///
    /// Must be an absolute path. The directory will be created if it
    /// doesn't exist.
    pub output_dir: String,

    /// Bypass the artifact cache for this build (defaults to `false`).
    ///
    /// The build still runs and, unless caching is disabled at the instance
    /// level (`cacheEnabled: false`), still warms the cache.
    pub no_cache: Option<bool>,
}

// =============================================================================
// Warm Options (Input Object)
// =============================================================================

/// Options for a cache-warm operation.
///
/// Pass this object to `Apvm.warmCache()`. Warming runs the **same pipeline**
/// as a build — resolve the ref, reuse whatever is already cached, and build or
/// download only what is missing — then stores everything into the cache. The
/// one difference is that it delivers **nothing** to an output directory; its
/// purpose is to prime the cache so a later `build()` of the same reference is
/// an instant hit.
///
/// It is therefore a deliberately smaller surface than [`BuildOptions`]:
///
/// - **no `outputDir`** — warming never writes artifacts anywhere but the cache;
/// - **no `noCache`** — warming *is* a cache operation, so bypassing the cache
///   would make it a no-op.
///
/// # Required Fields
///
/// - `project` — Project name (`"backwpup"`, `"wp-rocket"`, or `"imagify"`)
/// - `gitRef` — Reference to warm (same syntax as a build)
///
/// # TypeScript
///
/// ```typescript
/// // Warm WP Rocket's develop branch into the cache.
/// await apvm.warmCache({ project: 'wp-rocket', gitRef: 'branch:develop' });
///
/// // Warm a specific BackWPup version + variants, with progress.
/// await apvm.warmCache(
///   {
///     project: 'backwpup',
///     gitRef: 'pr:123',
///     version: '5.1.0',
///     variants: ['free', 'pro-en'],
///   },
///   (err, event) => {
///     if (err || !event) return;
///     console.log(event.type, event.message);
///   },
/// );
/// ```
#[napi(object)]
pub struct WarmOptions {
    /// The project to warm.
    ///
    /// Must be a registered project name: `"backwpup"`, `"wp-rocket"`, or
    /// `"imagify"`. Call `listProjects()` for the authoritative list.
    pub project: String,

    /// Git reference to warm.
    ///
    /// Accepts exactly the same syntax as [`BuildOptions::git_ref`] — automatic
    /// detection or explicit `pr:`/`branch:`/`tag:`/`commit:`/`release:`
    /// prefixes, including the `latest`/`previous` keywords.
    pub git_ref: String,

    /// Version to warm.
    ///
    /// - **BackWPup**: Required (e.g., `"5.1.0"`)
    /// - **WP Rocket / Imagify**: Optional, auto-detected from source if omitted
    ///
    /// Warming always pins this version exactly (there is no lenient fallback):
    /// warming `"5.1.0"` guarantees `5.1.0` ends up cached.
    pub version: Option<String>,

    /// Specific variants to warm.
    ///
    /// When `null` or omitted, the builder's default variants are warmed.
    ///
    /// - **BackWPup**: Supports `["free", "pro-de", "pro-en"]`
    /// - **WP Rocket / Imagify**: Have no variants (this field is ignored)
    pub variants: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use apvm_core::build::progress::OutputStream;
    use apvm_core::build::{BuildResult, ProducedArtifact};
    use apvm_core::{
        ArtifactOrigin, BuildEvent, BuildOutput, BuildPhase, BuildStep, RefSource, ResolvedRef,
        VersionOverride,
    };

    use super::*;

    /// A resolved branch ref at commit `abc1234…`.
    fn branch_ref() -> ResolvedRef {
        ResolvedRef {
            input: "branch:develop".to_string(),
            source: RefSource::Branch("develop".to_string()),
            git_ref: "develop".to_string(),
            commit_sha: Some("abc1234def".to_string()),
        }
    }

    /// An artifact of `size` bytes with `origin`.
    fn artifact(variant: Option<&str>, size: u64, origin: ArtifactOrigin) -> ProducedArtifact {
        ProducedArtifact::new(
            variant.map(str::to_string),
            PathBuf::from(format!("/out/{size}.zip")),
            format!("{size}.zip"),
            size,
        )
        .with_origin(origin)
    }

    /// A build output for `resolved` holding `artifacts`.
    fn output(resolved: ResolvedRef, artifacts: Vec<ProducedArtifact>) -> BuildOutput {
        BuildOutput {
            result: BuildResult::new(
                artifacts,
                PathBuf::from("/out"),
                "3.17.0".to_string(),
                vec!["free".to_string()],
            ),
            resolved_ref: resolved,
            commit: "abc1234def".to_string(),
            commit_short: "abc1234".to_string(),
            branch: "develop".to_string(),
            version_override: None,
        }
    }

    /// Whether every optional payload field of `event` is unset — the
    /// fields a JS consumer sees as absent.
    fn payload_is_empty(event: &JsBuildEvent) -> bool {
        event.phase.is_none()
            && event.message.is_none()
            && event.step.is_none()
            && event.stream.is_none()
            && event.line.is_none()
            && event.artifacts.is_none()
            && event.reason.is_none()
            && event.resolved_ref.is_none()
    }

    #[test]
    fn every_build_phase_maps_to_its_namesake() {
        // A swapped arm would hand JS consumers the wrong phase.
        macro_rules! same {
            ($($phase:ident),+) => {$(
                assert!(
                    matches!(JsBuildPhase::from(BuildPhase::$phase), JsBuildPhase::$phase),
                    stringify!($phase)
                );
            )+};
        }
        same!(
            Preflight,
            Cache,
            ReleaseDownload,
            Clone,
            Checkout,
            DependencyCheck,
            PreBuild,
            Setup,
            Build,
            BuildHook,
            PostBuild,
            CollectArtifacts
        );
    }

    #[test]
    fn output_streams_map_to_their_namesakes() {
        assert!(matches!(
            JsOutputStream::from(OutputStream::Stdout),
            JsOutputStream::Stdout
        ));
        assert!(matches!(
            JsOutputStream::from(OutputStream::Stderr),
            JsOutputStream::Stderr
        ));
    }

    #[test]
    fn ref_sources_use_the_documented_type_strings() {
        // `type` is a documented discriminant JS code switches on.
        let cases = [
            (RefSource::PullRequest(42), "pull_request", "42", "PR #42"),
            (
                RefSource::Branch("dev".into()),
                "branch",
                "dev",
                "branch 'dev'",
            ),
            (RefSource::Tag("v1".into()), "tag", "v1", "tag 'v1'"),
            (
                RefSource::Commit("abcdef123456".into()),
                "commit",
                "abcdef123456",
                "commit abcdef1",
            ),
            (
                RefSource::Release("v2".into()),
                "release",
                "v2",
                "release 'v2'",
            ),
        ];
        for (source, kind, value, description) in cases {
            let js = JsRefSource::from(&source);
            assert_eq!(
                (js.kind.as_str(), js.value.as_str(), js.description.as_str()),
                (kind, value, description)
            );
        }
    }

    #[test]
    fn resolved_refs_keep_every_field() {
        let js = JsResolvedRef::from(&branch_ref());
        assert_eq!(js.input, "branch:develop");
        assert_eq!(js.source.kind, "branch");
        assert_eq!(js.git_ref, "develop");
        assert_eq!(js.commit_sha.as_deref(), Some("abc1234def"));
    }

    #[test]
    fn artifacts_report_origin_strings_and_sizes() {
        for (origin, name) in [
            (ArtifactOrigin::Built, "built"),
            (ArtifactOrigin::Cache, "cache"),
            (ArtifactOrigin::Downloaded, "downloaded"),
        ] {
            let js = JsProducedArtifact::from(&artifact(Some("pro"), 7, origin));
            assert_eq!(js.origin, name);
            assert_eq!(js.variant_id.as_deref(), Some("pro"));
            assert_eq!(js.path, "/out/7.zip");
            assert_eq!(js.filename, "7.zip");
            assert_eq!(js.size, 7.0);
        }
    }

    #[test]
    fn build_results_carry_totals_computed_from_the_artifacts() {
        let result = BuildResult::new(
            vec![
                artifact(None, 3, ArtifactOrigin::Built),
                artifact(None, 4, ArtifactOrigin::Built),
            ],
            PathBuf::from("/out"),
            "1.0".to_string(),
            vec!["free".to_string(), "pro".to_string()],
        );
        let js = JsBuildResult::from(&result);
        assert_eq!((js.artifact_count, js.total_size), (2, 7.0));
        assert_eq!(js.artifacts.len(), 2);
        assert_eq!(js.build_dir, "/out");
        assert_eq!(js.version, "1.0");
        assert_eq!(js.variants_built, ["free", "pro"]);
    }

    #[test]
    fn build_output_is_from_cache_only_when_every_artifact_is() {
        let all_cached = output(
            branch_ref(),
            vec![
                artifact(None, 1, ArtifactOrigin::Cache),
                artifact(None, 2, ArtifactOrigin::Cache),
            ],
        );
        assert!(JsBuildOutput::from(all_cached).from_cache);

        let mixed = output(
            branch_ref(),
            vec![
                artifact(None, 1, ArtifactOrigin::Cache),
                artifact(None, 2, ArtifactOrigin::Built),
            ],
        );
        assert!(!JsBuildOutput::from(mixed).from_cache);

        // Nothing produced is not a cache hit.
        assert!(!JsBuildOutput::from(output(branch_ref(), vec![])).from_cache);
    }

    #[test]
    fn build_output_describes_the_ref_and_keeps_its_metadata() {
        let pr = ResolvedRef {
            input: "pr:12".to_string(),
            source: RefSource::PullRequest(12),
            git_ref: "feature/x".to_string(),
            commit_sha: None,
        };
        let mut built = output(pr, vec![artifact(None, 1, ArtifactOrigin::Built)]);
        built.version_override = Some(VersionOverride {
            file: "wp-rocket.php".to_string(),
            from: "3.16".to_string(),
            to: "3.17.0".to_string(),
            sites: vec!["header".to_string(), "constant".to_string()],
        });
        let js = JsBuildOutput::from(built);
        assert_eq!(js.description, "PR #12 (branch: feature/x) @ abc1234");
        assert_eq!(js.commit, "abc1234def");
        assert_eq!(js.commit_short, "abc1234");
        assert_eq!(js.branch, "develop");
        assert_eq!(js.resolved_ref.source.kind, "pull_request");
        let applied = js.version_override.expect("the override is passed through");
        assert_eq!(
            (applied.file, applied.from, applied.to, applied.sites),
            (
                "wp-rocket.php".to_string(),
                "3.16".to_string(),
                "3.17.0".to_string(),
                vec!["header".to_string(), "constant".to_string()]
            )
        );
        assert!(
            JsBuildOutput::from(output(branch_ref(), vec![]))
                .version_override
                .is_none()
        );
    }

    #[test]
    fn each_event_sets_its_type_and_only_its_own_payload() {
        let step = BuildStep::new("Install", "npm ci");
        let cases: Vec<(BuildEvent, &str)> = vec![
            (
                BuildEvent::ReferenceResolved {
                    resolved: branch_ref(),
                },
                "reference_resolved",
            ),
            (
                BuildEvent::PhaseStarted {
                    phase: BuildPhase::Clone,
                    message: "Cloning".to_string(),
                },
                "phase_started",
            ),
            (
                BuildEvent::PhaseCompleted {
                    phase: BuildPhase::Clone,
                },
                "phase_completed",
            ),
            (
                BuildEvent::StepStarted { step: step.clone() },
                "step_started",
            ),
            (BuildEvent::StepCompleted { step }, "step_completed"),
            (
                BuildEvent::CommandOutput {
                    stream: OutputStream::Stderr,
                    line: "warn".to_string(),
                },
                "command_output",
            ),
            (BuildEvent::Warning("w".to_string()), "warning"),
            (
                BuildEvent::BuildSucceeded {
                    artifacts: vec![PathBuf::from("/out/a.zip")],
                },
                "build_succeeded",
            ),
            (
                BuildEvent::BuildFailed {
                    reason: "boom".to_string(),
                },
                "build_failed",
            ),
        ];
        for (event, kind) in cases {
            let js = JsBuildEvent::from(&event);
            assert_eq!(js.kind, kind);
            // Clear the fields this kind owns; anything left set leaked in.
            let rest = match kind {
                "reference_resolved" => {
                    assert_eq!(
                        js.resolved_ref.as_ref().map(|r| r.git_ref.as_str()),
                        Some("develop")
                    );
                    JsBuildEvent {
                        resolved_ref: None,
                        ..js
                    }
                }
                "phase_started" => {
                    assert!(matches!(js.phase, Some(JsBuildPhase::Clone)));
                    assert_eq!(js.message.as_deref(), Some("Cloning"));
                    JsBuildEvent {
                        phase: None,
                        message: None,
                        ..js
                    }
                }
                "phase_completed" => {
                    assert!(matches!(js.phase, Some(JsBuildPhase::Clone)));
                    JsBuildEvent { phase: None, ..js }
                }
                "step_started" | "step_completed" => {
                    let step = js.step.as_ref().expect("a step event carries its step");
                    assert_eq!(
                        (step.label.as_str(), step.command.as_str()),
                        ("Install", "npm ci")
                    );
                    JsBuildEvent { step: None, ..js }
                }
                "command_output" => {
                    assert!(matches!(js.stream, Some(JsOutputStream::Stderr)));
                    assert_eq!(js.line.as_deref(), Some("warn"));
                    JsBuildEvent {
                        stream: None,
                        line: None,
                        ..js
                    }
                }
                "warning" => {
                    assert_eq!(js.message.as_deref(), Some("w"));
                    JsBuildEvent {
                        message: None,
                        ..js
                    }
                }
                "build_succeeded" => {
                    assert_eq!(
                        js.artifacts.as_deref(),
                        Some(&["/out/a.zip".to_string()][..])
                    );
                    JsBuildEvent {
                        artifacts: None,
                        ..js
                    }
                }
                "build_failed" => {
                    assert_eq!(js.reason.as_deref(), Some("boom"));
                    JsBuildEvent { reason: None, ..js }
                }
                other => panic!("unexpected kind {other}"),
            };
            assert!(
                payload_is_empty(&rest),
                "{kind} set a field it does not own"
            );
        }
    }
}
