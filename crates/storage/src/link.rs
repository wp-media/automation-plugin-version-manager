//! Cross-platform directory linking.
//!
//! Provides a unified interface for creating directory links:
//! - **Unix (macOS, Linux)**: symbolic links (`std::os::unix::fs::symlink`)
//! - **Windows**: NTFS junctions via the `junction` crate — junctions work
//!   without admin privileges or Developer Mode (unlike symlinks) and are
//!   created through the Win32 API directly, avoiding the quoting and
//!   injection hazards of shelling out to `cmd /C mklink`.

use std::path::Path;

use crate::error::{Error, IoResultExt, Result};

/// Create a directory link (symlink on Unix, junction on Windows).
///
/// If something already exists at `link` (a previous link, an empty
/// directory, or a stray file) it is removed first, so the operation is
/// idempotent. Parent directories are created as needed.
///
/// # Arguments
///
/// * `target` - The directory the link should point to. On Unix this may be
///   relative to the link's parent directory (preferred: keeps the store
///   relocatable). On Windows, junctions require absolute targets; relative
///   targets are resolved against the link's parent.
/// * `link` - The path where the link will be created.
pub fn create_dir_link(target: &Path, link: &Path) -> Result<()> {
    // Remove whatever occupies the link path (idempotent re-link).
    remove_link_path(link)?;

    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .io_ctx(|| format!("creating link parent directory {}", parent.display()))?;
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).io_ctx(|| {
            format!(
                "creating symlink {} -> {}",
                link.display(),
                target.display()
            )
        })?;
    }

    #[cfg(windows)]
    {
        create_junction(target, link)?;
    }

    Ok(())
}

/// Remove a directory link. Missing paths are a no-op.
///
/// Never follows the link: only the link itself is removed, the target is
/// untouched.
pub fn remove_dir_link(link: &Path) -> Result<()> {
    remove_link_path(link)
}

/// Remove whatever exists at `path` without following links:
/// links are unlinked, empty directories removed, stray files deleted.
fn remove_link_path(path: &Path) -> Result<()> {
    // symlink_metadata never follows links; NotFound means nothing to do.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io(format!("inspecting {}", path.display()), e)),
    };

    if is_link(path) {
        // Junctions and directory symlinks on Windows are removed with
        // remove_dir; Unix symlinks with remove_file.
        #[cfg(unix)]
        std::fs::remove_file(path).io_ctx(|| format!("removing link {}", path.display()))?;
        #[cfg(windows)]
        std::fs::remove_dir(path)
            .or_else(|_| std::fs::remove_file(path))
            .io_ctx(|| format!("removing link {}", path.display()))?;
    } else if meta.is_dir() {
        // Only empty real directories are removed — refusing to delete a
        // populated directory protects against destroying artifacts when a
        // link path is unexpectedly occupied.
        std::fs::remove_dir(path)
            .io_ctx(|| format!("removing directory occupying link path {}", path.display()))?;
    } else {
        std::fs::remove_file(path)
            .io_ctx(|| format!("removing file occupying link path {}", path.display()))?;
    }

    Ok(())
}

/// Check if a path is a link (symlink or junction) without following it.
pub fn is_link(path: &Path) -> bool {
    #[cfg(unix)]
    {
        path.is_symlink()
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        // FILE_ATTRIBUTE_REPARSE_POINT (0x400) covers both symlinks and
        // junctions (mount points).
        path.symlink_metadata()
            .map(|m| m.file_attributes() & 0x400 != 0)
            .unwrap_or(false)
    }
}

/// Read the target of a link.
///
/// On Unix returns the symlink target verbatim (possibly relative). On
/// Windows `std::fs::read_link` resolves junction reparse data to the
/// absolute target.
pub fn read_link(link: &Path) -> Result<std::path::PathBuf> {
    std::fs::read_link(link).io_ctx(|| format!("reading link target of {}", link.display()))
}

