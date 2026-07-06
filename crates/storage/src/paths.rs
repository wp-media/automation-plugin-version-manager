//! Store layout, input validation, and filesystem-name sanitization.
//!
//! # Layout
//!
//! ```text
//! {base_dir}/apvm.db                                  ← all metadata (SQLite)
//! {base_dir}/{project}/commits/{version}/{commit}/    ← build artifact files
//! {base_dir}/{project}/releases/{tag_dir}/            ← release asset files
//! ```
//!
//! Directory paths are stored in the database **relative** to `base_dir`
//! with `/` separators, so the whole store can be moved or mounted elsewhere
//! and reopened without any rewrite.
//!
//! # Validation philosophy
//!
//! Only values that become **path components** (project, version, commit,
//! filenames) carry strict character rules — and those rules are enforced on
//! every platform so a store written on Unix stays readable from Windows.
//! Values that live only in the database (tags, branch names, source
//! references) are stored verbatim; tags get a *sanitized derivation* for
//! their on-disk directory name, with the exact value kept in the database.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::fsx;

/// Filename of the SQLite database inside the store base directory.
pub(crate) const DB_FILE_NAME: &str = "apvm.db";

/// Filename of the advisory lock taken by mutating operations.
pub(crate) const LOCK_FILE_NAME: &str = ".apvm.lock";

/// Directory (under a project) holding builds, bucketed by version.
pub(crate) const COMMITS_DIR: &str = "commits";

/// Directory (under a project) holding cached release assets.
pub(crate) const RELEASES_DIR: &str = "releases";

/// Prefix of temporary files created while streaming artifacts in. Anything
/// with this prefix that outlives its writer is a crash leftover; `gc()`
/// removes stale ones.
pub(crate) const TMP_PREFIX: &str = ".apvm-tmp-";

/// Windows reserved device names — forbidden as filename stems even on Unix
/// so the store stays portable.
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

// ============================================================================
// Validators
// ============================================================================

/// Validate a project identifier (a root-level directory name).
///
/// Rules: 1–64 chars, lowercase `[a-z0-9._-]`, must start with `[a-z0-9]`,
/// must not end with `.`, and must not start with `apvm.` (reserved for the
/// database and its WAL siblings).
pub(crate) fn validate_project(project: &str) -> Result<()> {
    if project.is_empty() {
        return Err(Error::invalid("project", project, "must not be empty"));
    }
    if project.len() > 64 {
        return Err(Error::invalid(
            "project",
            project,
            "longer than 64 characters",
        ));
    }
    let mut chars = project.chars();
    let first = chars.next().unwrap_or(' ');
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(Error::invalid(
            "project",
            project,
            "must start with a lowercase letter or digit",
        ));
    }
    if !project
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(Error::invalid(
            "project",
            project,
            "only lowercase letters, digits, '.', '_' and '-' are allowed",
        ));
    }
    if project.ends_with('.') {
        return Err(Error::invalid("project", project, "must not end with '.'"));
    }
    if project.starts_with("apvm.") {
        return Err(Error::invalid(
            "project",
            project,
            "the 'apvm.' prefix is reserved for store internals",
        ));
    }
    Ok(())
}

/// Validate a version string (a directory name under `commits/`).
///
/// Rules: 1–64 chars from `[0-9A-Za-z._+-]`, at least one digit, no leading
/// or trailing `.` (Windows silently strips trailing dots, which would
/// desync the path from the database).
pub(crate) fn validate_version(version: &str) -> Result<()> {
    if version.is_empty() {
        return Err(Error::invalid("version", version, "must not be empty"));
    }
    if version.len() > 64 {
        return Err(Error::invalid(
            "version",
            version,
            "longer than 64 characters",
        ));
    }
    if !version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
    {
        return Err(Error::invalid(
            "version",
            version,
            "only letters, digits, '.', '_', '+' and '-' are allowed",
        ));
    }
    if !version.chars().any(|c| c.is_ascii_digit()) {
        return Err(Error::invalid(
            "version",
            version,
            "must contain at least one digit",
        ));
    }
    if version.starts_with('.') || version.ends_with('.') {
        return Err(Error::invalid(
            "version",
            version,
            "must not start or end with '.'",
        ));
    }
    Ok(())
}

/// Validate a commit SHA (7–64 hex chars) and normalize it to lowercase.
pub(crate) fn validate_commit(commit: &str) -> Result<String> {
    if !(7..=64).contains(&commit.len()) {
        return Err(Error::invalid(
            "commit",
            commit,
            "must be 7 to 64 hexadecimal characters",
        ));
    }
    if !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::invalid(
            "commit",
            commit,
            "must contain only hexadecimal characters",
        ));
    }
    Ok(commit.to_ascii_lowercase())
}

