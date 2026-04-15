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
//!   filename-based exclusions (replaces `zip -r ... -x`)
//! - [`matches_any_exclusion`] — check if a filename matches any of the
//!   given [`ExclusionPattern`]s

use std::path::Path;

use walkdir::WalkDir;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::error::{Error, Result};

// =============================================================================
// Exclusion Patterns
// =============================================================================

/// A filename exclusion pattern for filtering files during copy or archive operations.
///
/// These patterns match against the **filename component only** (not the full path),
/// which mirrors how `rsync --exclude` and `zip -x` work with `*/pattern` syntax.
///
/// # Examples
///
/// ```ignore
/// use apvm_core::build::fs::ExclusionPattern;
///
/// // Match dotfiles (e.g., .git, .env, .gitignore)
/// let dotfiles = ExclusionPattern::Prefix(".");
///
/// // Match exact filename
/// let gulpfile = ExclusionPattern::Exact("gulpfile.js");
///
/// // Match filename prefix (e.g., package.json, package-lock.json)
/// let packages = ExclusionPattern::Prefix("package");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusionPattern<'a> {
    /// Match files whose name starts with the given prefix.
    ///
    /// Example: `Prefix("package")` matches `package.json`, `package-lock.json`.
    /// Example: `Prefix(".")` matches `.git`, `.env`, `.gitignore`.
    Prefix(&'a str),

    /// Match files whose name equals the given string exactly.
    ///
    /// Example: `Exact("gulpfile.js")` matches only `gulpfile.js`.
    Exact(&'a str),
}

/// Check if a filename matches any of the given exclusion patterns.
///
/// # Arguments
///
/// * `filename` - The filename (not full path) to check.
/// * `patterns` - Slice of [`ExclusionPattern`]s to match against.
///
/// # Returns
///
/// `true` if `filename` matches at least one pattern.
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
    patterns.iter().any(|pattern| match pattern {
        ExclusionPattern::Prefix(prefix) => filename.starts_with(prefix),
        ExclusionPattern::Exact(name) => filename == *name,
    })
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
/// - Uses [`WalkDir::filter_entry`] to prevent **descending** into excluded
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

/// Create a ZIP archive from a directory's contents, with filename-based exclusions.
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
/// - Each entry is stored in the archive under `archive_prefix/relative/path`.
/// - Filenames matching any `exclusions` pattern are skipped.
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
/// * `exclusions` - Filename patterns to exclude from the archive.
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

    for entry in WalkDir::new(content_dir) {
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

        // Check exclusions against the filename component
        let filename = entry.file_name().to_string_lossy();
        if matches_any_exclusion(&filename, exclusions) {
            continue;
        }

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

        // 1 dir ("sub") + 2 files ("file.txt", "sub/nested.txt")
        assert_eq!(count, 3);
        assert!(archive_path.exists());

        // Verify archive contents
        let file = std::fs::File::open(&archive_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();

        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();

        assert!(names.contains(&"my-plugin/file.txt".to_string()));
        assert!(names.contains(&"my-plugin/sub/".to_string()));
        assert!(names.contains(&"my-plugin/sub/nested.txt".to_string()));

        // Verify file content
        let mut file_entry = archive.by_name("my-plugin/file.txt").unwrap();
        let mut content_str = String::new();
        std::io::Read::read_to_string(&mut file_entry, &mut content_str).unwrap();
        assert_eq!(content_str, "hello zip");
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
            ExclusionPattern::Prefix("package"),
            ExclusionPattern::Prefix("php"),
        ];

        let archive_path = dir.path().join("output.zip");
        let count = create_zip_archive(&content, &archive_path, "plugin", exclusions).unwrap();

        assert_eq!(count, 1); // Only keep.php

        let file = std::fs::File::open(&archive_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();

        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();

        assert!(names.contains(&"plugin/keep.php".to_string()));
        assert!(!names.iter().any(|n| n.contains(".hidden")));
        assert!(!names.iter().any(|n| n.contains("gulpfile")));
        assert!(!names.iter().any(|n| n.contains("package")));
        assert!(!names.iter().any(|n| n.contains("phpunit")));
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

        assert_eq!(count, 0);
        assert!(archive_path.exists());
    }
}
