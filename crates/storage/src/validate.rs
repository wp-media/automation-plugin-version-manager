//! Input validation for storage path components.
//!
//! Everything the caller provides (project names, versions, commits, artifact
//! filenames, release tags) ends up in filesystem paths. These validators
//! guarantee that no input can escape the store's base directory, collide
//! with internal files, or produce names that are invalid on any supported
//! platform (Unix, macOS, Windows).

use crate::error::{Error, Result};

/// Maximum length (in bytes) for a single validated path component.
///
/// Well below every mainstream filesystem's 255-byte component limit, leaving
/// headroom for prefixes (`branch-`, `tag-`) and disambiguation suffixes.
pub(crate) const MAX_COMPONENT_LEN: usize = 128;

/// Windows reserved device names that cannot be used as file/directory names,
/// even with an extension (`CON.txt` is still reserved).
///
/// Source: <https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Check whether a name (or its stem before the first `.`) is a Windows
/// reserved device name, case-insensitively.
///
/// Also used by [`crate::path::safe_component`] to force a disambiguation
/// suffix onto sanitized names that would otherwise be reserved.
pub(crate) fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    WINDOWS_RESERVED
        .iter()
        .any(|r| stem.eq_ignore_ascii_case(r))
}

/// Validate a generic path component: non-empty, bounded length, allowed
/// charset (`A-Z a-z 0-9 . _ - +`), no leading `.`/`-`, no trailing `.`,
/// not a Windows reserved name.
///
/// `what` names the field in error messages (e.g. `"project name"`).
fn validate_component(value: &str, what: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::InvalidInput(format!("{what} must not be empty")));
    }
    if value.len() > MAX_COMPONENT_LEN {
        return Err(Error::InvalidInput(format!(
            "{what} '{value}' exceeds {MAX_COMPONENT_LEN} bytes"
        )));
    }
    if let Some(bad) = value
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+')))
    {
        return Err(Error::InvalidInput(format!(
            "{what} '{value}' contains invalid character {bad:?} \
             (allowed: letters, digits, '.', '_', '-', '+')"
        )));
    }
    // Leading '.' would create hidden directories (and '..' escapes upward);
    // leading '-' confuses command-line tooling operating on the store.
    if value.starts_with('.') || value.starts_with('-') {
        return Err(Error::InvalidInput(format!(
            "{what} '{value}' must not start with '.' or '-'"
        )));
    }
    // Windows silently strips trailing dots, which would desync paths.
    if value.ends_with('.') {
        return Err(Error::InvalidInput(format!(
            "{what} '{value}' must not end with '.'"
        )));
    }
    if is_windows_reserved(value) {
        return Err(Error::InvalidInput(format!(
            "{what} '{value}' is a reserved name on Windows"
        )));
    }
    Ok(())
}

/// Validate a project name (top-level store directory).
pub(crate) fn validate_project(project: &str) -> Result<()> {
    validate_component(project, "project name")
}

/// Validate a version string (directory component under the project).
///
/// Beyond the generic component rules, a version must contain at least one
/// ASCII digit — this also guarantees it can never collide with the reserved
/// `releases` directory that lives alongside `major.minor` directories.
pub(crate) fn validate_version(version: &str) -> Result<()> {
    validate_component(version, "version")?;
    if !version.chars().any(|c| c.is_ascii_digit()) {
        return Err(Error::InvalidInput(format!(
            "version '{version}' must contain at least one digit"
        )));
    }
    Ok(())
}

/// Validate a commit hash (short or full) and return it normalized to
/// lowercase.
///
/// Accepts 7 to 64 hex characters: 7 is git's minimum unambiguous short
/// form, 40 is SHA-1, 64 leaves room for SHA-256 object format repos.
pub(crate) fn validate_commit(commit: &str) -> Result<String> {
    let len = commit.len();
    if !(7..=64).contains(&len) {
        return Err(Error::InvalidInput(format!(
            "commit '{commit}' must be 7-64 hex characters, got {len}"
        )));
    }
    if !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::InvalidInput(format!(
            "commit '{commit}' contains non-hexadecimal characters"
        )));
    }
    Ok(commit.to_ascii_lowercase())
}

/// Validate an artifact filename (the name a stored file will have inside a
/// commit or release directory).
///
/// Must be a bare filename: no path separators, no `.`/`..`, must not shadow
/// internal metadata files, and must be portable to Windows.
pub(crate) fn validate_artifact_filename(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidInput(
            "artifact filename must not be empty".to_string(),
        ));
    }
    if name.len() > 255 {
        return Err(Error::InvalidInput(format!(
            "artifact filename '{name}' exceeds 255 bytes"
        )));
    }
    if name == "." || name == ".." {
        return Err(Error::InvalidInput(format!(
            "artifact filename '{name}' is not a valid file name"
        )));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(Error::InvalidInput(format!(
            "artifact filename '{name}' must not contain path separators"
        )));
    }
    // Reject characters invalid on Windows plus ASCII control characters,
    // so a store written on Unix stays readable from Windows.
    if let Some(bad) = name
        .chars()
        .find(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (*c as u32) < 0x20)
    {
        return Err(Error::InvalidInput(format!(
            "artifact filename '{name}' contains invalid character {bad:?}"
        )));
    }
    if name.ends_with('.') || name.ends_with(' ') || name.starts_with(' ') {
        return Err(Error::InvalidInput(format!(
            "artifact filename '{name}' must not start/end with a space or end with '.'"
        )));
    }
    if is_windows_reserved(name) {
        return Err(Error::InvalidInput(format!(
            "artifact filename '{name}' is a reserved name on Windows"
        )));
    }
    // Never allow artifacts to overwrite store metadata.
    let reserved_metadata = [
        crate::manifest::BuildManifest::FILENAME,
        crate::release::ReleaseManifest::FILENAME,
    ];
    if reserved_metadata
        .iter()
        .any(|m| name.eq_ignore_ascii_case(m))
    {
        return Err(Error::InvalidInput(format!(
            "artifact filename '{name}' is reserved for store metadata"
        )));
    }
    // Temp-file prefix is reserved so stale-temp cleanup can never delete
    // a legitimate artifact.
    if name.starts_with(crate::fsx::TMP_PREFIX) {
        return Err(Error::InvalidInput(format!(
            "artifact filename '{name}' must not start with the reserved prefix '{}'",
            crate::fsx::TMP_PREFIX
        )));
    }
    Ok(())
}

