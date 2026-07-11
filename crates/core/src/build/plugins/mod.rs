//! Project-specific builders.
//!
//! This module defines the [`Builder`] trait and project implementations.
//! See [`VersionRequirement`] for how different projects handle versions.

mod bwu;
mod imagify;
mod wpr;

use std::path::Path;

pub use bwu::BackWPupBuilder;
pub use imagify::ImagifyBuilder;
pub use wpr::WpRocketBuilder;

use crate::Result;
use crate::error::Error;

use super::BuildContext;
use super::progress::{BuildStep, ProgressReporter};

// =============================================================================
// Version Requirement
// =============================================================================

/// Specifies how a builder handles version during the build process.
///
/// This enum controls:
/// 1. Whether a version parameter is required, forbidden, or optional
/// 2. Whether version detection should be used
/// 3. Error messages when the wrong combination is provided
///
/// # Decision Guide
///
/// Ask yourself: **Does the build process use the version parameter?**
///
/// - **Yes, it's required** → [`Required`](Self::Required)
///   - Example: `gulp --packageVersion="5.1.0"`
///   - The version is injected into the output
///
/// - **No, it's ignored** → [`Embedded`](Self::Embedded)
///   - Example: WP Rocket just packages files; version is in PHP header
///   - Build output version is determined by source files
///
/// - **It can override** → [`Optional`](Self::Optional)
///   - Example: A plugin where you want "nightly" or custom version labels
///   - Version can be provided to override source, or detected if not provided
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VersionRequirement {
    /// Version **must** be provided externally.
    ///
    /// Use this when the build process requires a version parameter:
    /// - Build commands use the version (e.g., `gulp --packageVersion`)
    /// - The version is embedded into the output artifact
    /// - Source file version may not match the intended release version
    ///
    /// # Behavior
    ///
    /// | `version` param | Result |
    /// |-----------------|--------|
    /// | `Some("X.Y.Z")` | ✅ Use provided version |
    /// | `None` | ❌ Error: "version required" |
    ///
    /// # Example
    ///
    /// ```ignore
    /// // BackWPup uses gulp --packageVersion
    /// fn version_requirement(&self) -> VersionRequirement {
    ///     VersionRequirement::Required
    /// }
    /// ```
    Required,

    /// Version is embedded in source files; provided version is ignored.
    ///
    /// Use this when:
    /// - The build process ignores the version parameter entirely
    /// - Version is hardcoded in source (e.g., PHP plugin header)
    ///
    /// # Behavior
    ///
    /// | `version` param | Result |
    /// |-----------------|--------|
    /// | `None` | ✅ Detect from source via `detect_version()` |
    /// | `Some("X.Y.Z")` | Ignore passed version ⚠️ Warning logged, then detect from source |
    ///
    /// # Example
    ///
    /// ```ignore
    /// // WP Rocket has version in wp-rocket.php header
    /// fn version_requirement(&self) -> VersionRequirement {
    ///     VersionRequirement::Embedded
    /// }
    ///
    /// fn detect_version(&self, working_dir: &Path) -> Result<Option<String>> {
    ///     detect_wordpress_plugin_version(&working_dir.join("wp-rocket.php"))
    /// }
    /// ```
    Embedded,

    /// Version can be provided OR auto-detected (most flexible).
    ///
    /// Use this when:
    /// - Version is usually in source files
    /// - But you want to allow overriding (e.g., "nightly", "custom-build")
    /// - The build process can optionally use the version
    ///
    /// # Behavior
    ///
    /// | `version` param | Result |
    /// |-----------------|--------|
    /// | `Some("X.Y.Z")` | ✅ Use provided version (override source) |
    /// | `None` | ✅ Detect from source via `detect_version()` |
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Plugin with optional version override
    /// fn version_requirement(&self) -> VersionRequirement {
    ///     VersionRequirement::Optional
    /// }
    ///
    /// fn detect_version(&self, working_dir: &Path) -> Result<Option<String>> {
    ///     detect_wordpress_plugin_version(&working_dir.join("my-plugin.php"))
    /// }
    /// ```
    #[default]
    Optional,
}

impl VersionRequirement {
    /// Returns `true` if version parameter must be provided.
    pub fn is_required(&self) -> bool {
        matches!(self, Self::Required)
    }

    /// Returns `true` if version parameter must NOT be provided.
    pub fn is_embedded(&self) -> bool {
        matches!(self, Self::Embedded)
    }

    /// Returns `true` if version parameter is optional.
    pub fn is_optional(&self) -> bool {
        matches!(self, Self::Optional)
    }

    /// Returns a human-readable description for error messages.
    pub fn description(&self) -> &'static str {
        match self {
            Self::Required => "version must be provided (build commands use it)",
            Self::Embedded => "version is embedded in source (build ignores parameter)",
            Self::Optional => "version can be provided or auto-detected",
        }
    }
}

impl std::fmt::Display for VersionRequirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Required => write!(f, "Required"),
            Self::Embedded => write!(f, "Embedded"),
            Self::Optional => write!(f, "Optional"),
        }
    }
}

// =============================================================================
// Version Override
// =============================================================================

/// A version override applied to a plugin's checked-out source before building.
///
/// Produced by [`Builder::apply_version_override`] when a caller pins an
/// explicit version for a plugin whose version lives in its source (WP Rocket,
/// Imagify). Reported on the build output so consumers can surface that the
/// delivered artifact carries a version different from the one in source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionOverride {
    /// Filename of the plugin file that was rewritten (e.g. `"wp-rocket.php"`).
    pub file: String,
    /// The version found in source before the override.
    pub from: String,
    /// The version written in its place (the caller's requested version).
    pub to: String,
    /// Human-readable labels of the declarations rewritten in
    /// [`file`](Self::file), e.g. `["Version: header", "WP_ROCKET_VERSION"]`.
    pub sites: Vec<String>,
}

// =============================================================================
// Build Types
// =============================================================================

/// A buildable variant of a project.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BuildVariant {
    /// Unique identifier for this variant.
    pub id: &'static str,
    /// Human-readable name for display.
    pub name: &'static str,
    /// Description of what this variant produces.
    pub description: &'static str,
}

/// Describes a tool dependency for a build process.
///
/// Each dependency has a name (the command to check in PATH), whether it is
/// required or optional, and a list of shell commands to run in order to
/// install the tool when it is missing.
///
/// # Behavior Matrix
///
/// | `required` | `install_commands` | Missing behavior                               |
/// |------------|--------------------|-------------------------------------------------|
/// | `true`     | non-empty          | Run install commands, fail if any command fails  |
/// | `true`     | empty              | Fail immediately with "install it" message       |
/// | `false`    | non-empty          | Run install commands, warn if any command fails  |
/// | `false`    | empty              | Warn and continue                                |
#[derive(Debug, Clone)]
pub struct ToolDependency {
    /// Command name to check in PATH.
    pub name: &'static str,
    /// Whether this tool is required for the build to proceed.
    pub required: bool,
    /// Shell commands to run (in order) to install the tool if missing.
    ///
    /// Multiple commands are useful when installing a tool requires
    /// intermediate steps (e.g., adding a repository before installing
    /// a package).
    pub install_commands: Vec<&'static str>,
}

impl ToolDependency {
    /// Create a required dependency with no auto-install commands.
    ///
    /// The build will fail if this tool is not found in PATH.
    pub fn required(name: &'static str) -> Self {
        Self {
            name,
            required: true,
            install_commands: vec![],
        }
    }

