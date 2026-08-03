//! Cross-platform filesystem utilities for build operations.
//!
//! This module provides pure Rust replacements for common shell commands
//! (`rsync`, `zip`, `mkdir -p`) used during the build process. These
//! functions work on all platforms without external tool dependencies.
//!
//! # Motivation
//!
//! Unix build commands (`rsync`, `zip`, `mkdir -p`) are unavailable on
//! Windows by default. These functions provide identical behavior using
//! only Rust standard library and well-established crates:
//!
//! - [`walkdir`](https://docs.rs/walkdir/2) for recursive directory traversal
//! - [`zip`](https://docs.rs/zip/8) for ZIP archive creation
//!
//! # Functions
//!
//! - [`copy_dir_with_exclusions`] — recursive directory copy with name-based
//!   exclusions (replaces `rsync -a --exclude`)
//! - [`create_zip_archive`] — create a ZIP archive from a directory with
//!   name-based exclusions (replaces `zip -r ... -x`)
//! - [`matches_any_exclusion`] — check if a filename matches any
//!   depth-independent [`ExclusionPattern`]
//! - [`matches_exclusion_at_depth`] — same check, including the
//!   depth-sensitive [`ExclusionPattern::RootPrefix`]

use std::path::Path;

use walkdir::WalkDir;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::error::{Error, Result};

// =============================================================================
// Exclusion Patterns
// =============================================================================

/// Depth of an entry that sits directly inside the walked root.
///
/// [`walkdir`] reports the root itself as depth `0`, so its immediate children
/// — the top level of the produced archive — are depth `1`.
const ROOT_CHILD_DEPTH: usize = 1;

/// Stand-in depth for callers that have no depth to supply.
///
/// Deliberately never equal to [`ROOT_CHILD_DEPTH`], so depth-sensitive patterns
/// cannot match when the entry's position is unknown.
const UNKNOWN_DEPTH: usize = usize::MAX;