/// Validate a release tag (used to key the releases cache).
///
/// Tags are less constrained than versions (e.g. `v3.0.0-beta.1`,
/// `release/5.6`); the raw tag is sanitized separately for directory usage,
/// so validation only rejects inputs that are empty, oversized, or contain
/// control characters.
pub(crate) fn validate_tag(tag: &str) -> Result<()> {
    if tag.trim().is_empty() {
        return Err(Error::InvalidInput(
            "release tag must not be empty".to_string(),
        ));
    }
    if tag.len() > MAX_COMPONENT_LEN {
        return Err(Error::InvalidInput(format!(
            "release tag '{tag}' exceeds {MAX_COMPONENT_LEN} bytes"
        )));
    }
    if tag.chars().any(|c| (c as u32) < 0x20) {
        return Err(Error::InvalidInput(format!(
            "release tag '{tag}' contains control characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_project_ok() {
        assert!(validate_project("backwpup").is_ok());
        assert!(validate_project("wp-rocket").is_ok());
        assert!(validate_project("my_plugin.v2").is_ok());
    }

    #[test]
    fn test_validate_project_rejects_traversal_and_separators() {
        assert!(validate_project("..").is_err());
        assert!(validate_project("../evil").is_err());
        assert!(validate_project("a/b").is_err());
        assert!(validate_project("a\\b").is_err());
    }

    #[test]
    fn test_validate_project_rejects_empty_hidden_reserved() {
        assert!(validate_project("").is_err());
        assert!(validate_project(".hidden").is_err());
        assert!(validate_project("-flag").is_err());
        assert!(validate_project("CON").is_err());
        assert!(validate_project("nul.txt").is_err());
        assert!(validate_project("trailing.").is_err());
        assert!(validate_project(&"x".repeat(MAX_COMPONENT_LEN + 1)).is_err());
    }

    #[test]
    fn test_validate_version_ok() {
        assert!(validate_version("5.6.0").is_ok());
        assert!(validate_version("3.17.4-beta1").is_ok());
        assert!(validate_version("5").is_ok());
    }

    #[test]
    fn test_validate_version_rejects_bad() {
        assert!(validate_version("").is_err());
        assert!(validate_version("releases").is_err()); // no digit
        assert!(validate_version("..").is_err());
        assert!(validate_version("1.0/x").is_err());
    }

    #[test]
    fn test_validate_commit_normalizes() {
        assert_eq!(validate_commit("ABC1234").unwrap(), "abc1234");
        assert_eq!(
            validate_commit("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2").unwrap(),
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
        );
    }

    #[test]
    fn test_validate_commit_rejects_bad() {
        assert!(validate_commit("").is_err());
        assert!(validate_commit("abc123").is_err()); // too short
        assert!(validate_commit(&"a".repeat(65)).is_err()); // too long
        assert!(validate_commit("xyz1234").is_err()); // non-hex
    }

    #[test]
    fn test_validate_artifact_filename_ok() {
        assert!(validate_artifact_filename("plugin.zip").is_ok());
        assert!(validate_artifact_filename("backwpup-pro-5.6.0.zip").is_ok());
    }

    #[test]
    fn test_validate_artifact_filename_rejects_bad() {
        assert!(validate_artifact_filename("").is_err());
        assert!(validate_artifact_filename("a/b.zip").is_err());
        assert!(validate_artifact_filename("a\\b.zip").is_err());
        assert!(validate_artifact_filename("..").is_err());
        assert!(validate_artifact_filename("con.zip").is_err());
        assert!(validate_artifact_filename("bad:name.zip").is_err());
        assert!(validate_artifact_filename("bad\u{0}name").is_err());
        assert!(validate_artifact_filename("trailing.zip ").is_err());
    }

    #[test]
    fn test_validate_artifact_filename_rejects_metadata_names() {
        assert!(validate_artifact_filename("build-manifest.json").is_err());
        assert!(validate_artifact_filename("BUILD-MANIFEST.JSON").is_err());
        assert!(validate_artifact_filename("release-manifest.json").is_err());
        assert!(validate_artifact_filename(".apvm-tmp-123").is_err());
    }

    #[test]
    fn test_validate_tag() {
        assert!(validate_tag("v1.0.0").is_ok());
        assert!(validate_tag("release/5.6").is_ok());
        assert!(validate_tag("").is_err());
        assert!(validate_tag("   ").is_err());
        assert!(validate_tag("bad\ntag").is_err());
    }
}
