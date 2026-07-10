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
    /// `true` when a cache hit returned a version different from the one
    /// requested (only possible without `strictVersion`). When `true`,
    /// `requestedVersion` is what you asked for and `result.version` is what
    /// was delivered.
    pub cache_version_mismatch: bool,
    /// The version the caller requested (`version` in the build options), if
    /// any — retained so a version mismatch can be reported.
    pub requested_version: Option<String>,
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
            cache_version_mismatch: output.cache_version_mismatch,
            requested_version: output.requested_version.clone(),
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
/// // Full options (BackWPup, version required)
/// await apvm.build({
///   project: 'backwpup',
///   gitRef: 'pr:123',
///   version: '5.1.0',
///   variants: ['pro', 'free'],
///   outputDir: '/tmp/output',
///   onProgress: (event) => console.log(event),
/// });
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
    /// - **WP Rocket**: Optional, auto-detected from source code if omitted
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

    /// Require a cache hit to match `version` exactly (defaults to `false`).
    ///
    /// Without this, a cached build of the same commit at a different version
    /// may be reused (with `cacheVersionMismatch` set on the result). Has no
    /// effect for projects whose version is embedded in source.
    pub strict_version: Option<bool>,
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
///   would make it a no-op;
/// - **no `strictVersion`** — warming always pins the requested version exactly.
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