/// An exclusion pattern for filtering entries during copy or archive operations.
///
/// Every variant matches against the entry's **name component**, never the full
/// path. [`RootPrefix`](Self::RootPrefix) additionally constrains *where* the
/// entry may sit, which is what distinguishes "a tooling file at the plugin
/// root" from "any path that happens to contain a similarly named directory".
///
/// # Why the distinction matters
///
/// The shell equivalents behave very differently, because `zip`'s `*` also
/// matches `/`:
///
/// | Pattern                    | Shell equivalent      | Matches `vendor/wordpress/php-mcp-schema/src/…` |
/// |----------------------------|-----------------------|--------------------------------------------------|
/// | `Prefix("php")`            | `-x "*/php*"`         | **yes** — `*` spans `vendor/wordpress`           |
/// | `RootPrefix("php")`        | `-x "<root>/php*"`    | no                                               |
///
/// Using [`Prefix`](Self::Prefix) for root-level tooling files therefore
/// silently deletes any nested directory whose name shares the prefix. Prefer
/// [`RootPrefix`](Self::RootPrefix) whenever the intent is "top level only".
///
/// # Examples
///
/// ```ignore
/// use apvm_core::build::fs::ExclusionPattern;
///
/// // Dotfiles at any depth (e.g., .git, .env, .gitignore)
/// let dotfiles = ExclusionPattern::Prefix(".");
///
/// // One exact filename, at any depth
/// let gulpfile = ExclusionPattern::Exact("gulpfile.js");
///
/// // Root-level tooling configs only (phpcs.xml, phpstan.neon.dist, …) —
/// // leaves vendor/**/php-*/ subtrees intact.
/// let php_tooling = ExclusionPattern::RootPrefix("php");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusionPattern<'a> {
    /// Match entries at **any depth** whose name starts with the given prefix.
    ///
    /// Equivalent to `zip -x "*/<prefix>*"`.
    ///
    /// Example: `Prefix(".")` matches `.git`, `.env`, `.gitignore`.
    ///
    /// Because this matches at any depth, a prefix shared with a nested
    /// directory name excludes that whole subtree. Use
    /// [`RootPrefix`](Self::RootPrefix) for top-level-only rules.
    Prefix(&'a str),

    /// Match entries at **any depth** whose name equals the given string exactly.
    ///
    /// Equivalent to `zip -x "*/<name>"`.
    ///
    /// Example: `Exact("gulpfile.js")` matches only `gulpfile.js`.
    Exact(&'a str),

    /// Match entries **directly at the root** whose name starts with the prefix.
    ///
    /// Equivalent to `zip -x "<root_dir>/<prefix>*"`. Nested entries are never
    /// matched, no matter how their names begin.
    ///
    /// Example: `RootPrefix("php")` matches a root-level `phpcs.xml` but not
    /// `vendor/wordpress/php-mcp-schema/src/Server/Tools/DTO/Tool.php`.
    RootPrefix(&'a str),
}

impl ExclusionPattern<'_> {
    /// Whether this pattern matches an entry with the given name and depth.
    ///
    /// # Arguments
    ///
    /// * `filename` - The entry's name component (not the full path).
    /// * `depth` - Depth relative to the walked root, as reported by
    ///   [`walkdir::DirEntry::depth`]: `0` is the root, `1` its direct children.
    fn matches(&self, filename: &str, depth: usize) -> bool {
        match self {
            Self::Prefix(prefix) => filename.starts_with(prefix),
            Self::Exact(name) => filename == *name,
            Self::RootPrefix(prefix) => depth == ROOT_CHILD_DEPTH && filename.starts_with(prefix),
        }
    }
}

/// Check if a filename matches any **depth-independent** exclusion pattern.
///
/// [`ExclusionPattern::RootPrefix`] is depth-sensitive and can never match here,
/// because no depth is supplied. Use [`matches_exclusion_at_depth`] when the
/// entry's depth is known — that is what [`create_zip_archive`] does.
///
/// # Arguments
///
/// * `filename` - The filename (not full path) to check.
/// * `patterns` - Slice of [`ExclusionPattern`]s to match against.
///
/// # Returns
///
/// `true` if `filename` matches at least one depth-independent pattern.
///
/// # Examples
///
/// ```ignore
/// use apvm_core::build::fs::{ExclusionPattern, matches_any_exclusion};
///
/// let patterns = &[
///     ExclusionPattern::Prefix("."),
///     ExclusionPattern::Exact("gulpfile.js"),
/// ];
///
/// assert!(matches_any_exclusion(".gitignore", patterns));
/// assert!(matches_any_exclusion("gulpfile.js", patterns));
/// assert!(!matches_any_exclusion("index.php", patterns));
/// ```
pub fn matches_any_exclusion(filename: &str, patterns: &[ExclusionPattern<'_>]) -> bool {
    matches_exclusion_at_depth(filename, UNKNOWN_DEPTH, patterns)
}

/// Check if an entry matches any exclusion pattern, honoring depth-sensitive rules.
///
/// # Arguments
///
/// * `filename` - The entry's name component (not the full path).
/// * `depth` - Depth relative to the walked root, as reported by
///   [`walkdir::DirEntry::depth`]: `0` is the root, `1` its direct children.
/// * `patterns` - Slice of [`ExclusionPattern`]s to match against.
///
/// # Returns
///
/// `true` if the entry matches at least one pattern.
///
/// # Examples
///
/// ```ignore
/// use apvm_core::build::fs::{ExclusionPattern, matches_exclusion_at_depth};
///
/// let patterns = &[ExclusionPattern::RootPrefix("php")];
///
/// // Root-level tooling config — excluded.
/// assert!(matches_exclusion_at_depth("phpcs.xml", 1, patterns));
/// // Nested vendor package sharing the prefix — kept.
/// assert!(!matches_exclusion_at_depth("php-mcp-schema", 3, patterns));
/// ```
pub fn matches_exclusion_at_depth(
    filename: &str,
    depth: usize,
    patterns: &[ExclusionPattern<'_>],
) -> bool {
    patterns
        .iter()
        .any(|pattern| pattern.matches(filename, depth))
}

// =============================================================================
// Directory Copy
// =============================================================================

/// Recursively copy a directory's contents to a destination, skipping
/// entries whose names match any of the given exclusion patterns.
///
/// This is the pure Rust equivalent of:
///
/// ```sh
/// rsync -a src/ dst/ --exclude dir1 --exclude dir2 ...
/// ```
///
/// # Behavior
///
/// - Uses [`walkdir::IntoIter::filter_entry`] to prevent **descending** into excluded
///   directories entirely (critical for performance — e.g., `node_modules`
///   can contain 50,000+ files).
///   Source: <https://docs.rs/walkdir/2/walkdir/struct.IntoIter.html#method.filter_entry>
/// - Creates destination directories as needed (`mkdir -p` equivalent via
///   [`std::fs::create_dir_all`]).
/// - Copies regular files with [`std::fs::copy`], which preserves file
///   contents and platform-appropriate metadata.
/// - Symlinks are skipped (not followed, not copied) because WordPress plugins
///   do not use symlinks in production builds.
///
/// # Arguments
///
/// * `src` - Source directory to copy from. Must exist.
/// * `dst` - Destination directory to copy into. Created if it does not exist.
/// * `exclude_names` - Directory/file names to skip. Matched against the
///   **filename component** of each entry (exact string match).
///
/// # Returns
///
/// The number of files (not directories) copied.
///
/// # Errors
///
/// Returns [`Error::Build`] if:
/// - The source directory cannot be walked (permissions, missing, etc.)
/// - A directory cannot be created at the destination
/// - A file cannot be copied
pub fn copy_dir_with_exclusions(src: &Path, dst: &Path, exclude_names: &[&str]) -> Result<u64> {
    let mut files_copied: u64 = 0;

    // Create destination root if it doesn't exist
    std::fs::create_dir_all(dst).map_err(|e| {
        Error::Build(format!(
            "Failed to create destination directory '{}': {e}",
            dst.display()
        ))
    })?;

    // WalkDir::filter_entry prevents descending into excluded directories,
    // which is much faster than walking into them and skipping files.
    let walker = WalkDir::new(src).into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        !exclude_names.iter().any(|exc| *exc == name.as_ref())
    });

    for entry in walker {
        let entry = entry.map_err(|e| {
            Error::Build(format!(
                "Failed to walk source directory '{}': {e}",
                src.display()
            ))
        })?;

        // Compute path relative to source root
        let relative = entry.path().strip_prefix(src).map_err(|e| {
            Error::Build(format!(
                "Failed to compute relative path for '{}': {e}",
                entry.path().display()
            ))
        })?;

        // Skip the root entry (src itself)
        if relative.as_os_str().is_empty() {
            continue;
        }

        let target = dst.join(relative);

        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| {
                Error::Build(format!(
                    "Failed to create directory '{}': {e}",
                    target.display()
                ))
            })?;
        } else if entry.file_type().is_file() {
            // Ensure parent directory exists (handles cases where walkdir
            // yields a file before its parent directory entry)
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Error::Build(format!(
                        "Failed to create parent directory '{}': {e}",
                        parent.display()
                    ))
                })?;
            }
            std::fs::copy(entry.path(), &target).map_err(|e| {
                Error::Build(format!(
                    "Failed to copy '{}' to '{}': {e}",
                    entry.path().display(),
                    target.display()
                ))
            })?;
            files_copied += 1;
        }
        // Symlinks and other file types are intentionally skipped.
    }

    Ok(files_copied)
}

