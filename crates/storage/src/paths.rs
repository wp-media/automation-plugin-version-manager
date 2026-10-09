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

/// Marker repair writes before it changes the database and removes once the
/// rebuilt index is committed. Left behind only by an interrupted repair: it
/// marks the store as needing repair, so nothing trusts (or `gc` acts on) a
/// half-built index.
pub(crate) const REPAIR_MARKER_NAME: &str = "apvm.db.repairing";

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
/// must not end with `.`, must not start with `apvm.` (reserved for the
/// database and its WAL siblings), and must not be a Windows device name.
pub(crate) fn validate_project(project: &str) -> Result<()> {
    if project.is_empty() {
        return Err(Error::invalid("project", project, "must not be empty"));
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
    // Checked after the ASCII-only rules, so bytes == characters here.
    if project.len() > 64 {
        return Err(Error::invalid(
            "project",
            project,
            "longer than 64 characters",
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
    if is_windows_reserved(project) {
        return Err(Error::invalid(
            "project",
            project,
            "Windows reserved device names are not allowed",
        ));
    }
    Ok(())
}

/// Validate a version string (a directory name under `commits/`).
///
/// Rules: 1–64 chars from `[0-9A-Za-z._+-]`, at least one digit, no leading
/// or trailing `.` (Windows silently strips trailing dots, which would
/// desync the path from the database), and no Windows device name (`COM1`,
/// `lpt1.2`).
pub(crate) fn validate_version(version: &str) -> Result<()> {
    if version.is_empty() {
        return Err(Error::invalid("version", version, "must not be empty"));
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
    // Checked after the ASCII-only rule, so bytes == characters here.
    if version.len() > 64 {
        return Err(Error::invalid(
            "version",
            version,
            "longer than 64 characters",
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
    if is_windows_reserved(version) {
        return Err(Error::invalid(
            "version",
            version,
            "Windows reserved device names are not allowed",
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
/// a sanitized derivation), so only sanity limits apply: 1–200 bytes of
/// UTF-8, no control characters.
pub(crate) fn validate_tag(tag: &str) -> Result<()> {
    if tag.is_empty() {
        return Err(Error::invalid("tag", tag, "must not be empty"));
    }
    if tag.len() > 200 {
        return Err(Error::invalid("tag", tag, "longer than 200 bytes"));
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
/// 1–500 bytes of UTF-8, no control characters.
pub(crate) fn validate_reference(what: &'static str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::invalid(what, value, "must not be empty"));
    }
    if value.len() > 500 {
        return Err(Error::invalid(what, value, "longer than 500 bytes"));
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

/// Validate a variant identifier: 1–100 bytes of UTF-8, no control
/// characters.
pub(crate) fn validate_variant(variant: &str) -> Result<()> {
    if variant.is_empty() {
        return Err(Error::invalid("variant", variant, "must not be empty"));
    }
    if variant.len() > 100 {
        return Err(Error::invalid("variant", variant, "longer than 100 bytes"));
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
/// Rules: 1–200 bytes of UTF-8 (filesystems cap name *bytes*); no path
/// separators, control chars, or `< > : " | ? *`;
/// not `.` or `..`; not starting with `.apvm-tmp-` (in-flight copies, which
/// gc sweeps); no leading space; no trailing space or dot; not a Windows
/// reserved device name (`CON`, `NUL`, `COM1`, ...).
pub(crate) fn validate_filename(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::invalid("filename", name, "must not be empty"));
    }
    if name.len() > 200 {
        return Err(Error::invalid("filename", name, "longer than 200 bytes"));
    }
    if name == "." || name == ".." {
        return Err(Error::invalid(
            "filename",
            name,
            "'.' and '..' are not allowed",
        ));
    }
    if name.starts_with(TMP_PREFIX) {
        // gc sweeps such names as crash leftovers.
        return Err(Error::invalid(
            "filename",
            name,
            "the '.apvm-tmp-' prefix is reserved for in-flight copies",
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
/// Splits on both `/` and `\` and keeps only plain names: `.`, `..`, empty
/// components and anything the platform would read as a root or prefix
/// (`C:` on Windows) are dropped. Legitimate stored paths never contain
/// them (project, version, commit and sanitized tags all forbid them), so
/// this filtering cannot change a valid path — it only ensures the result
/// stays inside `base_dir` even if the database were tampered with. Reads
/// check record paths too (`layout::is_store_rel`), and destructive
/// operations resolve through `layout::owned_dir`, which also refuses
/// symlinks.
pub(crate) fn rel_to_abs(base_dir: &Path, rel: &str) -> PathBuf {
    rel.split(['/', '\\'])
        .filter(|component| is_plain_component(component))
        .fold(base_dir.to_path_buf(), |path, component| {
            path.join(component)
        })
}

/// Whether `component` is one plain path name on this platform — so
/// joining it can only descend one level.
fn is_plain_component(component: &str) -> bool {
    let mut parts = Path::new(component).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
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

    /// The `reason` of an [`Error::InvalidInput`] rejection, so a test can
    /// tell *which* rule fired (a length cap vs. a character rule).
    fn rejection(result: Result<()>) -> String {
        match result {
            Err(Error::InvalidInput { reason, .. }) => reason,
            other => panic!("expected an InvalidInput rejection, got {other:?}"),
        }
    }

    #[test]
    fn length_caps_accept_the_limit_and_reject_one_more() {
        // Each cap is part of the on-disk / database contract: an off-by-one
        // either refuses a legitimate value or lets an oversized path
        // component reach the filesystem.
        assert!(validate_version(&format!("1{}", "a".repeat(63))).is_ok());
        assert!(
            rejection(validate_version(&format!("1{}", "a".repeat(64)))).contains("longer than 64")
        );

        assert!(validate_tag(&"t".repeat(200)).is_ok());
        assert!(rejection(validate_tag(&"t".repeat(201))).contains("longer than 200"));

        assert!(validate_reference("branch", &"b".repeat(500)).is_ok());
        assert!(
            rejection(validate_reference("branch", &"b".repeat(501))).contains("longer than 500")
        );

        assert!(validate_variant(&"v".repeat(100)).is_ok());
        assert!(rejection(validate_variant(&"v".repeat(101))).contains("longer than 100"));

        let name = |len: usize| format!("{}.zip", "f".repeat(len - 4));
        assert!(validate_filename(&name(200)).is_ok());
        assert!(rejection(validate_filename(&name(201))).contains("longer than 200"));
    }

    #[test]
    fn ascii_only_fields_report_the_character_rule_before_the_length() {
        // 40 × "é" is 40 characters but 80 bytes: the real problem is the
        // character set, and only then does "64 characters" hold true.
        let wide = "é".repeat(40);
        assert!(rejection(validate_version(&wide)).starts_with("only letters, digits"));
        assert!(rejection(validate_project(&wide)).starts_with("must start with a lowercase"));
        assert!(
            rejection(validate_project(&format!("a{wide}"))).starts_with("only lowercase letters")
        );
    }

    #[test]
    fn non_ascii_limits_count_bytes_and_say_so() {
        // 101 × "é" is 101 characters but 202 bytes (UTF-8). The caps are
        // byte caps (filesystems limit name bytes), and the message must not
        // claim "characters" for a value well under 200 characters.
        let wide = |count: usize| "é".repeat(count);
        assert!(validate_tag(&wide(100)).is_ok());
        assert_eq!(rejection(validate_tag(&wide(101))), "longer than 200 bytes");
        assert_eq!(
            rejection(validate_reference("branch", &wide(251))),
            "longer than 500 bytes"
        );
        assert_eq!(
            rejection(validate_variant(&wide(51))),
            "longer than 100 bytes"
        );
        assert_eq!(
            rejection(validate_filename(&format!("{}.zip", wide(99)))),
            "longer than 200 bytes"
        );
    }

    #[test]
    fn database_only_values_reject_empty_and_control_characters() {
        // Tags, source references, branch names and variants never become
        // path components verbatim, so any printable text — including
        // non-ASCII — is fine; empty values and control characters (which
        // would corrupt one-line CLI output and logs) are not.
        assert!(validate_tag("v5.3.2-ß/release candidate").is_ok());
        assert!(validate_reference("branch", "feature/ünïcode").is_ok());
        assert!(validate_variant("pro-de").is_ok());

        assert!(rejection(validate_tag("")).contains("empty"));
        assert!(rejection(validate_tag("v1\n")).contains("control"));
        assert!(rejection(validate_tag("v1\u{7f}")).contains("control"));

        assert!(rejection(validate_reference("branch", "")).contains("empty"));
        assert!(rejection(validate_reference("branch", "dev\telop")).contains("control"));

        assert!(rejection(validate_variant("")).contains("empty"));
        assert!(rejection(validate_variant("pro\r")).contains("control"));
    }

    #[test]
    fn rejections_name_the_field_the_caller_supplied() {
        // `validate_reference` serves both source references and branch
        // names; the error must say which one was wrong.
        let Err(Error::InvalidInput { what, value, .. }) = validate_reference("branch", "") else {
            panic!("an empty branch must be rejected");
        };
        assert_eq!((what, value.as_str()), ("branch", ""));
        let Err(Error::InvalidInput { what, .. }) = validate_reference("source reference", "a\0")
        else {
            panic!("a NUL in a reference must be rejected");
        };
        assert_eq!(what, "source reference");
    }

    #[test]
    fn long_tags_are_capped_and_stay_distinct() {
        // A 100-char clean tag is kept verbatim: the cap is inclusive.
        let exact = format!("v{}", "1".repeat(99));
        assert_eq!(sanitize_tag_dir(&exact), exact);
        assert_eq!(sanitize_tag_dir(&format!("{exact}1")).len(), 100 + 1 + 8);

        // Longer tags are cut to 100 chars plus a hash of the *whole* tag, so
        // two tags sharing their first 100 chars never share a directory.
        let long = format!("v{}", "1".repeat(149));
        let a = sanitize_tag_dir(&long);
        let b = sanitize_tag_dir(&format!("{long}2"));
        assert_eq!(a.len(), 100 + 1 + 8, "{a}");
        assert!(a.starts_with(&long[..100]));
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        );
    }

    #[test]
    fn tags_spelled_like_store_internals_are_suffixed() {
        // An `apvm.`-prefixed directory name is reserved for the store's own
        // files; a tag spelled that way must not produce one.
        let dir = sanitize_tag_dir("apvm.db");
        assert_ne!(dir, "apvm.db");
        assert!(dir.starts_with("apvm.db-"));
    }

    #[test]
    fn version_ordering_is_antisymmetric_and_sorts_lists() {
        // Sorting needs cmp(a, b) == cmp(b, a).reverse(); each pair drives a
        // different branch (numeric, lexical, longer numeric continuation,
        // longer prerelease continuation) from both sides.
        let pairs = [
            ("5.10.0", "5.9.0"),
            ("10.0", "2.0"),
            ("1.2.1", "1.2"),
            ("5.6.0", "5.6.0-beta1"),
            ("5.6.0-rc1", "5.6.0-beta1"),
        ];
        for (newer, older) in pairs {
            assert_eq!(
                cmp_versions(newer, older),
                Ordering::Greater,
                "{newer} > {older}"
            );
            assert_eq!(
                cmp_versions(older, newer),
                Ordering::Less,
                "{older} < {newer}"
            );
        }
        // Separators are interchangeable.
        assert_eq!(cmp_versions("5.6.0-beta1", "5.6.0_beta1"), Ordering::Equal);

        let mut versions = ["5.6.0", "5.10.0", "1.2", "5.6.0-beta1", "5.9.0", "1.2.1"];
        versions.sort_by(|a, b| cmp_versions(a, b));
        assert_eq!(
            versions,
            ["1.2", "1.2.1", "5.6.0-beta1", "5.6.0", "5.9.0", "5.10.0"]
        );
    }
}