    /// Create a required dependency with auto-install commands.
    ///
    /// If missing, the install commands are run in order.
    /// The build fails if any installation command fails.
    pub fn required_with_install(name: &'static str, install_commands: Vec<&'static str>) -> Self {
        Self {
            name,
            required: true,
            install_commands,
        }
    }

    /// Create an optional dependency with no auto-install commands.
    ///
    /// A warning is printed if missing, but the build continues.
    pub fn optional(name: &'static str) -> Self {
        Self {
            name,
            required: false,
            install_commands: vec![],
        }
    }

    /// Create an optional dependency with auto-install commands.
    ///
    /// If missing, the install commands are run in order. A warning is
    /// printed if any installation command fails, but the build continues.
    pub fn optional_with_install(name: &'static str, install_commands: Vec<&'static str>) -> Self {
        Self {
            name,
            required: false,
            install_commands,
        }
    }
}

/// Describes a build artifact (output file).
#[derive(Debug, Clone)]
pub struct BuildArtifact {
    /// Variant ID this artifact belongs to (None if single-variant project).
    pub variant_id: Option<String>,
    /// Path to the artifact file.
    ///
    /// Can be either:
    /// - **Relative** — resolved against `repo_dir` by the runner (most common)
    /// - **Absolute** — used as-is, for artifacts placed outside the repo
    ///   (e.g., in `workspace_dir`)
    pub source_path: String,
    /// Target filename.
    pub target_name: String,
}

// =============================================================================
// Builder Trait
// =============================================================================

/// Trait for project-specific build logic.
///
/// Implement this trait to add support for building a new project.
///
/// # Version Handling
///
/// The most important decision is [`version_requirement`](Self::version_requirement):
///
/// | Return Value | When to Use |
/// |--------------|-------------|
/// | [`Required`](VersionRequirement::Required) | Build commands need version (e.g., `gulp --packageVersion`) |
/// | [`Embedded`](VersionRequirement::Embedded) | Version is in source; build ignores parameter |
/// | [`Optional`](VersionRequirement::Optional) | Version can override source or be auto-detected |
///
/// # Implementation Guide
///
/// ```ignore
/// impl Builder for MyPluginBuilder {
///     fn version_requirement(&self) -> VersionRequirement {
///         VersionRequirement::Embedded  // Most WordPress plugins
///     }
///     
///     fn detect_version(&self, working_dir: &Path) -> Result<Option<String>> {
///         detect_wordpress_plugin_version(&working_dir.join("my-plugin.php"))
///     }
///     
///     // ... other required methods
/// }
/// ```
pub trait Builder: Send + Sync {
    // =========================================================================
    // Version Handling
    // =========================================================================

    /// Returns how this builder handles version during the build process.
    ///
    /// # Return Values
    ///
    /// - [`Required`](VersionRequirement::Required): Version must be provided;
    ///   error if `None` is passed. Use when build commands need version.
    ///
    /// - [`Embedded`](VersionRequirement::Embedded): Version is in source files;
    ///   error if `Some(v)` is passed. Use when build ignores version parameter.
    ///
    /// - [`Optional`](VersionRequirement::Optional): Version can be provided to
    ///   override, or `None` to auto-detect. Most flexible option.
    ///
    /// # Default
    ///
    /// Returns [`Optional`](VersionRequirement::Optional) — the most flexible choice.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // BackWPup: build commands use version
    /// fn version_requirement(&self) -> VersionRequirement {
    ///     VersionRequirement::Required
    /// }
    ///
    /// // WP Rocket: version embedded in PHP header
    /// fn version_requirement(&self) -> VersionRequirement {
    ///     VersionRequirement::Embedded
    /// }
    /// ```
    fn version_requirement(&self) -> VersionRequirement {
        VersionRequirement::Optional
    }

    /// Attempt to detect the version from source files after checkout.
    ///
    /// Called when:
    /// - [`version_requirement`](Self::version_requirement) is [`Embedded`](VersionRequirement::Embedded)
    /// - [`version_requirement`](Self::version_requirement) is [`Optional`](VersionRequirement::Optional)
    ///   and no version was provided
    ///
    /// # Common Detection Strategies
    ///
    /// 1. **WordPress plugin header** (most common):
    ///    ```php
    ///    /**
    ///     * Plugin Name: My Plugin
    ///     * Version: 1.2.3
    ///     */
    ///    ```
    ///    Use [`detect_wordpress_plugin_version`] helper.
    ///
    /// 2. **readme.txt stable tag**:
    ///    ```txt
    ///    Stable tag: 1.2.3
    ///    ```
    ///    Use [`detect_wordpress_readme_version`] helper.
    ///
    /// 3. **package.json version**:
    ///    ```json
    ///    { "version": "1.2.3" }
    ///    ```
    ///
    /// # Arguments
    ///
    /// * `working_dir` - The repository root directory (after checkout)
    ///
    /// # Returns
    ///
    /// * `Ok(Some(version))` - Version successfully detected
    /// * `Ok(None)` - Detection not supported or version not found
    /// * `Err(_)` - I/O error or other failure
    ///
    /// # Default
    ///
    /// Returns `Ok(None)` — no auto-detection by default.
    fn detect_version(&self, _working_dir: &Path) -> Result<Option<String>> {
        Ok(None)
    }

