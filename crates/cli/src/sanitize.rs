//! Input sanitization for CLI configuration values.
//!
//! Ensures user-provided values are safe and normalized before being stored
//! in the configuration file. Each config key has its own validation rules.

use std::path::{Path, PathBuf};

use apvm_config::ConfigKey;

/// Result of sanitizing a config value.
#[derive(Debug)]
pub enum SanitizeResult {
    /// Value accepted (possibly transformed) with the clean value.
    Ok(String),
    /// Value accepted but the user should be warned about something.
    Warning { value: String, message: String },
    /// Value rejected with an error explanation.
    Error(String),
}

/// Sanitize a config value based on its key.
///
/// Dispatches to key-specific sanitization logic:
/// - [`ConfigKey::Token`] → [`sanitize_token`]
/// - [`ConfigKey::BuildsDir`] → [`sanitize_path`]
pub fn sanitize_value(key: ConfigKey, value: &str) -> SanitizeResult {
    match key {
        ConfigKey::Token => sanitize_token(value),
        ConfigKey::BuildsDir => sanitize_path(value),
    }
}

/// Sanitize a GitHub token value.
///
/// Rules:
/// - Trim leading/trailing whitespace
/// - Reject empty strings
/// - Warn on unrecognized prefixes (formats change over time)
///
/// Known GitHub token prefixes:
/// - `ghp_` — Personal Access Token (classic)
/// - `gho_` — OAuth Access Token
/// - `ghu_` — User-to-server Token
/// - `ghs_` — Server-to-server Token
/// - `ghr_` — Refresh Token
/// - `github_pat_` — Fine-grained Personal Access Token
fn sanitize_token(value: &str) -> SanitizeResult {
    let trimmed = value.trim();

    if trimmed.is_empty() {
        return SanitizeResult::Error(
            "Token cannot be empty. Use 'apvm config unset token' to remove it.".to_string(),
        );
    }

    // Check for known GitHub token prefixes
    let known_prefixes = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"];

    let has_known_prefix = known_prefixes.iter().any(|p| trimmed.starts_with(p));

    if has_known_prefix {
        SanitizeResult::Ok(trimmed.to_string())
    } else {
        SanitizeResult::Warning {
            value: trimmed.to_string(),
            message: format!(
                "Token doesn't start with a known GitHub prefix ({}). \
                 It will be stored as-is, but verify it's correct.",
                known_prefixes.join(", ")
            ),
        }
    }
}

/// Sanitize a path value for use as a directory.
///
/// Rules:
/// - Trim leading/trailing whitespace
/// - Reject empty strings
/// - Expand `~` to home directory
/// - Resolve `.` and `..` to produce an absolute path
/// - Reject paths that are still relative after resolution
/// - The stored path is always absolute
fn sanitize_path(value: &str) -> SanitizeResult {
    let trimmed = value.trim();

    if trimmed.is_empty() {
        return SanitizeResult::Error(
            "Path cannot be empty. Use 'apvm config unset builds-dir' to reset to default."
                .to_string(),
        );
    }

    let expanded = expand_tilde(trimmed);
    let resolved = resolve_path(&expanded);

    if !resolved.is_absolute() {
        return SanitizeResult::Error(format!(
            "Path must be absolute. '{}' resolved to '{}' which is not absolute.",
            trimmed,
            resolved.display()
        ));
    }

    SanitizeResult::Ok(resolved.display().to_string())
}

/// Expand `~` at the start of a path to the user's home directory.
///
/// Only expands a leading `~` followed by `/` or end-of-string.
/// Does not expand `~user` — only the current user's home.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }

    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }

    PathBuf::from(path)
}

/// Resolve a path to an absolute path.
///
/// Uses `std::fs::canonicalize` if the path exists (resolves symlinks, `.`, `..`).
/// Falls back to manual resolution against `$PWD` if the path doesn't exist yet.
fn resolve_path(path: &Path) -> PathBuf {
    // If the path already exists, canonicalize resolves everything
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    // Path doesn't exist yet — resolve manually
    if path.is_relative()
        && let Ok(cwd) = std::env::current_dir()
    {
        return normalize_components(&cwd.join(path));
    }

    normalize_components(path)
}

/// Normalize path components by resolving `.` and `..` without touching the
/// filesystem.
///
/// This is a pure string operation — it doesn't check if directories exist
/// or follow symlinks.
fn normalize_components(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();

    for component in path.components() {
        match component {
            std::path::Component::CurDir => {
                // `.` — skip
            }
            std::path::Component::ParentDir => {
                // `..` — pop the last component
                result.pop();
            }
            other => {
                result.push(other);
            }
        }
    }

    result
}