/// Validate a release tag. Tags live in the database (the directory name is
/// a sanitized derivation), so only sanity limits apply: 1–200 chars, no
/// control characters.
pub(crate) fn validate_tag(tag: &str) -> Result<()> {
    if tag.is_empty() {
        return Err(Error::invalid("tag", tag, "must not be empty"));
    }
    if tag.len() > 200 {
        return Err(Error::invalid("tag", tag, "longer than 200 characters"));
    }
    if tag.chars().any(char::is_control) {
        return Err(Error::invalid(
            "tag",
            tag,
            "control characters are not allowed",
        ));
    }
    Ok(())
}

/// Validate a source reference or branch name (database-only values):
/// 1–500 chars, no control characters.
pub(crate) fn validate_reference(what: &'static str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::invalid(what, value, "must not be empty"));
    }
    if value.len() > 500 {
        return Err(Error::invalid(what, value, "longer than 500 characters"));
    }
    if value.chars().any(char::is_control) {
        return Err(Error::invalid(
            what,
            value,
            "control characters are not allowed",
        ));
    }
    Ok(())
}

/// Validate a variant identifier: 1–100 chars, no control characters.
pub(crate) fn validate_variant(variant: &str) -> Result<()> {
    if variant.is_empty() {
        return Err(Error::invalid("variant", variant, "must not be empty"));
    }
    if variant.len() > 100 {
        return Err(Error::invalid(
            "variant",
            variant,
            "longer than 100 characters",
        ));
    }
    if variant.chars().any(char::is_control) {
        return Err(Error::invalid(
            "variant",
            variant,
            "control characters are not allowed",
        ));
    }
    Ok(())
}

/// Validate an artifact filename so it is safe as a single path component on
/// every supported platform.
///
/// Rules: 1–200 chars; no path separators, control chars, or `< > : " | ? *`;
/// not `.` or `..`; no leading space; no trailing space or dot; not a
/// Windows reserved device name (`CON`, `NUL`, `COM1`, ...).
pub(crate) fn validate_filename(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::invalid("filename", name, "must not be empty"));
    }
    if name.len() > 200 {
        return Err(Error::invalid(
            "filename",
            name,
            "longer than 200 characters",
        ));
    }
    if name == "." || name == ".." {
        return Err(Error::invalid(
            "filename",
            name,
            "'.' and '..' are not allowed",
        ));
    }
    if name.chars().any(|c| {
        c.is_control() || matches!(c, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*')
    }) {
        return Err(Error::invalid(
            "filename",
            name,
            "path separators, control characters and < > : \" | ? * are not allowed",
        ));
    }
    if name.starts_with(' ') {
        return Err(Error::invalid(
            "filename",
            name,
            "must not start with a space",
        ));
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Err(Error::invalid(
            "filename",
            name,
            "must not end with a space or '.'",
        ));
    }
    if is_windows_reserved(name) {
        return Err(Error::invalid(
            "filename",
            name,
            "Windows reserved device names are not allowed",
        ));
    }
    Ok(())
}

/// Whether the filename stem (part before the first `.`) is a Windows
/// reserved device name, case-insensitively.
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    WINDOWS_RESERVED
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

// ============================================================================
// Tag directory sanitization
// ============================================================================

/// Derive a filesystem-safe directory name for a release tag.
///
/// Characters outside `[A-Za-z0-9._-]` become `-`; leading/trailing dots are
/// trimmed; the result is capped at 100 chars. Whenever the derivation is
/// lossy (or hits a reserved name), the first 8 hex chars of the tag's
/// SHA-256 are appended so distinct tags can never collide on disk. The
/// exact tag is always kept in the database — lookups match on it, never on
/// the directory name.
pub(crate) fn sanitize_tag_dir(tag: &str) -> String {
    let mapped: String = tag
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();

    let mut name: String = mapped.trim_matches('.').to_string();
    let mut lossy = name != tag;

    if name.chars().count() > 100 {
        name = name.chars().take(100).collect();
        lossy = true;
    }
    if name.is_empty() {
        name = "tag".to_string();
        lossy = true;
    }
    if is_windows_reserved(&name) || name.starts_with("apvm.") {
        lossy = true;
    }

    if lossy {
        format!("{name}-{}", hash8(tag))
    } else {
        name
    }
}

/// First 8 hex chars of the SHA-256 of `input` — a compact deterministic
/// disambiguator for filesystem names.
pub(crate) fn hash8(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    fsx::to_hex(&digest[..4])
}

// ============================================================================
// Relative paths
// ============================================================================

/// Relative directory of a build: `{project}/commits/{version}/{dir_name}`.
pub(crate) fn build_dir_rel(project: &str, version: &str, dir_name: &str) -> String {
    format!("{project}/{COMMITS_DIR}/{version}/{dir_name}")
}