    /// Repo-relative files that [`detect_version`](Self::detect_version) reads,
    /// so the version can be detected **before cloning** by fetching just these
    /// files at the target commit (see the build pipeline's pre-clone cache
    /// fast path).
    ///
    /// The returned paths MUST be exactly the files
    /// [`detect_version`](Self::detect_version) consults, so detecting from the
    /// fetched copies yields the same version as a full checkout would. A commit
    /// is immutable, so the version read this way is precisely what a build of
    /// that commit produces — no override and no race.
    ///
    /// # Default
    ///
    /// Returns an empty list, which **disables** pre-clone detection: the build
    /// resolves the version after checkout as usual. This is the correct default
    /// for builders whose version is supplied to the build
    /// ([`Required`](VersionRequirement::Required)) or that do not derive it from
    /// source. Builders whose version lives in a source file (WP Rocket,
    /// Imagify) return that file here.
    fn version_source_files(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Rewrite the version embedded in the checked-out source to `version`.
    ///
    /// Called after checkout — and only when the caller pinned an explicit
    /// version *and* the build path is taken (never when artifacts are served
    /// from the cache) — so a plugin whose version lives in its source can be
    /// made to produce an artifact at the requested version instead of the
    /// source's. It runs before the build packages the source, so the rewritten
    /// files flow into the produced artifact.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(override))` — the source was rewritten; the returned
    ///   [`VersionOverride`] is reported to the caller.
    /// - `Ok(None)` — nothing to do: this builder does not override its version,
    ///   or the pinned version already matched source.
    /// - `Err(_)` — the override was requested but could not be applied safely
    ///   (e.g. a required declaration was not found); the build fails rather
    ///   than deliver a mislabeled artifact.
    ///
    /// # Default
    ///
    /// Returns `Ok(None)` — no override. Builders whose version is provided to
    /// the build ([`Required`](VersionRequirement::Required), e.g. BackWPup) or
    /// that have nothing to rewrite keep this default. Builders whose version is
    /// in a WordPress plugin header (WP Rocket, Imagify) override it with
    /// [`rewrite_wordpress_plugin_version`].
    fn apply_version_override(
        &self,
        _working_dir: &Path,
        _version: &str,
    ) -> Result<Option<VersionOverride>> {
        Ok(None)
    }

    // =========================================================================
    // Commands and Setup
    // =========================================================================

    /// Verify this builder can run on the current platform.
    ///
    /// Called by the build pipeline **before** any network or git work (see
    /// [`BuildCommand::execute`](crate::commands::BuildCommand)), so a build on
    /// an unsupported host fails fast with a clear, actionable message instead
    /// of failing deep inside a build step (or not at all).
    ///
    /// # Default
    ///
    /// Cross-platform — returns `Ok(())`. Override for a builder whose build
    /// process depends on a toolchain unavailable on some platform (e.g. a
    /// Unix-only packaging script) and return [`Error::PlatformUnsupported`].
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn ensure_platform_supported(&self) -> Result<()> {
    ///     #[cfg(windows)]
    ///     {
    ///         Err(Error::PlatformUnsupported {
    ///             project: "my-plugin".to_string(),
    ///             platform: std::env::consts::OS.to_string(),
    ///             reason: "requires a Unix-like toolchain; use WSL".to_string(),
    ///         })
    ///     }
    ///     #[cfg(not(windows))]
    ///     {
    ///         Ok(())
    ///     }
    /// }
    /// ```
    ///
    /// [`Error::PlatformUnsupported`]: crate::error::Error::PlatformUnsupported
    fn ensure_platform_supported(&self) -> Result<()> {
        Ok(())
    }

    /// Get all tool dependencies for this builder.
    ///
    /// Returns a list of [`ToolDependency`] entries that describe what CLI
    /// tools are needed and how to handle missing ones. The build runner
    /// processes each entry according to its `required` and `install_commands`
    /// fields.
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn tool_dependencies(&self) -> Vec<ToolDependency> {
    ///     vec![
    ///         ToolDependency::required("npm"),
    ///         ToolDependency::required("composer"),
    ///         ToolDependency::required_with_install("gulp", vec!["npm install --global gulp-cli"]),
    ///     ]
    /// }
    /// ```
    fn tool_dependencies(&self) -> Vec<ToolDependency> {
        vec![]
    }

    /// Get setup commands (run once before building).
    ///
    /// These run after checkout but before build commands.
    /// Typically used for dependency installation.
    ///
    /// Each step has a human-readable label (shown to users) and the
    /// actual shell command (shown in verbose/debug mode).
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn setup_commands(&self) -> Vec<BuildStep> {
    ///     vec![
    ///         BuildStep::new("Installing PHP dependencies", "composer install --no-dev"),
    ///         BuildStep::new("Installing JS dependencies", "npm ci"),
    ///     ]
    /// }
    /// ```
    fn setup_commands(&self) -> Vec<BuildStep>;

    // =========================================================================
    // Build Hooks
    // =========================================================================

    /// Hook called before the build starts.
    ///
    /// Use this for pre-build cleanup or preparation. The reporter is
    /// provided so hooks can emit fine-grained progress for long operations.
    ///
    /// # Arguments
    ///
    /// * `context` - The build context with repo and workspace paths
    /// * `version` - The version being built
    /// * `variants` - The variants being built (empty = all)
    /// * `reporter` - Progress reporter for emitting step events
    fn pre_build_hook(
        &self,
        _context: &BuildContext,
        _version: &str,
        _variants: &[&str],
        _reporter: &dyn ProgressReporter,
    ) -> Result<()> {
        Ok(())
    }

    /// Hook called during the build process.
    ///
    /// Useful if the project doesn't use traditional build commands
    /// but needs custom logic (e.g., file manipulation).
    ///
    /// # Arguments
    ///
    /// * `context` - The build context with repo and workspace paths
    /// * `version` - The version being built
    /// * `variants` - The variants being built (empty = all)
    /// * `reporter` - Progress reporter for emitting step events
    fn build_hook(
        &self,
        _context: &BuildContext,
        _version: &str,
        _variants: &[&str],
        _reporter: &dyn ProgressReporter,
    ) -> Result<()> {
        Ok(())
    }

    /// Hook called after the build completes.
    ///
    /// Use this for post-build cleanup or artifact organization.
    ///
    /// # Arguments
    ///
    /// * `context` - The build context with repo and workspace paths
    /// * `version` - The version being built
    /// * `variants` - The variants being built (empty = all)
    /// * `reporter` - Progress reporter for emitting step events
    fn post_build_hook(
        &self,
        _context: &BuildContext,
        _version: &str,
        _variants: &[&str],
        _reporter: &dyn ProgressReporter,
    ) -> Result<()> {
        Ok(())
    }

    // =========================================================================
    // Variants
    // =========================================================================

    /// Get all available build variants.
    ///
    /// Returns empty Vec if project has no variants (single output).
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn variants(&self) -> Vec<BuildVariant> {
    ///     vec![
    ///         BuildVariant { id: "free", name: "Free", description: "Free version" },
    ///         BuildVariant { id: "pro", name: "Pro", description: "Pro version" },
    ///     ]
    /// }
    /// ```
    fn variants(&self) -> Vec<BuildVariant> {
        vec![]
    }

    /// Check if this project has multiple variants.
    fn has_variants(&self) -> bool {
        self.variants().len() > 1
    }

    // =========================================================================
    // Defaults (for CLI convenience)
    // =========================================================================

    /// Get the default version to use when none is provided.
    ///
    /// This is primarily used by CLI tools to provide sensible defaults.
    /// Returns `None` if the builder requires explicit version input.
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn default_version(&self) -> Option<&'static str> {
    ///     Some("9.99.99")  // Development version
    /// }
    /// ```
    fn default_version(&self) -> Option<&'static str> {
        None
    }

    /// Get the default variants to build when none are specified.
    ///
    /// Returns a subset of available variants that should be built by default.
    /// Returns empty Vec to build all variants by default.
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn default_variants(&self) -> Vec<&'static str> {
    ///     vec!["free", "pro-en"]  // Skip pro-de by default
    /// }
    /// ```
    fn default_variants(&self) -> Vec<&'static str> {
        vec![]
    }

    // =========================================================================
    // Build Execution
    // =========================================================================

    /// Get build commands with human-readable labels.
    ///
    /// If project has variants, only requested ones are built.
    /// If no variants, the `variants` parameter is ignored.
    ///
    /// Each step has a human-readable label (shown to users) and the
    /// actual shell command (shown in verbose/debug mode).
    ///
    /// # Arguments
    ///
    /// * `context` - The build context with repo and workspace paths
    /// * `version` - The version being built (may be empty if not required)
    /// * `variants` - The variants to build (empty = all)
    fn build_commands(
        &self,
        context: &BuildContext,
        version: &str,
        variants: &[&str],
    ) -> Vec<BuildStep>;

    /// Get the artifacts produced by the build.
    ///
    /// This method is called after the build completes. Builders should return
    /// resolved, concrete paths to the produced artifacts. If the builder needs
    /// to use glob patterns to locate files (e.g., when filenames contain commit
    /// hashes), it should resolve them internally before returning.
    ///
    /// # Arguments
    ///
    /// * `context` - The build context with repo and workspace paths
    /// * `version` - The version that was built
    /// * `variants` - The variants that were built (empty = all)
    ///
    /// # Returns
    ///
    /// A list of artifacts with resolved source paths.
    fn artifacts(
        &self,
        context: &BuildContext,
        version: &str,
        variants: &[&str],
    ) -> Result<Vec<BuildArtifact>>;

    /// Get the subdirectory where the build should run.
    ///
    /// Override this if the build commands need to run in a subdirectory
    /// of the repository root.
    fn build_subdirectory(&self) -> Option<&'static str> {
        None
    }

    /// Validate requested variants.
    fn validate_variants(&self, requested: &[&str]) -> Result<()> {
        if !self.has_variants() {
            // No variants = ignore the request, build everything
            return Ok(());
        }

        let available: Vec<&str> = self.variants().iter().map(|v| v.id).collect();
        for variant in requested {
            if !available.contains(variant) {
                return Err(Error::Build(format!(
                    "Unknown variant '{}'. Available: {}",
                    variant,
                    available.join(", ")
                )));
            }
        }
        Ok(())
    }

    // =========================================================================
    // Release Asset Matching
    // =========================================================================

    /// Check whether a release asset name belongs to this project.
    ///
    /// Used to filter release assets during `release:` downloads.
    /// Only assets that match are downloaded.
    ///
    /// # Arguments
    ///
    /// * `asset_name` - Filename of the release asset (e.g., `"backwpup-free-5.6.8.zip"`)
    ///
    /// # Default
    ///
    /// Returns `false` — override when the project has downloadable release assets.
    fn matches_release_asset(&self, _asset_name: &str) -> bool {
        false
    }

    /// Determine which variant a release asset belongs to.
    ///
    /// Called for assets that pass [`matches_release_asset`](Self::matches_release_asset).
    /// Returns `None` for single-variant projects.
    ///
    /// # Arguments
    ///
    /// * `asset_name` - Filename of the release asset
    fn variant_from_release_asset(&self, _asset_name: &str) -> Option<String> {
        None
    }
}

// =============================================================================
// Version Detection Helpers
// =============================================================================

/// Detect version from a WordPress plugin's main PHP file header.
///
/// WordPress plugins use a standardized header format:
/// ```php
/// /**
///  * Plugin Name: My Plugin
///  * Version: 1.2.3
///  * Description: ...
///  */
/// ```
///
/// This function parses the header and extracts the version.
///
/// # Arguments
///
/// * `php_file` - Path to the main plugin PHP file
///
/// # Returns
///
/// * `Ok(Some(version))` - Version found in header
/// * `Ok(None)` - No version header found
/// * `Err(_)` - File not found or I/O error
///
/// # Example
///
/// ```ignore
/// impl Builder for WpRocketBuilder {
///     fn detect_version(&self, working_dir: &Path) -> Result<Option<String>> {
///         detect_wordpress_plugin_version(&working_dir.join("wp-rocket.php"))
///     }
/// }
/// ```
///
/// # References
///
/// - [WordPress Plugin Header Requirements](https://developer.wordpress.org/plugins/plugin-basics/header-requirements/)
pub fn detect_wordpress_plugin_version(php_file: &Path) -> Result<Option<String>> {
    use std::fs;
    use std::io::ErrorKind;

    // Read the file (WordPress checks first 8KB)
    let content = match fs::read_to_string(php_file) {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            tracing::debug!("Plugin file not found: {}", php_file.display());
            return Ok(None);
        }
        Err(e) => {
            return Err(Error::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to read plugin file '{}': {}", php_file.display(), e),
            )));
        }
    };

    // WordPress only checks first 8KB for headers
    let header_section = if content.len() > 8192 {
        &content[..8192]
    } else {
        &content
    };

    // Look for "Version:" line (case-insensitive, allows various spacing)
    // Matches: "Version: 1.2.3", "Version:1.2.3", " * Version: 1.2.3"
    for line in header_section.lines() {
        let line = line.trim();

        // Skip if line doesn't contain "version" (case-insensitive quick check)
        if !line.to_lowercase().contains("version") {
            continue;
        }

        // Try to extract version after "Version:"
        // Handle both "Version: X.Y.Z" and "* Version: X.Y.Z" formats
        if let Some(rest) = line
            .strip_prefix("*")
            .map(|s| s.trim())
            .unwrap_or(line)
            .strip_prefix("Version:")
            .or_else(|| line.strip_prefix("version:"))
        {
            let version = rest.trim();
            if !version.is_empty() && is_valid_version(version) {
                tracing::debug!("Detected version '{}' from {}", version, php_file.display());
                return Ok(Some(version.to_string()));
            }
        }
    }

    tracing::debug!("No version found in {}", php_file.display());
    Ok(None)
}

