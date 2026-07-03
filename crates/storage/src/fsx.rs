//! Filesystem utilities: atomic writes, streamed hashing copies, temp-file
//! management, and store-wide locking.
//!
//! # Crash consistency model
//!
//! The store never modifies files in place. Every write (manifest or
//! artifact) goes to a [`tempfile::NamedTempFile`] created *in the
//! destination directory* and is then atomically renamed over the final
//! name via [`NamedTempFile::persist`] (`rename(2)` on Unix,
//! `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` on Windows). Readers therefore
//! see either the old complete file or the new complete file, never a torn
//! one.
//!
//! Using `NamedTempFile` (rather than hand-rolled temp paths) buys three
//! things: collision-safe random names that survive process restarts,
//! automatic removal of the temp file on `Drop` (so every early return
//! cleans up without explicit handling), and a battle-tested cross-platform
//! rename. The temp file keeps [`TMP_PREFIX`] so that, in the rare case a
//! `SIGKILL`/power loss bypasses `Drop`, [`remove_stale_temps`] can still
//! sweep it and [`crate::validate`] can keep artifacts from colliding with it.
//!
//! Temp files are fsynced before the rename so a crash right after the
//! rename cannot leave an empty/partial file behind the final name.
//! Directory fsync after rename is attempted on Unix but treated as
//! best-effort: its failure can only lose the *rename* on power loss, never
//! produce an inconsistent state, and some filesystems reject it.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};
use tempfile::Builder;

use crate::error::{Error, IoResultExt, Result};

/// Prefix for all temporary files created by this crate.
///
/// Applied to every [`tempfile::NamedTempFile`] we create (via
/// [`Builder::prefix`]) so leftover temps from an interrupted operation are
/// recognizable to [`remove_stale_temps`]. Reserved:
/// [`crate::validate::validate_artifact_filename`] rejects artifact names
/// starting with it, so stale-temp cleanup can never delete real data.
pub(crate) const TMP_PREFIX: &str = ".apvm-tmp-";

/// Buffer size for streamed copies/hashing (64 KiB balances syscall count
/// against memory for multi-megabyte plugin zips).
const IO_BUF_SIZE: usize = 64 * 1024;

/// Create a [`NamedTempFile`] in `dir`, prefixed with [`TMP_PREFIX`].
///
/// Same-directory placement is required for the atomic-rename guarantee on
/// `persist` (renames across filesystems are not atomic and can fail
/// outright); `dir` is always the destination file's own parent.
fn new_temp_in(dir: &Path) -> Result<tempfile::NamedTempFile> {
    Builder::new()
        .prefix(TMP_PREFIX)
        .tempfile_in(dir)
        .io_ctx(|| format!("creating temp file in {}", dir.display()))
}

/// Fsync the parent directory of `path` (Unix only, best-effort).
///
/// Persists the rename itself. Failure is only logged: it cannot produce an
/// inconsistent store, and several filesystems (and all of Windows, where
/// directories cannot be opened this way) do not support it.
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        match File::open(parent) {
            Ok(dir) => {
                if let Err(e) = dir.sync_all() {
                    tracing::debug!("directory fsync failed for {}: {e}", parent.display());
                }
            }
            Err(e) => tracing::debug!("could not open {} for fsync: {e}", parent.display()),
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Atomically write `bytes` to `path` (temp file + fsync + rename).
///
/// Creates the parent directory if missing. On any failure `path` is left
/// untouched and the temp file is removed when it drops.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::InvalidInput(format!("path '{}' has no parent", path.display())))?;
    std::fs::create_dir_all(parent)
        .io_ctx(|| format!("creating directory {}", parent.display()))?;

    // The temp file auto-removes on drop, so every `?` below cleans up
    // without explicit handling; only a successful `persist` keeps it.
    let mut temp = new_temp_in(parent)?;

    temp.write_all(bytes)
        .io_ctx(|| format!("writing temp file {}", temp.path().display()))?;
    // Flush file contents to disk before the rename makes it visible.
    temp.as_file()
        .sync_all()
        .io_ctx(|| format!("syncing temp file {}", temp.path().display()))?;

    // Atomic replace of any existing file at `path`.
    temp.persist(path)
        .map_err(|e| Error::io(format!("renaming temp file to {}", path.display()), e.error))?;

    sync_parent_dir(path);
    Ok(())
}

/// Copy `src` to `dest` atomically, computing size and SHA-256 in one pass.
///
/// The hash is computed over the bytes actually written, so a successful
/// return guarantees `dest` exists, is complete, and matches the returned
/// checksum. Returns `(size_bytes, sha256_hex)`.
pub(crate) fn copy_file_atomic_hashed(src: &Path, dest: &Path) -> Result<(u64, String)> {
    let parent = dest
        .parent()
        .ok_or_else(|| Error::InvalidInput(format!("path '{}' has no parent", dest.display())))?;
    std::fs::create_dir_all(parent)
        .io_ctx(|| format!("creating directory {}", parent.display()))?;

    // Open the source before creating the temp file: if the source is
    // missing we bail out having created nothing to clean up.
    let mut reader =
        File::open(src).io_ctx(|| format!("opening source artifact {}", src.display()))?;
    // Auto-removed on drop, so every `?` below cleans up implicitly.
    let mut temp = new_temp_in(parent)?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; IO_BUF_SIZE];
    let mut total: u64 = 0;

    loop {
        let n = reader
            .read(&mut buf)
            .io_ctx(|| format!("reading {}", src.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        temp.write_all(&buf[..n])
            .io_ctx(|| format!("writing {}", temp.path().display()))?;
        total += n as u64;
    }

    temp.as_file()
        .sync_all()
        .io_ctx(|| format!("syncing temp file {}", temp.path().display()))?;

    temp.persist(dest)
        .map_err(|e| Error::io(format!("renaming temp file to {}", dest.display()), e.error))?;

    sync_parent_dir(dest);
    Ok((total, hex_digest(hasher)))
}

/// Compute the SHA-256 of a file with constant memory usage.
pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).io_ctx(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; IO_BUF_SIZE];
    loop {
        let n = file
            .read(&mut buf)
            .io_ctx(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_digest(hasher))
}