/// Create a junction on Windows via the Win32 API (no subprocess).
#[cfg(windows)]
fn create_junction(target: &Path, link: &Path) -> Result<()> {
    // Junctions require absolute targets; resolve relative ones against the
    // link's parent directory.
    let target_abs = if target.is_absolute() {
        target.to_path_buf()
    } else {
        let base = link.parent().unwrap_or(Path::new("."));
        base.join(target)
            .canonicalize()
            .io_ctx(|| format!("resolving junction target {}", target.display()))?
    };

    junction::create(&target_abs, link).map_err(|e| {
        Error::Link(format!(
            "failed to create junction {} -> {}: {e}",
            link.display(),
            target_abs.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_create_and_read_dir_link() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target_dir");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("file.txt"), "hello").unwrap();

        let link = dir.path().join("link_dir");
        create_dir_link(&target, &link).unwrap();

        assert!(is_link(&link));
        let content = std::fs::read_to_string(link.join("file.txt")).unwrap();
        assert_eq!(content, "hello");
    }

    #[test]
    fn test_is_link_regular_dir() {
        let dir = TempDir::new().unwrap();
        let regular = dir.path().join("regular");
        std::fs::create_dir(&regular).unwrap();
        assert!(!is_link(&regular));
    }

    #[test]
    fn test_is_link_nonexistent() {
        assert!(!is_link(Path::new("/nonexistent/path")));
    }

    #[test]
    fn test_remove_dir_link() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();

        let link = dir.path().join("link");
        create_dir_link(&target, &link).unwrap();
        assert!(is_link(&link));

        remove_dir_link(&link).unwrap();
        assert!(!is_link(&link));
        // Target untouched.
        assert!(target.exists());
    }

    #[test]
    fn test_remove_nonexistent_link_is_noop() {
        let dir = TempDir::new().unwrap();
        remove_dir_link(&dir.path().join("no_such_link")).unwrap();
    }

    #[test]
    fn test_overwrite_existing_link() {
        let dir = TempDir::new().unwrap();
        let target1 = dir.path().join("target1");
        std::fs::create_dir(&target1).unwrap();
        std::fs::write(target1.join("id.txt"), "one").unwrap();

        let target2 = dir.path().join("target2");
        std::fs::create_dir(&target2).unwrap();
        std::fs::write(target2.join("id.txt"), "two").unwrap();

        let link = dir.path().join("link");
        create_dir_link(&target1, &link).unwrap();
        create_dir_link(&target2, &link).unwrap();

        let content = std::fs::read_to_string(link.join("id.txt")).unwrap();
        assert_eq!(content, "two");
    }

    #[test]
    fn test_create_link_replaces_stray_file() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();

        // A regular file occupies the link path (e.g. leftover from a bug or
        // manual tampering) — create_dir_link must recover.
        let link = dir.path().join("link");
        std::fs::write(&link, "stray").unwrap();

        create_dir_link(&target, &link).unwrap();
        assert!(is_link(&link));
    }

    #[test]
    fn test_create_link_refuses_populated_directory() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();

        // A real directory with content occupies the link path: creating the
        // link must fail rather than destroy data.
        let link = dir.path().join("link");
        std::fs::create_dir(&link).unwrap();
        std::fs::write(link.join("precious.txt"), "data").unwrap();

        assert!(create_dir_link(&target, &link).is_err());
        assert!(link.join("precious.txt").exists());
    }

    #[test]
    fn test_read_link() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();

        let link = dir.path().join("link");
        create_dir_link(&target, &link).unwrap();

        let resolved = read_link(&link).unwrap();
        // On Windows read_link returns the absolute junction target; on Unix
        // the verbatim symlink target (absolute here since we linked absolute).
        assert!(resolved.ends_with("target"));
    }

    #[test]
    fn test_read_link_nonexistent() {
        assert!(read_link(Path::new("/nonexistent/link")).is_err());
    }

    #[test]
    fn test_create_link_creates_parents() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();

        let link = dir.path().join("a").join("b").join("c").join("link");
        create_dir_link(&target, &link).unwrap();
        assert!(is_link(&link));
    }

    #[cfg(unix)]
    #[test]
    fn test_relative_symlink() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("real_dir");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("data.txt"), "payload").unwrap();

        let link = dir.path().join("rel_link");
        create_dir_link(Path::new("real_dir"), &link).unwrap();

        assert!(is_link(&link));
        let content = std::fs::read_to_string(link.join("data.txt")).unwrap();
        assert_eq!(content, "payload");
    }
}