/// Check if a string looks like a valid semantic version.
///
/// Accepts formats like:
/// - `1.2.3`
/// - `1.2.3-beta`
/// - `1.2.3-beta.1`
/// - `1.2`
fn is_valid_version(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }

    // Must start with a digit
    if !s
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        return false;
    }

    // Allow digits, dots, hyphens, and alphanumerics
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+')
}

/// Rewrite the version embedded in a WordPress plugin's main PHP file.
///
/// Overrides the version so a produced artifact carries a caller-supplied
/// version instead of the one hard-coded in source. It rewrites:
///
/// 1. the plugin header `Version:` line in the first 8KB (always required — the
///    canonical WordPress version, mirroring [`detect_wordpress_plugin_version`]
///    so detection and rewrite always agree on which line), and
/// 2. the runtime version constant named by `version_constant` (e.g.
///    `WP_ROCKET_VERSION`), matched by its **exact quoted name** so a sibling
///    such as `WP_ROCKET_LASTVERSION` is never touched. Pass `None` to rewrite
///    the header only.
///
/// # Returns
///
/// - `Ok(Some(override))` — at least one declaration was rewritten.
/// - `Ok(None)` — every targeted declaration already held `new_version`.
/// - `Err(_)` — `new_version` is not a valid version, the file could not be
///   read/written, or a required declaration (the header, or the requested
///   constant) was not found. Failing is deliberate: shipping an artifact whose
///   version is only partially overridden would mislabel the plugin.
///
/// [WordPress header spec](https://developer.wordpress.org/plugins/plugin-basics/header-requirements/)
pub fn rewrite_wordpress_plugin_version(
    php_file: &Path,
    new_version: &str,
    version_constant: Option<&str>,
) -> Result<Option<VersionOverride>> {
    if !is_valid_version(new_version) {
        return Err(Error::Build(format!(
            "Refusing to override '{}' to invalid version '{new_version}'",
            php_file.display()
        )));
    }

    let content = std::fs::read_to_string(php_file).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!(
                "Failed to read plugin file '{}' for version override: {e}",
                php_file.display()
            ),
        ))
    })?;

    // (byte span in `content`, current value, human-readable label) per site.
    let mut edits: Vec<(std::ops::Range<usize>, String, &str)> = Vec::new();

    // 1. Header `Version:` line — always required.
    match find_header_version_span(&content) {
        Some((span, current)) => edits.push((span, current, "Version: header")),
        None => {
            return Err(Error::Build(format!(
                "Could not find a 'Version:' plugin header in '{}' to override to '{new_version}'",
                php_file.display()
            )));
        }
    }

    // 2. Runtime version constant — required when a name is configured.
    if let Some(name) = version_constant {
        match find_constant_version_span(&content, name) {
            Some((span, current)) => edits.push((span, current, name)),
            None => {
                return Err(Error::Build(format!(
                    "Could not find version constant '{name}' in '{}' to override to '{new_version}'",
                    php_file.display()
                )));
            }
        }
    }

    // The stale declarations are exactly those to rewrite and report; if none
    // are stale, every declaration already holds the requested version.
    let sites: Vec<String> = edits
        .iter()
        .filter(|(_, current, _)| current != new_version)
        .map(|(_, _, label)| (*label).to_string())
        .collect();
    if sites.is_empty() {
        return Ok(None);
    }
    // Report the first stale declaration's value as the "from" version (in the
    // normal case the header and constant agree, so this is simply the source
    // version).
    let from = edits
        .iter()
        .find(|(_, current, _)| current != new_version)
        .map(|(_, current, _)| current.clone())
        .unwrap_or_else(|| edits[0].1.clone());

    // Apply replacements right-to-left so earlier byte offsets stay valid.
    let mut rewritten = content.clone();
    let mut ordered: Vec<&(std::ops::Range<usize>, String, &str)> = edits.iter().collect();
    ordered.sort_by_key(|edit| std::cmp::Reverse(edit.0.start));
    for (span, current, _) in ordered {
        if current != new_version {
            rewritten.replace_range(span.clone(), new_version);
        }
    }

    std::fs::write(php_file, rewritten).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!(
                "Failed to write version override to '{}': {e}",
                php_file.display()
            ),
        ))
    })?;

    let file = php_file
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| php_file.display().to_string());

    Ok(Some(VersionOverride {
        file,
        from,
        to: new_version.to_string(),
        sites,
    }))
}

