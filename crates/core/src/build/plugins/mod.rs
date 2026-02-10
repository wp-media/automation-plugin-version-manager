//! Project-specific builders.
//!
//! This module defines the [`Builder`] trait and project implementations.
//! See [`VersionRequirement`] for how different projects handle versions.

mod bwu;

use std::path::{Path, PathBuf};

pub use bwu::BackWPupBuilder;

use crate::Result;
use crate::error::Error;

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

/// An optional command that can be installed if missing.
pub struct OptionalCommand {
    /// Command name to check.
    pub name: &'static str,
    /// Installation command if missing.
    pub install_cmd: &'static str,
}

/// Describes a build artifact (output file).
#[derive(Debug, Clone)]
pub struct BuildArtifact {
    /// Variant ID this artifact belongs to (None if single-variant project).
    pub variant_id: Option<String>,
    /// Source path (relative to build dir).
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

    // =========================================================================
    // Commands and Setup
    // =========================================================================

    /// Get required system commands.
    ///
    /// These commands must be available in PATH before the build starts.
    /// The build will fail immediately if any are missing.
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn required_commands(&self) -> Vec<&'static str> {
    ///     vec!["npm", "composer", "php"]
    /// }
    /// ```
    fn required_commands(&self) -> Vec<&'static str>;

    /// Get optional commands that will be installed if missing.
    ///
    /// Unlike required commands, these will be automatically installed
    /// using the provided installation command.
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn optional_commands(&self) -> Vec<OptionalCommand> {
    ///     vec![OptionalCommand {
    ///         name: "gulp",
    ///         install_cmd: "npm install --global gulp-cli",
    ///     }]
    /// }
    /// ```
    fn optional_commands(&self) -> Vec<OptionalCommand> {
        vec![]
    }

    /// Get setup commands (run once before building).
    ///
    /// These run after checkout but before build commands.
    /// Typically used for dependency installation.
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn setup_commands(&self) -> Vec<String> {
    ///     vec![
    ///         "composer install --no-dev".to_string(),
    ///         "npm ci".to_string(),
    ///     ]
    /// }
    /// ```
    fn setup_commands(&self) -> Vec<String>;

    // =========================================================================
    // Build Hooks
    // =========================================================================

    /// Hook called before the build starts.
    ///
    /// Use this for pre-build cleanup or preparation.
    ///
    /// # Arguments
    ///
    /// * `working_dir` - The build directory
    /// * `version` - The version being built
    /// * `variants` - The variants being built (empty = all)
    fn pre_build_hook(&self, _working_dir: &PathBuf, _version: &str, _variants: &[&str]) -> Result<()> {
        Ok(())
    }

    /// Hook called during the build process.
    ///
    /// Useful if the project doesn't use traditional build commands
    /// but needs custom logic (e.g., file manipulation).
    fn build_hook(&self, _working_dir: &PathBuf, _version: &str, _variants: &[&str]) -> Result<()> {
        Ok(())
    }

    /// Hook called after the build completes.
    ///
    /// Use this for post-build cleanup or artifact organization.
    fn post_build_hook(&self, _working_dir: &PathBuf, _version: &str, _variants: &[&str]) -> Result<()> {
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

    /// Get build commands.
    ///
    /// If project has variants, only requested ones are built.
    /// If no variants, the `variants` parameter is ignored.
    ///
    /// # Arguments
    ///
    /// * `version` - The version being built (may be empty if not required)
    /// * `variants` - The variants to build (empty = all)
    fn build_commands(&self, version: &str, variants: &[&str]) -> Vec<String>;

    /// Get the artifacts produced by the build.
    ///
    /// This method is called after the build completes. Builders should return
    /// resolved, concrete paths to the produced artifacts. If the builder needs
    /// to use glob patterns to locate files (e.g., when filenames contain commit
    /// hashes), it should resolve them internally before returning.
    ///
    /// # Arguments
    ///
    /// * `working_dir` - The directory where the build ran
    /// * `version` - The version that was built
    /// * `variants` - The variants that were built (empty = all)
    ///
    /// # Returns
    ///
    /// A list of artifacts with resolved source paths.
    fn artifacts(&self, working_dir: &PathBuf, version: &str, variants: &[&str]) -> Result<Vec<BuildArtifact>>;

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
    if !s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        return false;
    }

    // Allow digits, dots, hyphens, and alphanumerics
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+')
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
                format!("Failed to read readme file '{}': {}", readme_file.display(), e),
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
}