/// Relative directory of a release: `{project}/releases/{tag_dir}`.
pub(crate) fn release_dir_rel(project: &str, tag_dir: &str) -> String {
    format!("{project}/{RELEASES_DIR}/{tag_dir}")
}

/// Join a stored relative path onto the base directory, component by
/// component, so it works on every platform.
///
/// Splits on both `/` and `\` and drops `.`/`..`/empty components.
/// Legitimate stored paths never contain `.` or `..` as a component
/// (project, version, commit and sanitized tags all forbid them), so this
/// filtering cannot change a valid path — it only ensures the result stays
/// inside `base_dir` even if the database were tampered with.
pub(crate) fn rel_to_abs(base_dir: &Path, rel: &str) -> PathBuf {
    rel.split(['/', '\\'])
        .filter(|component| !component.is_empty() && *component != "." && *component != "..")
        .fold(base_dir.to_path_buf(), |path, component| {
            path.join(component)
        })
}

/// Resolve a stored relative path to an absolute path **only when it is a
/// clean, strict descendant of `base_dir`**.
///
/// Every well-formed stored directory (`{project}/commits/{version}/{dir}`
/// or `{project}/releases/{tag_dir}`) qualifies. The path is rejected
/// (`None`) when it is empty, or contains any empty / `.` / `..` component —
/// conditions only reachable from a corrupt or tampered database. Unlike
/// [`rel_to_abs`], which *neutralizes* such components (fine for read paths,
/// where a wrong path is just a cache miss), this refuses them so
/// destructive operations (`delete_build`, `delete_release`, `clean`) never
/// remove the store root or follow a tampered path into the filesystem — the
/// bogus record is dropped from the index and its files are left untouched
/// (reclaimed later by [`crate::ArtifactStore::gc`]).
pub(crate) fn resolve_within_base(base_dir: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() {
        return None;
    }
    let mut path = base_dir.to_path_buf();
    let mut components = 0usize;
    for component in rel.split(['/', '\\']) {
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        path.push(component);
        components += 1;
    }
    // At least one component, and (belt and braces) the result must remain
    // strictly inside the base directory.
    (components > 0 && path != base_dir && path.starts_with(base_dir)).then_some(path)
}

// ============================================================================
// Version ordering
// ============================================================================