/// Find the byte span (within `content`) of the version value on the plugin
/// header `Version:` line, plus that current value.
///
/// Restricted to the first 8KB (WordPress only reads that far). Iterates whole
/// lines to stay UTF-8-safe rather than slicing at byte 8192.
fn find_header_version_span(content: &str) -> Option<(std::ops::Range<usize>, String)> {
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        if offset >= 8192 {
            break;
        }
        let line_start = offset;
        offset += line.len();
        let logical = line.trim_end_matches(['\n', '\r']);
        if let Some((value_offset, value)) = header_version_value(logical) {
            let start = line_start + value_offset;
            return Some((start..start + value.len(), value.to_string()));
        }
    }
    None
}

/// If `line` is a WordPress plugin header `Version:` line, return the byte
/// offset (within `line`) of the version value and the value itself.
///
/// Accepts an optional doc-comment prefix of whitespace / `*` / `/` before the
/// key (`" * Version: 1.2.3"`, `"Version:1.2.3"`), matches the `Version:` key
/// case-insensitively, and requires the value to look like a version — so
/// unrelated header lines (`Requires PHP:`, `Stable tag:`) are ignored.
fn header_version_value(line: &str) -> Option<(usize, &str)> {
    const KEY: &str = "version:";
    let key_pos = find_ascii_ci(line, KEY)?;
    // Only a header/comment prefix may precede the key.
    if !line[..key_pos]
        .chars()
        .all(|c| c.is_whitespace() || c == '*' || c == '/')
    {
        return None;
    }
    let after = &line[key_pos + KEY.len()..];
    let leading_ws = after.len() - after.trim_start().len();
    let value_start = key_pos + KEY.len() + leading_ws;
    let value = value_token(&line[value_start..])?;
    is_valid_version(value).then_some((value_start, value))
}

/// Find the byte span (within `content`) of the version literal assigned to the
/// PHP constant named `const_name`, plus its current value.
///
/// Matches the constant by its **exact quoted name** (`'WP_ROCKET_VERSION'` or
/// `"WP_ROCKET_VERSION"`), so a sibling whose name merely contains it (e.g.
/// `WP_ROCKET_LASTVERSION`) is never matched, then takes the next single- or
/// double-quoted string literal as the value and requires it to look like a
/// version. Scans occurrences in order, returning the first followed by a
/// version literal so stray mentions are skipped.
fn find_constant_version_span(
    content: &str,
    const_name: &str,
) -> Option<(std::ops::Range<usize>, String)> {
    for quote in ['\'', '"'] {
        let needle = format!("{quote}{const_name}{quote}");
        let mut search_from = 0usize;
        while let Some(rel) = content[search_from..].find(&needle) {
            let name_end = search_from + rel + needle.len();
            if let Some(found) = version_literal_after(content, name_end) {
                return Some(found);
            }
            search_from = name_end;
        }
    }
    None
}

/// Starting at byte `from`, skip whitespace and a single `,`, then read a
/// single- or double-quoted string literal and return its span (the value
/// between the quotes) and content — but only if it looks like a version.
fn version_literal_after(content: &str, from: usize) -> Option<(std::ops::Range<usize>, String)> {
    let rest = &content[from..];
    let trimmed = rest.trim_start_matches([' ', '\t', '\r', '\n', ',']);
    let skipped = rest.len() - trimmed.len();
    let quote = trimmed.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let after_quote = &trimmed[quote.len_utf8()..];
    let close = after_quote.find(quote)?;
    let value = &after_quote[..close];
    if !is_valid_version(value) {
        return None;
    }
    let start = from + skipped + quote.len_utf8();
    Some((start..start + value.len(), value.to_string()))
}

/// The leading token of `s` up to the first whitespace, or `None` if empty.
fn value_token(s: &str) -> Option<&str> {
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    (end > 0).then_some(&s[..end])
}

/// First byte index of the ASCII `needle` (given lowercase) in `haystack`,
/// case-insensitively. Byte indices are valid in `haystack` because ASCII
/// lowercasing preserves byte length.
fn find_ascii_ci(haystack: &str, needle_lower: &str) -> Option<usize> {
    haystack.to_ascii_lowercase().find(needle_lower)
}

