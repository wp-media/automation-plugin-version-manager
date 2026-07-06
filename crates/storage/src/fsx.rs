//! Filesystem primitives: crash-safe copies with streaming SHA-256, size
//! probes, and directory cleanup helpers.
//!
//! # Atomicity
//!
//! Files enter the store via a temp file created **in the destination
//! directory** (same filesystem, so the final rename is atomic). The temp
//! file is fsynced before the rename, so under a stored filename there is
//! only ever a complete, durable file — a crash can at worst leave a
//! `.apvm-tmp-*` leftover, which `gc()` sweeps.

use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::error::{Error, IoContext, Result};
use crate::paths::TMP_PREFIX;

/// Copy buffer size: 64 KiB balances syscall count and memory.
const COPY_BUF_SIZE: usize = 64 * 1024;

/// Size and content hash of a stored file.
#[derive(Debug, Clone)]
pub(crate) struct FileDigest {
    /// File size in bytes.
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 of the file content.
    pub sha256: String,
}

/// Lowercase hex encoding of a byte slice.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Copy `src` to `dest` atomically, hashing the content on the way.
///
/// Streams through a temp file next to `dest`, fsyncs it, then renames it
/// over `dest` (replacing any previous file). Returns the copied size and
/// SHA-256.
pub(crate) fn copy_file_hashed(src: &Path, dest: &Path) -> Result<FileDigest> {
    let dest_dir = dest.parent().ok_or_else(|| Error::Data {
        details: format!("destination '{}' has no parent directory", dest.display()),
    })?;
    fs::create_dir_all(dest_dir)
        .io_ctx(|| format!("failed to create directory {}", dest_dir.display()))?;

    let mut reader = BufReader::new(
        File::open(src).io_ctx(|| format!("failed to open source file {}", src.display()))?,
    );
    let mut tmp = tempfile::Builder::new()
        .prefix(TMP_PREFIX)
        .tempfile_in(dest_dir)
        .io_ctx(|| format!("failed to create temp file in {}", dest_dir.display()))?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let mut size_bytes: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .io_ctx(|| format!("failed to read {}", src.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        tmp.as_file_mut()
            .write_all(&buf[..n])
            .io_ctx(|| format!("failed to write temp file for {}", dest.display()))?;
        size_bytes += n as u64;
    }

    tmp.as_file()
        .sync_all()
        .io_ctx(|| format!("failed to sync temp file for {}", dest.display()))?;
    tmp.persist(dest).map_err(|persist_err| Error::Io {
        context: format!("failed to move temp file into place at {}", dest.display()),
        source: persist_err.error,
    })?;
    sync_dir(dest_dir)?;

    Ok(FileDigest {
        size_bytes,
        sha256: to_hex(&hasher.finalize()),
    })
}

/// Compute size and SHA-256 of an existing file by streaming it.
pub(crate) fn hash_file(path: &Path) -> Result<FileDigest> {
    let mut reader =
        BufReader::new(File::open(path).io_ctx(|| format!("failed to open {}", path.display()))?);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let mut size_bytes: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .io_ctx(|| format!("failed to read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size_bytes += n as u64;
    }
    Ok(FileDigest {
        size_bytes,
        sha256: to_hex(&hasher.finalize()),
    })
}

/// Size of `path` if it exists and is a regular file, else `None`.
pub(crate) fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|meta| meta.len())
}

/// Remove a directory tree; a missing directory is success.
pub(crate) fn remove_dir_all_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::Io {
            context: format!("failed to remove directory {}", path.display()),
            source: err,
        }),
    }
}

/// Walk upward from `start`, removing empty directories, stopping at (and
/// never removing) `stop`. Non-empty directories end the walk silently.
pub(crate) fn remove_empty_parents(start: &Path, stop: &Path) {
    let mut current: Option<&Path> = Some(start);
    while let Some(dir) = current {
        if dir == stop || !dir.starts_with(stop) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break; // not empty, already gone, or not removable — all fine
        }
        current = dir.parent();
    }
}

/// Best-effort recursive size of a directory tree in bytes (used to report
/// how much space removing an orphan directory reclaims).
pub(crate) fn dir_size_recursive(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// Remove `.apvm-tmp-*` files in `dir` older than `max_age`. Returns how
/// many were removed. Fresh temp files are left alone — they may belong to
/// a concurrent writer.
pub(crate) fn remove_stale_temp_files(dir: &Path, max_age: Duration) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(TMP_PREFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if stale && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Flush directory metadata so a just-renamed file survives power loss.
/// Directory handles cannot be fsynced on Windows; the rename itself is
/// still atomic there.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .io_ctx(|| format!("failed to sync directory {}", dir.display()))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_hex_encodes_lowercase() {
        assert_eq!(to_hex(&[0x00, 0xff, 0x1a]), "00ff1a");
    }

    #[test]
    fn copy_hashes_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"hello").unwrap();
        let dest = dir.path().join("out/artifact.zip");

        let digest = copy_file_hashed(&src, &dest).unwrap();
        assert_eq!(digest.size_bytes, 5);
        // SHA-256 of "hello" — well-known vector.
        assert_eq!(
            digest.sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");

        // Overwrite works and no temp files remain.
        std::fs::write(&src, b"world!").unwrap();
        let digest = copy_file_hashed(&src, &dest).unwrap();
        assert_eq!(digest.size_bytes, 6);
        let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(TMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn stale_temp_files_are_swept_fresh_ones_kept() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join(format!("{TMP_PREFIX}stale"));
        std::fs::write(&stale, b"x").unwrap();
        // max_age zero ⇒ everything qualifies as stale.
        assert_eq!(remove_stale_temp_files(dir.path(), Duration::ZERO), 1);
        assert!(!stale.exists());

        let fresh = dir.path().join(format!("{TMP_PREFIX}fresh"));
        std::fs::write(&fresh, b"x").unwrap();
        assert_eq!(
            remove_stale_temp_files(dir.path(), Duration::from_secs(3600)),
            0
        );
        assert!(fresh.exists());
    }

    #[test]
    fn remove_empty_parents_stops_at_base() {
        let base = tempfile::tempdir().unwrap();
        let deep = base.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        remove_empty_parents(&deep, base.path());
        assert!(!base.path().join("a").exists());
        assert!(base.path().exists());
    }
}