/// Get the current user's home directory.
///
/// # Panics
///
/// Panics if the home directory cannot be determined. This is expected to
/// always succeed on supported platforms (macOS, Linux).
fn home_dir() -> PathBuf {
    directories::BaseDirs::new()
        .expect("Failed to determine home directory")
        .home_dir()
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Token sanitization
    // =========================================================================

    #[test]
    fn token_valid_ghp_prefix() {
        let result = sanitize_token("ghp_abc123");
        assert!(matches!(result, SanitizeResult::Ok(v) if v == "ghp_abc123"));
    }

    #[test]
    fn token_valid_github_pat_prefix() {
        let result = sanitize_token("github_pat_abc123");
        assert!(matches!(result, SanitizeResult::Ok(v) if v == "github_pat_abc123"));
    }

    #[test]
    fn token_trims_whitespace() {
        let result = sanitize_token("  ghp_abc123  ");
        assert!(matches!(result, SanitizeResult::Ok(v) if v == "ghp_abc123"));
    }

    #[test]
    fn token_empty_is_error() {
        let result = sanitize_token("");
        assert!(matches!(result, SanitizeResult::Error(_)));
    }

    #[test]
    fn token_whitespace_only_is_error() {
        let result = sanitize_token("   ");
        assert!(matches!(result, SanitizeResult::Error(_)));
    }

    #[test]
    fn token_unknown_prefix_warns() {
        let result = sanitize_token("some_random_token");
        assert!(matches!(result, SanitizeResult::Warning { .. }));
    }

    #[test]
    fn token_all_known_prefixes_accepted() {
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"] {
            let token = format!("{prefix}test123");
            let result = sanitize_token(&token);
            assert!(
                matches!(result, SanitizeResult::Ok(_)),
                "Expected Ok for prefix '{prefix}'"
            );
        }
    }

    // =========================================================================
    // Path sanitization
    // =========================================================================

    #[test]
    fn path_absolute_stays_absolute() {
        let result = sanitize_path("/var/lib/builds");
        assert!(matches!(result, SanitizeResult::Ok(v) if v == "/var/lib/builds"));
    }

    #[test]
    fn path_dot_resolves_to_cwd() {
        let result = sanitize_path(".");
        if let SanitizeResult::Ok(v) = result {
            let resolved = PathBuf::from(&v);
            assert!(resolved.is_absolute(), "Expected absolute, got: {v}");
        } else {
            panic!("Expected Ok, got: {result:?}");
        }
    }

    #[test]
    fn path_dotdot_resolves() {
        let result = sanitize_path("/var/lib/../cache/builds");
        assert!(matches!(result, SanitizeResult::Ok(v) if v == "/var/cache/builds"));
    }

    #[test]
    fn path_tilde_expands_to_home() {
        let result = sanitize_path("~/my-builds");
        if let SanitizeResult::Ok(v) = result {
            let resolved = PathBuf::from(&v);
            assert!(resolved.is_absolute(), "Expected absolute, got: {v}");
            assert!(
                resolved.ends_with("my-builds"),
                "Expected to end with 'my-builds', got: {v}"
            );
            assert!(!v.contains('~'), "Expected ~ to be expanded, got: {v}");
        } else {
            panic!("Expected Ok, got: {result:?}");
        }
    }

    #[test]
    fn path_empty_is_error() {
        let result = sanitize_path("");
        assert!(matches!(result, SanitizeResult::Error(_)));
    }

    #[test]
    fn path_whitespace_only_is_error() {
        let result = sanitize_path("   ");
        assert!(matches!(result, SanitizeResult::Error(_)));
    }

    #[test]
    fn path_trims_whitespace() {
        let result = sanitize_path("  /var/lib/builds  ");
        assert!(matches!(result, SanitizeResult::Ok(v) if v == "/var/lib/builds"));
    }

    #[test]
    fn path_relative_resolves_to_absolute() {
        let result = sanitize_path("some/relative/path");
        if let SanitizeResult::Ok(v) = result {
            let resolved = PathBuf::from(&v);
            assert!(resolved.is_absolute(), "Expected absolute path, got: {v}");
        } else {
            panic!("Expected Ok, got: {result:?}");
        }
    }

    // =========================================================================
    // Component helpers
    // =========================================================================

    #[test]
    fn expand_tilde_bare() {
        let result = expand_tilde("~");
        assert!(result.is_absolute());
    }

    #[test]
    fn expand_tilde_with_subpath() {
        let result = expand_tilde("~/foo/bar");
        assert!(result.is_absolute());
        assert!(result.ends_with("foo/bar"));
    }

    #[test]
    fn expand_tilde_not_at_start() {
        let result = expand_tilde("/home/~user");
        assert_eq!(result, PathBuf::from("/home/~user"));
    }

    #[test]
    fn normalize_removes_dots() {
        let result = normalize_components(Path::new("/a/./b/../c"));
        assert_eq!(result, PathBuf::from("/a/c"));
    }

    // =========================================================================
    // sanitize_value dispatch
    // =========================================================================

    #[test]
    fn sanitize_value_dispatches_token() {
        let result = sanitize_value(ConfigKey::Token, "ghp_test");
        assert!(matches!(result, SanitizeResult::Ok(_)));
    }

    #[test]
    fn sanitize_value_dispatches_path() {
        let result = sanitize_value(ConfigKey::BuildsDir, "/my/builds");
        assert!(matches!(result, SanitizeResult::Ok(_)));
    }
}
