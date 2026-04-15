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