/// Order two version strings, newest-meaning-greatest, for display sorting.
///
/// Segments (split on `.`, `-`, `_`, `+`) compare numerically when both
/// sides are numeric, lexicographically otherwise. When one version is a
/// prefix of the other, the longer one wins if its next segment is numeric
/// (`1.2 < 1.2.1`) and loses if it is not (`5.6.0-beta1 < 5.6.0`). This is
/// an intentional approximation of semver used only for sorting lists.
pub(crate) fn cmp_versions(a: &str, b: &str) -> Ordering {
    let split = |s: &str| -> Vec<String> {
        s.split(['.', '-', '_', '+'])
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .collect()
    };
    let (left, right) = (split(a), split(b));
    let numeric = |s: &str| s.parse::<u64>().ok();

    for i in 0..left.len().max(right.len()) {
        match (left.get(i), right.get(i)) {
            (Some(l), Some(r)) => {
                let ord = match (numeric(l), numeric(r)) {
                    (Some(ln), Some(rn)) => ln.cmp(&rn),
                    _ => l.cmp(r),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            // Longer side continues: numeric continuation = newer patch
            // (1.2.1 > 1.2); non-numeric continuation = prerelease (< base).
            (Some(l), None) => {
                return if numeric(l).is_some() {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (None, Some(r)) => {
                return if numeric(r).is_some() {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (None, None) => break,
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_validation() {
        assert!(validate_project("backwpup").is_ok());
        assert!(validate_project("wp-rocket").is_ok());
        assert!(validate_project("").is_err());
        assert!(validate_project("WP-Rocket").is_err());
        assert!(validate_project(".hidden").is_err());
        assert!(validate_project("has/slash").is_err());
        assert!(validate_project("ends.").is_err());
        assert!(validate_project("apvm.db").is_err());
        assert!(validate_project(&"x".repeat(65)).is_err());
    }

    #[test]
    fn version_validation() {
        assert!(validate_version("5.6.0").is_ok());
        assert!(validate_version("9.99.99").is_ok());
        assert!(validate_version("3.18.1-beta1").is_ok());
        assert!(validate_version("").is_err());
        assert!(validate_version("beta").is_err()); // no digit
        assert!(validate_version(".5").is_err());
        assert!(validate_version("5.").is_err());
        assert!(validate_version("5 6").is_err());
    }

    /// Regression test: the validator intentionally checks charset + "has a
    /// digit" + no leading/trailing '.', not a fixed segment count or semver
    /// shape — so real plugin version schemes must keep working. BackWPup
    /// uses plain three-segment versions; WP Rocket uses four segments and
    /// alpha/beta pre-release suffixes.
    #[test]
    fn version_validation_accepts_real_world_plugin_formats() {
        for version in [
            "5.6.7",          // BackWPup-style x.x.x
            "3.22.1",         // BackWPup-style x.x.x
            "3.22.1.1",       // WP Rocket-style x.x.x.x
            "v3.22.1-alpha2", // WP Rocket-style pre-release
            "v3.22.1-beta",
            "v3.22.1-beta2",
        ] {
            assert!(
                validate_version(version).is_ok(),
                "expected {version:?} to be accepted"
            );
        }
    }

    #[test]
    fn commit_validation_normalizes_case() {
        assert_eq!(validate_commit("A1B2C3D").unwrap(), "a1b2c3d");
        assert!(validate_commit("a1b2c3").is_err()); // too short
        assert!(validate_commit("zzzzzzz").is_err()); // not hex
        assert!(validate_commit(&"a".repeat(65)).is_err());
    }

    #[test]
    fn filename_validation() {
        assert!(validate_filename("backwpup-5.6.0.zip").is_ok());
        assert!(validate_filename("").is_err());
        assert!(validate_filename("..").is_err());
        assert!(validate_filename("a/b.zip").is_err());
        assert!(validate_filename("a\\b.zip").is_err());
        assert!(validate_filename("bad:name.zip").is_err());
        assert!(validate_filename("trailing.").is_err());
        assert!(validate_filename(" leading.zip").is_err());
        assert!(validate_filename("CON.zip").is_err());
        assert!(validate_filename("com1.tar.gz").is_err());
    }

    #[test]
    fn tag_sanitization_is_safe_and_collision_free() {
        // Clean tags keep their name verbatim.
        assert_eq!(sanitize_tag_dir("v5.3.2"), "v5.3.2");
        // Lossy tags get a hash suffix; distinct tags stay distinct.
        let a = sanitize_tag_dir("release/5.3");
        let b = sanitize_tag_dir("release?5.3");
        assert!(a.starts_with("release-5.3-"));
        assert_ne!(a, b);
        // Degenerate input still yields a usable name.
        assert!(
            sanitize_tag_dir("///").starts_with("tag-") || sanitize_tag_dir("///").contains('-')
        );
        // Reserved names are suffixed.
        assert_ne!(sanitize_tag_dir("CON"), "CON");
    }

    #[test]
    fn rel_path_round_trip() {
        let rel = build_dir_rel("backwpup", "5.6.0", "a1b2c3d");
        assert_eq!(rel, "backwpup/commits/5.6.0/a1b2c3d");
        let abs = rel_to_abs(Path::new("/base"), &rel);
        assert!(abs.ends_with(Path::new("backwpup/commits/5.6.0/a1b2c3d")));
    }

    #[test]
    fn rel_to_abs_neutralizes_traversal_components() {
        let base = Path::new("/base");
        // `..`, `.`, empty and backslash-separated traversal all collapse to
        // safe descendants (or the base) — never above it.
        for rel in ["../escape", "../../etc/passwd", "a/../../b", "..\\..\\x"] {
            let abs = rel_to_abs(base, rel);
            assert!(
                abs.starts_with(base),
                "{rel:?} escaped base: {}",
                abs.display()
            );
        }
    }

    #[test]
    fn resolve_within_base_accepts_clean_descendants_rejects_tampered() {
        let base = Path::new("/base");
        // A well-formed build directory resolves.
        let rel = build_dir_rel("backwpup", "5.6.0", "a1b2c3d");
        assert_eq!(
            resolve_within_base(base, &rel),
            Some(Path::new("/base/backwpup/commits/5.6.0/a1b2c3d").to_path_buf())
        );
        // Any traversal / empty / dot component (only reachable via a
        // tampered database) is refused outright, so destructive callers
        // never follow it or remove the store root.
        for tampered in [
            "",
            ".",
            "..",
            "../escape",
            "../../etc/passwd",
            "a/../../b",
            "..\\..\\x",
            "backwpup//commits",
            "backwpup/./commits",
        ] {
            assert_eq!(
                resolve_within_base(base, tampered),
                None,
                "tampered path {tampered:?} must be refused"
            );
        }
    }

    #[test]
    fn version_ordering() {
        assert_eq!(cmp_versions("5.6.0", "5.6.0"), Ordering::Equal);
        assert_eq!(cmp_versions("5.10.0", "5.9.0"), Ordering::Greater);
        assert_eq!(cmp_versions("1.2", "1.2.1"), Ordering::Less);
        assert_eq!(cmp_versions("5.6.0-beta1", "5.6.0"), Ordering::Less);
        assert_eq!(
            cmp_versions("5.6.0-beta2", "5.6.0-beta1"),
            Ordering::Greater
        );
    }
}