// =============================================================================
// ZIP Archive Creation
// =============================================================================

/// Create a ZIP archive from a directory's contents, with name-based exclusions.
///
/// This is the pure Rust equivalent of:
///
/// ```sh
/// cd parent_of_content && zip -r output.zip prefix_dir -x "*/pattern1" -x "*/pattern2"
/// ```
///
/// # Behavior
///
/// - Walks `content_dir` recursively using [`WalkDir`].
/// - Each entry is stored in the archive under `archive_prefix/relative/path`,
///   preceded by an `archive_prefix/` directory entry for the archive root —
///   the same entry `zip -r <archive> <prefix_dir>` records.
/// - `archive_prefix` must be non-empty; it names the single top-level directory
///   every entry lives under (WordPress requires the plugin slug as that name).
/// - Entries matching any `exclusions` pattern are skipped. An excluded
///   **directory** is pruned via [`walkdir::IntoIter::filter_entry`], so its
///   whole subtree is left out — matching `zip -x`, whose `*` also spans `/`.
///   Pruning also avoids descending into large excluded trees.
///   Source: <https://docs.rs/walkdir/2/walkdir/struct.IntoIter.html#method.filter_entry>
/// - Depth-sensitive patterns ([`ExclusionPattern::RootPrefix`]) are resolved
///   against the entry's depth below `content_dir`, so they apply to the top
///   level of the archive only.
/// - Uses Deflate compression ([`CompressionMethod::Deflated`]), matching
///   the default behavior of the `zip` command-line tool.
///   Source: <https://docs.rs/zip/8/zip/enum.CompressionMethod.html>
/// - Path separators are normalized to `/` for ZIP compatibility on all platforms.
///   Source: <https://docs.rs/zip/8/zip/write/struct.ZipWriter.html#method.start_file>
///
/// # Arguments
///
/// * `content_dir` - The directory whose contents will be archived.
/// * `archive_path` - Output path for the ZIP file. Parent directory must exist.
/// * `archive_prefix` - Directory prefix inside the archive.
///   E.g., `"wp-rocket"` → entries become `wp-rocket/file.php`.
/// * `exclusions` - Name patterns to exclude from the archive.
///
/// # Returns
///
/// The number of entries (files + directories) written to the archive.
///
/// # Errors
///
/// Returns [`Error::Build`] if:
/// - The output file cannot be created
/// - The content directory cannot be walked
/// - A file cannot be read or written to the archive
/// - The archive cannot be finalized
pub fn create_zip_archive(
    content_dir: &Path,
    archive_path: &Path,
    archive_prefix: &str,
    exclusions: &[ExclusionPattern<'_>],
) -> Result<u64> {
    let mut entries_written: u64 = 0;

    let file = std::fs::File::create(archive_path).map_err(|e| {
        Error::Build(format!(
            "Failed to create archive file '{}': {e}",
            archive_path.display()
        ))
    })?;

    let mut zip_writer = zip::ZipWriter::new(file);

    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    // `zip -r <archive> <prefix_dir>` records the prefix directory itself as the
    // archive's first entry. Emit it too, so both build paths agree entry-for-entry.
    zip_writer
        .add_directory(format!("{archive_prefix}/"), options)
        .map_err(|e| {
            Error::Build(format!(
                "Failed to add archive root '{archive_prefix}/' to archive: {e}"
            ))
        })?;
    entries_written += 1;

    // filter_entry prunes excluded directories, so their subtrees never reach the
    // archive. The root (depth 0) is always retained — it is the content dir
    // itself, already written above as the archive root.
    let walker = WalkDir::new(content_dir).into_iter().filter_entry(|entry| {
        entry.depth() == 0
            || !matches_exclusion_at_depth(
                &entry.file_name().to_string_lossy(),
                entry.depth(),
                exclusions,
            )
    });

    for entry in walker {
        let entry = entry.map_err(|e| {
            Error::Build(format!(
                "Failed to walk content directory '{}': {e}",
                content_dir.display()
            ))
        })?;

        // Compute path relative to content_dir
        let relative = entry.path().strip_prefix(content_dir).map_err(|e| {
            Error::Build(format!(
                "Failed to compute relative archive path for '{}': {e}",
                entry.path().display()
            ))
        })?;

        // Skip root entry
        if relative.as_os_str().is_empty() {
            continue;
        }

        // Build the zip-internal path: prefix/relative (always forward slashes)
        let relative_str = relative.to_string_lossy().replace('\\', "/");
        let zip_path = format!("{archive_prefix}/{relative_str}");

        if entry.file_type().is_dir() {
            // ZIP spec requires trailing slash for directories.
            // Source: https://docs.rs/zip/8/zip/write/struct.ZipWriter.html#method.add_directory
            zip_writer
                .add_directory(format!("{zip_path}/"), options)
                .map_err(|e| {
                    Error::Build(format!(
                        "Failed to add directory '{zip_path}' to archive: {e}"
                    ))
                })?;
            entries_written += 1;
        } else if entry.file_type().is_file() {
            zip_writer.start_file(&zip_path, options).map_err(|e| {
                Error::Build(format!("Failed to start file '{zip_path}' in archive: {e}"))
            })?;

            let mut source_file = std::fs::File::open(entry.path()).map_err(|e| {
                Error::Build(format!(
                    "Failed to open '{}' for archiving: {e}",
                    entry.path().display()
                ))
            })?;

            std::io::copy(&mut source_file, &mut zip_writer).map_err(|e| {
                Error::Build(format!(
                    "Failed to write '{}' to archive: {e}",
                    entry.path().display()
                ))
            })?;

            entries_written += 1;
        }
        // Symlinks and other file types are intentionally skipped.
    }

    zip_writer.finish().map_err(|e| {
        Error::Build(format!(
            "Failed to finalize archive '{}': {e}",
            archive_path.display()
        ))
    })?;

    Ok(entries_written)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // =========================================================================
    // ExclusionPattern / matches_any_exclusion
    // =========================================================================

    #[test]
    fn test_matches_prefix_pattern() {
        let patterns = &[ExclusionPattern::Prefix(".")];
        assert!(matches_any_exclusion(".git", patterns));
        assert!(matches_any_exclusion(".env", patterns));
        assert!(matches_any_exclusion(".gitignore", patterns));
        assert!(!matches_any_exclusion("index.php", patterns));
    }

    #[test]
    fn test_matches_exact_pattern() {
        let patterns = &[ExclusionPattern::Exact("gulpfile.js")];
        assert!(matches_any_exclusion("gulpfile.js", patterns));
        assert!(!matches_any_exclusion("gulpfile.ts", patterns));
        assert!(!matches_any_exclusion("Gulpfile.js", patterns));
    }

    #[test]
    fn test_matches_multiple_patterns() {
        let patterns = &[
            ExclusionPattern::Prefix("."),
            ExclusionPattern::Exact("gulpfile.js"),
            ExclusionPattern::Prefix("package"),
            ExclusionPattern::Prefix("php"),
        ];

        assert!(matches_any_exclusion(".git", patterns));
        assert!(matches_any_exclusion("gulpfile.js", patterns));
        assert!(matches_any_exclusion("package.json", patterns));
        assert!(matches_any_exclusion("package-lock.json", patterns));
        assert!(matches_any_exclusion("phpunit.xml", patterns));
        assert!(matches_any_exclusion("phpcs.xml", patterns));
        assert!(!matches_any_exclusion("index.php", patterns));
        assert!(!matches_any_exclusion("readme.txt", patterns));
    }

    #[test]
    fn test_matches_empty_patterns() {
        let patterns: &[ExclusionPattern<'_>] = &[];
        assert!(!matches_any_exclusion("anything", patterns));
    }

    #[test]
    fn test_matches_empty_filename() {
        let patterns = &[ExclusionPattern::Prefix(".")];
        assert!(!matches_any_exclusion("", patterns));
    }

    #[test]
    fn test_root_prefix_is_never_matched_without_a_depth() {
        // matches_any_exclusion has no depth to evaluate, so the depth-sensitive
        // variant must not match rather than guess.
        let patterns = &[ExclusionPattern::RootPrefix("php")];
        assert!(!matches_any_exclusion("phpcs.xml", patterns));
    }

    // =========================================================================
    // matches_exclusion_at_depth
    // =========================================================================

    #[test]
    fn test_root_prefix_matches_only_root_children() {
        let patterns = &[ExclusionPattern::RootPrefix("php")];

        // Depth 1 == directly inside the archive root.
        assert!(matches_exclusion_at_depth("phpcs.xml", 1, patterns));
        assert!(matches_exclusion_at_depth("phpstan.neon.dist", 1, patterns));

        // The root itself and anything nested is untouched.
        assert!(!matches_exclusion_at_depth("php-mcp-schema", 0, patterns));
        assert!(!matches_exclusion_at_depth("php-mcp-schema", 3, patterns));
        assert!(!matches_exclusion_at_depth("phpunit.xml", 2, patterns));
    }

    #[test]
    fn test_depth_independent_patterns_match_at_any_depth() {
        let patterns = &[
            ExclusionPattern::Prefix("."),
            ExclusionPattern::Exact("gulpfile.js"),
        ];

        for depth in [1usize, 2, 5] {
            assert!(matches_exclusion_at_depth(".gitignore", depth, patterns));
            assert!(matches_exclusion_at_depth("gulpfile.js", depth, patterns));
            assert!(!matches_exclusion_at_depth("index.php", depth, patterns));
        }
    }

    // =========================================================================
    // copy_dir_with_exclusions
    // =========================================================================

    #[test]
    fn test_copy_dir_basic() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();

        // Create source structure
        std::fs::write(src.path().join("file.txt"), "hello").unwrap();
        std::fs::create_dir_all(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/nested.txt"), "world").unwrap();

        let dst_path = dst.path().join("output");
        let count = copy_dir_with_exclusions(src.path(), &dst_path, &[]).unwrap();

        assert_eq!(count, 2);
        assert_eq!(
            std::fs::read_to_string(dst_path.join("file.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst_path.join("sub/nested.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn test_copy_dir_with_exclusions_skips_dirs() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();

        // Create source with excluded directory
        std::fs::write(src.path().join("keep.txt"), "keep").unwrap();
        std::fs::create_dir_all(src.path().join("node_modules")).unwrap();
        std::fs::write(src.path().join("node_modules/package.json"), "excluded").unwrap();
        std::fs::create_dir_all(src.path().join(".git")).unwrap();
        std::fs::write(src.path().join(".git/HEAD"), "ref").unwrap();

        let dst_path = dst.path().join("output");
        let count =
            copy_dir_with_exclusions(src.path(), &dst_path, &["node_modules", ".git"]).unwrap();

        assert_eq!(count, 1);
        assert!(dst_path.join("keep.txt").exists());
        assert!(!dst_path.join("node_modules").exists());
        assert!(!dst_path.join(".git").exists());
    }

    #[test]
    fn test_copy_dir_preserves_nested_structure() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();

        std::fs::create_dir_all(src.path().join("a/b/c")).unwrap();
        std::fs::write(src.path().join("a/b/c/deep.txt"), "deep").unwrap();

        let dst_path = dst.path().join("output");
        let count = copy_dir_with_exclusions(src.path(), &dst_path, &[]).unwrap();

        assert_eq!(count, 1);
        assert_eq!(
            std::fs::read_to_string(dst_path.join("a/b/c/deep.txt")).unwrap(),
            "deep"
        );
    }

    #[test]
    fn test_copy_dir_empty_source() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();

        let dst_path = dst.path().join("output");
        let count = copy_dir_with_exclusions(src.path(), &dst_path, &[]).unwrap();

        assert_eq!(count, 0);
        assert!(dst_path.exists());
    }

    // =========================================================================
    // create_zip_archive
    // =========================================================================

    #[test]
    fn test_create_zip_basic() {
        let dir = TempDir::new().unwrap();
        let content = dir.path().join("content");
        std::fs::create_dir_all(&content).unwrap();
        std::fs::write(content.join("file.txt"), "hello zip").unwrap();
        std::fs::create_dir_all(content.join("sub")).unwrap();
        std::fs::write(content.join("sub/nested.txt"), "nested").unwrap();

        let archive_path = dir.path().join("output.zip");
        let count = create_zip_archive(&content, &archive_path, "my-plugin", &[]).unwrap();

        // root ("my-plugin/") + 1 dir ("sub") + 2 files
        assert_eq!(count, 4);
        assert!(archive_path.exists());

        // Verify archive contents
        let file = std::fs::File::open(&archive_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();

        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();

        assert!(
            names.contains(&"my-plugin/".to_string()),
            "archive root entry"
        );
        assert!(names.contains(&"my-plugin/file.txt".to_string()));
        assert!(names.contains(&"my-plugin/sub/".to_string()));
        assert!(names.contains(&"my-plugin/sub/nested.txt".to_string()));

        // Verify file content
        let mut file_entry = archive.by_name("my-plugin/file.txt").unwrap();
        let mut content_str = String::new();
        std::io::Read::read_to_string(&mut file_entry, &mut content_str).unwrap();
        assert_eq!(content_str, "hello zip");
    }

    /// Read every entry name out of a zip archive.
    fn archive_entry_names(archive_path: &Path) -> Vec<String> {
        let file = std::fs::File::open(archive_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    #[test]
    fn test_create_zip_with_exclusions() {
        let dir = TempDir::new().unwrap();
        let content = dir.path().join("content");
        std::fs::create_dir_all(&content).unwrap();
        std::fs::write(content.join("keep.php"), "<?php").unwrap();
        std::fs::write(content.join(".hidden"), "secret").unwrap();
        std::fs::write(content.join("gulpfile.js"), "gulp").unwrap();
        std::fs::write(content.join("package.json"), "{}").unwrap();
        std::fs::write(content.join("phpunit.xml"), "<xml>").unwrap();

        let exclusions = &[
            ExclusionPattern::Prefix("."),
            ExclusionPattern::Exact("gulpfile.js"),
            ExclusionPattern::RootPrefix("package"),
            ExclusionPattern::RootPrefix("php"),
        ];

        let archive_path = dir.path().join("output.zip");
        let count = create_zip_archive(&content, &archive_path, "plugin", exclusions).unwrap();

        assert_eq!(count, 2); // archive root + keep.php

        let names = archive_entry_names(&archive_path);

        assert!(names.contains(&"plugin/keep.php".to_string()));
        assert!(!names.iter().any(|n| n.contains(".hidden")));
        assert!(!names.iter().any(|n| n.contains("gulpfile")));
        assert!(!names.iter().any(|n| n.contains("package")));
        assert!(!names.iter().any(|n| n.contains("phpunit")));
    }

    #[test]
    fn test_create_zip_prunes_excluded_directory_subtrees() {
        let dir = TempDir::new().unwrap();
        let content = dir.path().join("content");
        std::fs::create_dir_all(content.join(".git/objects")).unwrap();
        std::fs::write(content.join(".git/objects/blob"), "obj").unwrap();
        std::fs::write(content.join("keep.php"), "<?php").unwrap();

        let exclusions = &[ExclusionPattern::Prefix(".")];
        let archive_path = dir.path().join("output.zip");
        let count = create_zip_archive(&content, &archive_path, "plugin", exclusions).unwrap();

        // Archive root + keep.php — the excluded directory's whole subtree is
        // pruned, matching `zip -x "*/.*"` where `*` also spans `/`.
        assert_eq!(count, 2);
        let names = archive_entry_names(&archive_path);
        assert_eq!(
            names,
            vec!["plugin/".to_string(), "plugin/keep.php".to_string()]
        );
    }

    /// Regression: a root-anchored `php` rule must not strip nested vendor
    /// packages whose directory name merely starts with `php`. Dropping
    /// `vendor/wordpress/php-mcp-schema/` left `wordpress/mcp-adapter` with an
    /// unloadable `WP\McpSchema\…` namespace and a fatal error at plugin load.
    #[test]
    fn test_create_zip_keeps_nested_vendor_package_sharing_root_prefix() {
        let dir = TempDir::new().unwrap();
        let content = dir.path().join("content");
        let schema_dto = content.join("vendor/wordpress/php-mcp-schema/src/Server/Tools/DTO");
        std::fs::create_dir_all(&schema_dto).unwrap();
        std::fs::write(schema_dto.join("Tool.php"), "<?php").unwrap();
        std::fs::write(content.join("phpcs.xml"), "<xml>").unwrap();
        std::fs::write(content.join("wp-rocket.php"), "<?php").unwrap();

        let exclusions = &[ExclusionPattern::RootPrefix("php")];
        let archive_path = dir.path().join("output.zip");
        create_zip_archive(&content, &archive_path, "wp-rocket", exclusions).unwrap();

        let names = archive_entry_names(&archive_path);

        assert!(
            names.contains(
                &"wp-rocket/vendor/wordpress/php-mcp-schema/src/Server/Tools/DTO/Tool.php"
                    .to_string()
            ),
            "nested vendor class must ship: {names:?}"
        );
        assert!(
            names.contains(&"wp-rocket/wp-rocket.php".to_string()),
            "plugin entry point must ship: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.ends_with("phpcs.xml")),
            "root tooling config must still be excluded: {names:?}"
        );
    }

    #[test]
    fn test_create_zip_archive_prefix() {
        let dir = TempDir::new().unwrap();
        let content = dir.path().join("content");
        std::fs::create_dir_all(&content).unwrap();
        std::fs::write(content.join("test.txt"), "data").unwrap();

        let archive_path = dir.path().join("output.zip");
        create_zip_archive(&content, &archive_path, "my-prefix", &[]).unwrap();

        let file = std::fs::File::open(&archive_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();

        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();

        assert!(
            names.iter().all(|n| n.starts_with("my-prefix/")),
            "All entries should be prefixed with 'my-prefix/': {:?}",
            names
        );
    }

    #[test]
    fn test_create_zip_empty_dir() {
        let dir = TempDir::new().unwrap();
        let content = dir.path().join("content");
        std::fs::create_dir_all(&content).unwrap();

        let archive_path = dir.path().join("output.zip");
        let count = create_zip_archive(&content, &archive_path, "empty", &[]).unwrap();

        // The archive root entry is still recorded, as `zip -r` does.
        assert_eq!(count, 1);
        assert!(archive_path.exists());
        assert_eq!(
            archive_entry_names(&archive_path),
            vec!["empty/".to_string()]
        );
    }

    #[test]
    fn test_create_zip_missing_content_dir_errors() {
        let dir = TempDir::new().unwrap();
        let archive_path = dir.path().join("output.zip");

        let result = create_zip_archive(
            &dir.path().join("does-not-exist"),
            &archive_path,
            "plugin",
            &[],
        );

        assert!(result.is_err(), "walking a missing directory must error");
        assert!(
            result.unwrap_err().to_string().contains("does-not-exist"),
            "error should name the unreadable directory"
        );
    }
}
