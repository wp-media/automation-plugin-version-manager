//! Cross-platform directory linking.
//!
//! Provides a unified interface for creating directory links:
//! - **Unix (macOS, Linux)**: Symbolic links
//! - **Windows**: Junctions (no admin privileges required)

use std::path::Path;

use crate::error::{Error, Result};

/// Create a directory link (symlink on Unix, junction on Windows).
///
/// # Arguments
///
/// * `target` - The directory the link should point to
/// * `link` - The path where the link will be created
///
/// # Platform Behavior
///
/// - **Unix**: Creates a symbolic link using `std::os::unix::fs::symlink`
/// - **Windows**: Creates a junction using `mklink /J`
pub fn create_dir_link(target: &Path, link: &Path) -> Result<()> {
    // Remove existing link if present
    if link.exists() || is_link(link) {
        remove_dir_link(link)?;
    }

    // Create parent directory if needed
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)?;
    }

    #[cfg(windows)]
    {
        create_junction(target, link)?;
    }

    Ok(())
}

/// Remove a directory link.
pub fn remove_dir_link(link: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        if link.is_symlink() {
            std::fs::remove_file(link)?;
        } else if link.is_dir() {
            std::fs::remove_dir(link)?;
        }
    }

    #[cfg(windows)]
    {
        // Junctions are removed as directories
        if link.exists() || is_link(link) {
            // Use remove_dir for junctions
            std::fs::remove_dir(link).or_else(|_| std::fs::remove_file(link))?;
        }
    }

    Ok(())
}

/// Check if a path is a link (symlink or junction).
pub fn is_link(path: &Path) -> bool {
    #[cfg(unix)]
    {
        path.is_symlink()
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        path.symlink_metadata()
            .map(|m| {
                // FILE_ATTRIBUTE_REPARSE_POINT = 0x400
                m.file_attributes() & 0x400 != 0
            })
            .unwrap_or(false)
    }
}

/// Read the target of a link.
pub fn read_link(link: &Path) -> Result<std::path::PathBuf> {
    std::fs::read_link(link).map_err(Error::Io)
}

/// Create a junction on Windows.
#[cfg(windows)]
fn create_junction(target: &Path, link: &Path) -> Result<()> {
    use std::process::Command;

    // Junctions require absolute paths
    let target_abs = if target.is_absolute() {
        target.to_path_buf()
    } else {
        // Resolve relative path from link's parent directory
        link.parent()
            .unwrap_or(Path::new("."))
            .join(target)
            .canonicalize()
            .map_err(Error::Io)?
    };

    // Create junction using mklink /J
    let output = Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &link.to_string_lossy(),
            &target_abs.to_string_lossy(),
        ])
        .output()?;

    if !output.status.success() {
        return Err(Error::Link(format!(
            "Failed to create junction: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // =========================================================================
    // 4.1 – Create and Read Link
    // =========================================================================

    #[test]
    fn test_create_and_read_dir_link() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target_dir");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("file.txt"), "hello").unwrap();

        let link = dir.path().join("link_dir");
        create_dir_link(&target, &link).unwrap();

        assert!(is_link(&link));
        // Content accessible through link
        let content = std::fs::read_to_string(link.join("file.txt")).unwrap();
        assert_eq!(content, "hello");
    }

    // =========================================================================
    // 4.2 – is_link
    // =========================================================================

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

    // =========================================================================
    // 4.3 – Remove Link
    // =========================================================================

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
        // Target still exists
        assert!(target.exists());
    }

    // =========================================================================
    // 4.4 – Remove Nonexistent Link (no-op)
    // =========================================================================

    #[test]
    fn test_remove_nonexistent_link() {
        let dir = TempDir::new().unwrap();
        let link = dir.path().join("no_such_link");
        // Should not error
        remove_dir_link(&link).unwrap();
    }

    // =========================================================================
    // 4.5 – Overwrite Existing Link
    // =========================================================================

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

        // Overwrite with new target
        create_dir_link(&target2, &link).unwrap();

        let content = std::fs::read_to_string(link.join("id.txt")).unwrap();
        assert_eq!(content, "two");
    }

    // =========================================================================
    // 4.6 – read_link
    // =========================================================================

    #[test]
    fn test_read_link() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();

        let link = dir.path().join("link");
        create_dir_link(&target, &link).unwrap();

        let resolved = read_link(&link).unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn test_read_link_nonexistent() {
        let result = read_link(Path::new("/nonexistent/link"));
        assert!(result.is_err());
    }

    // =========================================================================
    // 4.7 – Creates Parent Directories
    // =========================================================================

    #[test]
    fn test_create_link_creates_parents() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();

        let link = dir.path().join("a").join("b").join("c").join("link");
        create_dir_link(&target, &link).unwrap();
        assert!(is_link(&link));
    }

    // =========================================================================
    // 4.8 – Relative Symlinks (Unix only)
    // =========================================================================

    #[cfg(unix)]
    #[test]
    fn test_relative_symlink() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("real_dir");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("data.txt"), "payload").unwrap();

        // Create a relative symlink manually
        let link = dir.path().join("rel_link");
        std::os::unix::fs::symlink(Path::new("real_dir"), &link).unwrap();

        assert!(is_link(&link));
        let content = std::fs::read_to_string(link.join("data.txt")).unwrap();
        assert_eq!(content, "payload");
    }
}