/// Detect version from a readme.txt "Stable tag" header.
///
/// WordPress.org uses this format:
/// ```txt
/// === My Plugin ===
/// Stable tag: 1.2.3
/// ```
///
/// # Arguments
///
/// * `readme_file` - Path to readme.txt
///
/// # Returns
///
/// * `Ok(Some(version))` - Version found
/// * `Ok(None)` - No stable tag found
/// * `Err(_)` - I/O error
pub fn detect_wordpress_readme_version(readme_file: &Path) -> Result<Option<String>> {
    use std::fs;
    use std::io::ErrorKind;

    let content = match fs::read_to_string(readme_file) {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "Failed to read readme file '{}': {}",
                    readme_file.display(),
                    e
                ),
            )));
        }
    };

    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line
            .strip_prefix("Stable tag:")
            .or_else(|| line.strip_prefix("stable tag:"))
        {
            let version = rest.trim();
            if !version.is_empty() && is_valid_version(version) {
                return Ok(Some(version.to_string()));
            }
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    // =========================================================================
    // 5.1–5.6 – VersionRequirement
    // =========================================================================

    #[test]
    fn test_is_valid_version() {
        assert!(is_valid_version("1.0.0"));
        assert!(is_valid_version("1.0"));
        assert!(is_valid_version("1.0.0-beta"));
        assert!(is_valid_version("1.0.0-beta.1"));
        assert!(is_valid_version("1.0.0+build123"));

        assert!(!is_valid_version(""));
        assert!(!is_valid_version("   "));
        assert!(!is_valid_version("vX.Y"));
        assert!(!is_valid_version("latest"));
    }

    #[test]
    fn test_version_requirement_is_required() {
        assert!(VersionRequirement::Required.is_required());
        assert!(!VersionRequirement::Embedded.is_required());
        assert!(!VersionRequirement::Optional.is_required());
    }

    #[test]
    fn test_version_requirement_is_embedded() {
        assert!(VersionRequirement::Embedded.is_embedded());
        assert!(!VersionRequirement::Required.is_embedded());
        assert!(!VersionRequirement::Optional.is_embedded());
    }

    #[test]
    fn test_version_requirement_is_optional() {
        assert!(VersionRequirement::Optional.is_optional());
        assert!(!VersionRequirement::Required.is_optional());
        assert!(!VersionRequirement::Embedded.is_optional());
    }

    #[test]
    fn test_version_requirement_description() {
        assert!(!VersionRequirement::Required.description().is_empty());
        assert!(!VersionRequirement::Embedded.description().is_empty());
        assert!(!VersionRequirement::Optional.description().is_empty());
    }

    #[test]
    fn test_version_requirement_display() {
        assert_eq!(format!("{}", VersionRequirement::Required), "Required");
        assert_eq!(format!("{}", VersionRequirement::Embedded), "Embedded");
        assert_eq!(format!("{}", VersionRequirement::Optional), "Optional");
    }

    #[test]
    fn test_version_requirement_default() {
        assert_eq!(VersionRequirement::default(), VersionRequirement::Optional);
    }

    // =========================================================================
    // 5.7–5.10 – ToolDependency
    // =========================================================================

    #[test]
    fn test_tool_dependency_required() {
        let dep = ToolDependency::required("npm");
        assert_eq!(dep.name, "npm");
        assert!(dep.required);
        assert!(dep.install_commands.is_empty());
    }

    #[test]
    fn test_tool_dependency_required_with_install() {
        let dep = ToolDependency::required_with_install("gulp", vec!["npm install -g gulp-cli"]);
        assert_eq!(dep.name, "gulp");
        assert!(dep.required);
        assert_eq!(dep.install_commands.len(), 1);
        assert!(dep.install_commands[0].contains("gulp-cli"));
    }

    #[test]
    fn test_tool_dependency_optional() {
        let dep = ToolDependency::optional("rsync");
        assert_eq!(dep.name, "rsync");
        assert!(!dep.required);
        assert!(dep.install_commands.is_empty());
    }

    #[test]
    fn test_tool_dependency_optional_with_install() {
        let dep = ToolDependency::optional_with_install("jq", vec!["apt-get install jq"]);
        assert_eq!(dep.name, "jq");
        assert!(!dep.required);
        assert_eq!(dep.install_commands.len(), 1);
    }

    // =========================================================================
    // 5.11–5.16 – detect_wordpress_plugin_version
    // =========================================================================

    #[test]
    fn test_detect_wordpress_plugin_version() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("plugin.php");
        let mut f = std::fs::File::create(&file).unwrap();
        writeln!(f, "<?php").unwrap();
        writeln!(f, "/**").unwrap();
        writeln!(f, " * Plugin Name: My Plugin").unwrap();
        writeln!(f, " * Version: 2.3.4").unwrap();
        writeln!(f, " */").unwrap();

        let version = detect_wordpress_plugin_version(&file).unwrap();
        assert_eq!(version, Some("2.3.4".to_string()));
    }

    #[test]
    fn test_detect_wordpress_plugin_version_beta() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("plugin.php");
        let mut f = std::fs::File::create(&file).unwrap();
        writeln!(f, "<?php").unwrap();
        writeln!(f, " * Version: 1.0.0-beta").unwrap();

        let version = detect_wordpress_plugin_version(&file).unwrap();
        assert_eq!(version, Some("1.0.0-beta".to_string()));
    }

    #[test]
    fn test_detect_wordpress_plugin_version_with_star_prefix() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("plugin.php");
        let mut f = std::fs::File::create(&file).unwrap();
        writeln!(f, "<?php").unwrap();
        writeln!(f, "/**").unwrap();
        writeln!(f, " * Plugin Name: WP Rocket").unwrap();
        writeln!(f, " * Version: 3.17.4").unwrap();
        writeln!(f, " */").unwrap();

        let version = detect_wordpress_plugin_version(&file).unwrap();
        assert_eq!(version, Some("3.17.4".to_string()));
    }

    #[test]
    fn test_detect_wordpress_plugin_version_no_header() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("plugin.php");
        std::fs::write(&file, "<?php\n// No header here\n").unwrap();

        let version = detect_wordpress_plugin_version(&file).unwrap();
        assert_eq!(version, None);
    }

    #[test]
    fn test_detect_wordpress_plugin_version_missing_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("nonexistent.php");

        let version = detect_wordpress_plugin_version(&file).unwrap();
        assert_eq!(version, None);
    }

    #[test]
    fn test_detect_wordpress_plugin_version_over_8kb() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("big.php");

        // Write version AFTER 8KB — should NOT be found
        let mut content = "<?php\n".to_string();
        // Fill with 8200 bytes of comments
        while content.len() < 8200 {
            content.push_str("// padding line\n");
        }
        content.push_str(" * Version: 9.9.9\n");
        std::fs::write(&file, &content).unwrap();

        let version = detect_wordpress_plugin_version(&file).unwrap();
        assert_eq!(version, None);
    }

    // =========================================================================
    // 5.17–5.19 – detect_wordpress_readme_version
    // =========================================================================

    #[test]
    fn test_detect_wordpress_readme_version() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("readme.txt");
        std::fs::write(&file, "=== My Plugin ===\nStable tag: 1.2.3\n").unwrap();

        let version = detect_wordpress_readme_version(&file).unwrap();
        assert_eq!(version, Some("1.2.3".to_string()));
    }

    #[test]
    fn test_detect_wordpress_readme_version_not_found() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("readme.txt");
        std::fs::write(&file, "=== My Plugin ===\nNo version here\n").unwrap();

        let version = detect_wordpress_readme_version(&file).unwrap();
        assert_eq!(version, None);
    }

    #[test]
    fn test_detect_wordpress_readme_version_missing_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("nonexistent.txt");

        let version = detect_wordpress_readme_version(&file).unwrap();
        assert_eq!(version, None);
    }

    // =========================================================================
    // 5.20–5.22 – validate_variants
    // =========================================================================

    #[test]
    fn test_validate_variants_valid() {
        let b = BackWPupBuilder;
        assert!(b.validate_variants(&["free", "pro-en"]).is_ok());
    }

    #[test]
    fn test_validate_variants_invalid() {
        let b = BackWPupBuilder;
        let result = b.validate_variants(&["nonexistent"]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unknown variant"));
        assert!(err.contains("nonexistent"));
    }

    #[test]
    fn test_validate_variants_no_variants_builder() {
        // WP Rocket has no variants — any input should be accepted
        let b = WpRocketBuilder;
        assert!(b.validate_variants(&["anything"]).is_ok());
        assert!(b.validate_variants(&[]).is_ok());
    }

    // =========================================================================
    // ensure_platform_supported (trait default)
    // =========================================================================

    #[test]
    fn test_default_ensure_platform_supported_is_ok() {
        // Builders that do not override the default are cross-platform, so the
        // gate is a no-op on every platform.
        assert!(WpRocketBuilder.ensure_platform_supported().is_ok());
        assert!(BackWPupBuilder.ensure_platform_supported().is_ok());
    }

    // =========================================================================
    // rewrite_wordpress_plugin_version
    // =========================================================================

    /// A realistic WP-Rocket-shaped main file: doc-comment header plus both the
    /// runtime `WP_ROCKET_VERSION` constant and the unrelated
    /// `WP_ROCKET_LASTVERSION` that must never be touched.
    fn wpr_like_source(version: &str, last_version: &str) -> String {
        format!(
            "<?php\n\
             /**\n\
             \x20* Plugin Name: WP Rocket\n\
             \x20* Version: {version}\n\
             \x20* Requires PHP: 7.3\n\
             \x20*/\n\
             defined( 'ABSPATH' ) || exit;\n\
             define( 'WP_ROCKET_VERSION', '{version}' );\n\
             define( 'WP_ROCKET_LASTVERSION', '{last_version}' );\n"
        )
    }

    #[test]
    fn rewrite_updates_header_and_constant_but_not_lastversion() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("wp-rocket.php");
        std::fs::write(&file, wpr_like_source("3.23", "3.22.1")).unwrap();

        let result = rewrite_wordpress_plugin_version(&file, "3.99.0", Some("WP_ROCKET_VERSION"))
            .unwrap()
            .expect("an override should have been applied");

        assert_eq!(result.file, "wp-rocket.php");
        assert_eq!(result.from, "3.23");
        assert_eq!(result.to, "3.99.0");
        assert_eq!(
            result.sites,
            vec![
                "Version: header".to_string(),
                "WP_ROCKET_VERSION".to_string()
            ]
        );

        let rewritten = std::fs::read_to_string(&file).unwrap();
        assert!(rewritten.contains(" * Version: 3.99.0"));
        assert!(rewritten.contains("define( 'WP_ROCKET_VERSION', '3.99.0' );"));
        // The sibling constant is preserved exactly.
        assert!(
            rewritten.contains("define( 'WP_ROCKET_LASTVERSION', '3.22.1' );"),
            "WP_ROCKET_LASTVERSION must not be rewritten"
        );
        assert!(!rewritten.contains("3.23"), "no old version should remain");

        // Detection now agrees with the rewrite.
        assert_eq!(
            detect_wordpress_plugin_version(&file).unwrap(),
            Some("3.99.0".to_string())
        );
    }

    #[test]
    fn rewrite_is_noop_when_version_already_matches() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("wp-rocket.php");
        let original = wpr_like_source("3.23", "3.22.1");
        std::fs::write(&file, &original).unwrap();

        let result =
            rewrite_wordpress_plugin_version(&file, "3.23", Some("WP_ROCKET_VERSION")).unwrap();
        assert!(result.is_none(), "matching version must be a no-op");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            original,
            "a no-op must not modify the file"
        );
    }

    #[test]
    fn rewrite_partial_noop_updates_only_the_stale_site() {
        // Header already at target, constant stale → only the constant changes,
        // and only it is reported as a rewritten site.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("wp-rocket.php");
        std::fs::write(
            &file,
            "<?php\n * Version: 3.99.0\ndefine( 'WP_ROCKET_VERSION', '3.23' );\n",
        )
        .unwrap();

        let result = rewrite_wordpress_plugin_version(&file, "3.99.0", Some("WP_ROCKET_VERSION"))
            .unwrap()
            .expect("the constant still needed rewriting");
        assert_eq!(result.sites, vec!["WP_ROCKET_VERSION".to_string()]);
        // `from` reflects the stale declaration that actually changed, not the
        // header that already matched.
        assert_eq!(result.from, "3.23");
        assert_eq!(result.to, "3.99.0");
        let rewritten = std::fs::read_to_string(&file).unwrap();
        assert!(rewritten.contains("define( 'WP_ROCKET_VERSION', '3.99.0' );"));
    }

    #[test]
    fn rewrite_header_only_when_no_constant_requested() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("imagify.php");
        std::fs::write(&file, "<?php\n * Version: 2.3.0\n").unwrap();

        let result = rewrite_wordpress_plugin_version(&file, "2.9.9", None)
            .unwrap()
            .expect("header override applied");
        assert_eq!(result.sites, vec!["Version: header".to_string()]);
        assert_eq!(result.from, "2.3.0");
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains(" * Version: 2.9.9")
        );
    }

    #[test]
    fn rewrite_preserves_crlf_and_only_touches_the_version() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("imagify.php");
        std::fs::write(
            &file,
            "<?php\r\n * Version: 2.3.0\r\ndefine('IMAGIFY_VERSION','2.3.0');\r\n",
        )
        .unwrap();

        rewrite_wordpress_plugin_version(&file, "2.9.9", Some("IMAGIFY_VERSION"))
            .unwrap()
            .expect("override applied");

        let rewritten = std::fs::read_to_string(&file).unwrap();
        assert!(
            rewritten.contains(" * Version: 2.9.9\r\n"),
            "CRLF preserved"
        );
        assert!(rewritten.contains("define('IMAGIFY_VERSION','2.9.9');"));
    }

    #[test]
    fn rewrite_errors_when_header_missing() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("wp-rocket.php");
        std::fs::write(&file, "<?php\n// no plugin header here\n").unwrap();

        let err = rewrite_wordpress_plugin_version(&file, "3.99.0", None).unwrap_err();
        assert!(err.to_string().contains("Version:"));
    }

    #[test]
    fn rewrite_errors_when_constant_missing() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("wp-rocket.php");
        std::fs::write(&file, "<?php\n * Version: 3.23\n").unwrap();

        let err = rewrite_wordpress_plugin_version(&file, "3.99.0", Some("WP_ROCKET_VERSION"))
            .unwrap_err();
        assert!(err.to_string().contains("WP_ROCKET_VERSION"));
    }

    #[test]
    fn rewrite_errors_on_missing_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("nonexistent.php");
        assert!(rewrite_wordpress_plugin_version(&file, "3.99.0", None).is_err());
    }

    #[test]
    fn rewrite_errors_on_invalid_new_version() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("wp-rocket.php");
        std::fs::write(&file, "<?php\n * Version: 3.23\n").unwrap();
        assert!(rewrite_wordpress_plugin_version(&file, "not-a-version", None).is_err());
    }

    #[test]
    fn rewrite_ignores_header_beyond_8kb() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("wp-rocket.php");
        let mut content = "<?php\n".to_string();
        while content.len() < 8200 {
            content.push_str("// padding line\n");
        }
        content.push_str(" * Version: 9.9.9\n");
        std::fs::write(&file, &content).unwrap();

        // The only header sits past 8KB, so it is not found → error.
        assert!(rewrite_wordpress_plugin_version(&file, "1.0.0", None).is_err());
    }

    #[test]
    fn rewrite_handles_beta_versions() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("wp-rocket.php");
        std::fs::write(
            &file,
            "<?php\n * Version: 3.23\ndefine( 'WP_ROCKET_VERSION', '3.23' );\n",
        )
        .unwrap();

        rewrite_wordpress_plugin_version(&file, "4.0.0-beta1", Some("WP_ROCKET_VERSION"))
            .unwrap()
            .expect("override applied");
        let rewritten = std::fs::read_to_string(&file).unwrap();
        assert!(rewritten.contains(" * Version: 4.0.0-beta1"));
        assert!(rewritten.contains("define( 'WP_ROCKET_VERSION', '4.0.0-beta1' );"));
    }

    #[test]
    fn rewrite_does_not_touch_unrelated_version_lines() {
        // A "Requires at least" / "Stable tag" style neighbor and a value that
        // is not the constant must be left untouched.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("imagify.php");
        std::fs::write(
            &file,
            "<?php\n\
             \x20* Version: 2.3.0\n\
             \x20* Requires at least: 5.3\n\
             \x20* Stable tag: 2.3.0\n\
             define( 'IMAGIFY_VERSION', '2.3.0' );\n",
        )
        .unwrap();

        rewrite_wordpress_plugin_version(&file, "2.9.9", Some("IMAGIFY_VERSION"))
            .unwrap()
            .expect("override applied");
        let rewritten = std::fs::read_to_string(&file).unwrap();
        assert!(rewritten.contains(" * Version: 2.9.9"));
        assert!(rewritten.contains("define( 'IMAGIFY_VERSION', '2.9.9' );"));
        // Non-version header fields are preserved verbatim.
        assert!(rewritten.contains(" * Requires at least: 5.3"));
        assert!(
            rewritten.contains(" * Stable tag: 2.3.0"),
            "Stable tag is not the plugin Version: header and must be untouched"
        );
    }

    #[test]
    fn rewrite_leaves_wp_rocket_sibling_constants_untouched() {
        // Mirrors the real wp-rocket.php shape: WP_ROCKET_VERSION first appears
        // inside an `if ( ! defined(...) )` guard *before* its define(), next to
        // several sibling *VERSION constants (last/WP/PHP) that must never move.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("wp-rocket.php");
        let source = "<?php\n\
             /**\n\
             \x20* Plugin Name: WP Rocket\n\
             \x20* Version: 3.23\n\
             \x20*/\n\
             if ( ! defined( 'WP_ROCKET_VERSION' ) ) {\n\
             \tdefine( 'WP_ROCKET_VERSION', '3.23' );\n\
             }\n\
             if ( ! defined( 'WP_ROCKET_LASTVERSION' ) ) {\n\
             \tdefine( 'WP_ROCKET_LASTVERSION', '3.22.1' );\n\
             }\n\
             define( 'WP_ROCKET_WP_VERSION', '5.8' );\n\
             define( 'WP_ROCKET_WP_VERSION_TESTED', '6.3.1' );\n\
             define( 'WP_ROCKET_PHP_VERSION', '7.4' );\n";
        std::fs::write(&file, source).unwrap();

        let applied = rewrite_wordpress_plugin_version(&file, "3.99.0", Some("WP_ROCKET_VERSION"))
            .unwrap()
            .expect("override applied");
        assert_eq!(applied.from, "3.23");
        assert_eq!(
            applied.sites,
            vec![
                "Version: header".to_string(),
                "WP_ROCKET_VERSION".to_string()
            ]
        );

        let out = std::fs::read_to_string(&file).unwrap();
        // Header + the guarded WP_ROCKET_VERSION define are updated...
        assert!(out.contains(" * Version: 3.99.0"));
        assert!(out.contains("define( 'WP_ROCKET_VERSION', '3.99.0' );"));
        // ...the guard keyword still references the constant by name...
        assert!(out.contains("if ( ! defined( 'WP_ROCKET_VERSION' ) )"));
        // ...and every sibling constant is preserved byte-for-byte.
        assert!(out.contains("define( 'WP_ROCKET_LASTVERSION', '3.22.1' );"));
        assert!(out.contains("define( 'WP_ROCKET_WP_VERSION', '5.8' );"));
        assert!(out.contains("define( 'WP_ROCKET_WP_VERSION_TESTED', '6.3.1' );"));
        assert!(out.contains("define( 'WP_ROCKET_PHP_VERSION', '7.4' );"));
        // Only the plugin version moved off 3.23 (siblings are 3.22.1/5.8/6.3.1/7.4).
        assert!(!out.contains("3.23"), "no old plugin version should remain");
    }

    #[test]
    fn rewrite_handles_double_quoted_name_and_value() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("imagify.php");
        std::fs::write(
            &file,
            "<?php\n * Version: 2.3.0\ndefine( \"IMAGIFY_VERSION\", \"2.3.0\" );\n",
        )
        .unwrap();
        rewrite_wordpress_plugin_version(&file, "2.9.9", Some("IMAGIFY_VERSION"))
            .unwrap()
            .expect("override applied");
        let out = std::fs::read_to_string(&file).unwrap();
        assert!(out.contains("define( \"IMAGIFY_VERSION\", \"2.9.9\" );"));
    }

    #[test]
    fn rewrite_skips_quoted_mentions_not_followed_by_a_version() {
        // A quoted use of the constant name that is *not* a `define` assignment
        // (here an array key) must be skipped in favor of the real define.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("plugin.php");
        std::fs::write(
            &file,
            "<?php\n * Version: 1.0.0\n$keys = array( 'MY_VERSION' => true );\ndefine( 'MY_VERSION', '1.0.0' );\n",
        )
        .unwrap();
        rewrite_wordpress_plugin_version(&file, "2.0.0", Some("MY_VERSION"))
            .unwrap()
            .expect("override applied");
        let out = std::fs::read_to_string(&file).unwrap();
        assert!(
            out.contains("$keys = array( 'MY_VERSION' => true );"),
            "a non-assignment mention must be left untouched"
        );
        assert!(out.contains("define( 'MY_VERSION', '2.0.0' );"));
    }

    #[test]
    fn rewrite_errors_when_constant_value_is_not_a_literal() {
        // The constant is assigned another constant, not a quoted literal — the
        // version cannot be applied, so the override must fail loudly.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("plugin.php");
        std::fs::write(
            &file,
            "<?php\n * Version: 1.0.0\ndefine( 'MY_VERSION', OTHER_CONST );\n",
        )
        .unwrap();
        assert!(rewrite_wordpress_plugin_version(&file, "2.0.0", Some("MY_VERSION")).is_err());
    }

    #[test]
    fn default_apply_version_override_returns_none() {
        // A builder that does not override its version (e.g. BackWPup, whose
        // version is injected by the build) keeps the trait default: a no-op.
        let dir = TempDir::new().unwrap();
        assert!(
            BackWPupBuilder
                .apply_version_override(dir.path(), "5.0.0")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rewrite_is_utf8_safe_with_multibyte_source() {
        // Multi-byte characters before the declarations shift byte offsets; the
        // rewrite must locate and splice correctly without panicking.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("imagify.php");
        std::fs::write(
            &file,
            "<?php\n/**\n * Plugin Name: Imagify — Optimización\n * Version: 2.3.0\n */\n\
             define( 'IMAGIFY_VERSION', '2.3.0' );\n",
        )
        .unwrap();

        rewrite_wordpress_plugin_version(&file, "2.9.9", Some("IMAGIFY_VERSION"))
            .unwrap()
            .expect("override applied");
        let out = std::fs::read_to_string(&file).unwrap();
        assert!(out.contains(" * Version: 2.9.9"));
        assert!(out.contains("define( 'IMAGIFY_VERSION', '2.9.9' );"));
        // The multi-byte header text survives intact.
        assert!(out.contains("Imagify — Optimización"));
    }

    #[test]
    fn rewrite_rejects_version_that_could_inject_php() {
        // A version containing a quote/paren/semicolon would break out of the
        // PHP string literal; `is_valid_version` rejects it before any write, so
        // the source is left untouched.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("wp-rocket.php");
        let original = "<?php\n * Version: 3.23\ndefine( 'WP_ROCKET_VERSION', '3.23' );\n";
        std::fs::write(&file, original).unwrap();

        assert!(
            rewrite_wordpress_plugin_version(&file, "3.0'); evil('", Some("WP_ROCKET_VERSION"))
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            original,
            "a rejected version must not modify the file"
        );
    }
}