/// Finalize a hasher into a lowercase hex string.
fn hex_digest(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Remove leftover temp files from interrupted operations in `dir`.
///
/// Safe by construction: only names starting with [`TMP_PREFIX`] are
/// touched, and that prefix is rejected for artifact filenames. Errors are
/// logged, not propagated — stale temps are garbage, not state.
pub(crate) fn remove_stale_temps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(TMP_PREFIX) {
            let path = entry.path();
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::debug!("could not remove stale temp {}: {e}", path.display());
            } else {
                tracing::debug!("removed stale temp file {}", path.display());
            }
        }
    }
}

// ============================================================================
// Store-wide lock
// ============================================================================

/// Lock file name at the store root.
const LOCK_FILENAME: &str = ".apvm-store.lock";

/// RAII guard holding an exclusive advisory lock on the whole store.
///
/// All *mutating* operations (store, delete, cleanup) take this lock, which
/// serializes them across threads **and** processes (`flock` on Unix,
/// `LockFileEx` on Windows via [`std::fs::File::lock`], stable since Rust
/// 1.89). Read operations are deliberately lock-free: atomic renames
/// guarantee readers always see complete files.
///
/// The lock is released when the guard drops (closing the file releases the
/// OS lock even if the process is killed).
///
/// # Caveats
///
/// - **Not reentrant**: acquiring it twice on the same thread deadlocks.
///   Internal callers that already hold it must use the `_unlocked`
///   variants of cleanup helpers.
/// - **Network filesystems**: advisory locking on NFS/SMB is only as
///   reliable as the server/mount options. On such mounts, concurrent
///   writers from *different machines* may not be fully serialized; the
///   atomic-rename write model still prevents torn files.
pub(crate) struct StoreLock {
    /// Keeps the locked file handle alive; the OS releases the lock on close.
    _file: File,
}

impl StoreLock {
    /// Block until an exclusive lock on the store at `base_dir` is acquired.
    ///
    /// Creates `base_dir` and the lock file if they do not exist yet.
    pub(crate) fn acquire(base_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(base_dir)
            .io_ctx(|| format!("creating store directory {}", base_dir.display()))?;

        let path = base_dir.join(LOCK_FILENAME);
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .io_ctx(|| format!("opening lock file {}", path.display()))?;

        file.lock()
            .io_ctx(|| format!("acquiring exclusive lock on {}", path.display()))?;

        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    #[test]
    fn test_atomic_write_creates_parents_and_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a").join("b").join("file.json");
        atomic_write(&path, b"{\"k\":1}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"k\":1}");
    }

    #[test]
    fn test_atomic_write_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.json");
        atomic_write(&path, b"old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn test_atomic_write_leaves_no_temps() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.json");
        atomic_write(&path, b"data").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(TMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn test_copy_file_atomic_hashed_matches_sha256_file() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"hello storage world").unwrap();

        let dest = dir.path().join("out").join("dest.bin");
        let (size, sha) = copy_file_atomic_hashed(&src, &dest).unwrap();

        assert_eq!(size, 19);
        assert_eq!(sha.len(), 64);
        assert_eq!(sha, sha256_file(&dest).unwrap());
        assert_eq!(sha, sha256_file(&src).unwrap());
    }

    #[test]
    fn test_copy_missing_source_errors_without_leftovers() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("dest.bin");
        let err = copy_file_atomic_hashed(&dir.path().join("nope.bin"), &dest).unwrap_err();
        assert!(err.to_string().contains("opening source artifact"));
        assert!(!dest.exists());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(TMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn test_remove_stale_temps_only_touches_prefix() {
        let dir = TempDir::new().unwrap();
        let stale = dir.path().join(format!("{TMP_PREFIX}999-1"));
        let real = dir.path().join("artifact.zip");
        std::fs::write(&stale, b"junk").unwrap();
        std::fs::write(&real, b"data").unwrap();

        remove_stale_temps(dir.path());

        assert!(!stale.exists());
        assert!(real.exists());
    }

    #[test]
    fn test_store_lock_serializes_threads() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let base = base.clone();
                let counter = counter.clone();
                std::thread::spawn(move || {
                    let _lock = StoreLock::acquire(&base).unwrap();
                    // With the lock held, no other thread may observe an odd value.
                    counter.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    counter.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(counter.load(Ordering::SeqCst) % 2, 0);
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 8);
    }
}
