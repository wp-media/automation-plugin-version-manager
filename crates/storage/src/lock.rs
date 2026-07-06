//! Cross-process advisory lock serializing mutating store operations.
//!
//! SQLite (WAL mode) already serializes database writes on its own; this
//! lock exists for the **filesystem** side: it prevents, say, a `clean()`
//! in one process from deleting a directory while a `store()` in another
//! process is still copying files into it. Read-only operations never take
//! it.
//!
//! Implementation: `std::fs::File::lock` — `flock` on Unix, `LockFileEx` on
//! Windows (stable since Rust 1.89). Advisory locking over network
//! filesystems (NFS/SMB) is only as reliable as the server; keep the store
//! on a local disk.

use std::fs::{File, OpenOptions};
use std::path::Path;

use crate::error::{IoContext, Result};
use crate::paths::LOCK_FILE_NAME;

/// Exclusive lock over the store, released on drop.
#[derive(Debug)]
pub(crate) struct StoreLock {
    file: File,
}

impl StoreLock {
    /// Block until the store-wide exclusive lock is acquired.
    ///
    /// The lock file (`.apvm.lock`) is created on first use and left in
    /// place afterwards — on Windows a locked file cannot be deleted, so a
    /// persistent lock file is the portable choice.
    pub(crate) fn acquire(base_dir: &Path) -> Result<Self> {
        let path = base_dir.join(LOCK_FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .io_ctx(|| format!("failed to open store lock file {}", path.display()))?;
        file.lock()
            .io_ctx(|| format!("failed to acquire store lock {}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Closing the descriptor would release the lock anyway; unlocking
        // explicitly just makes the release prompt and intentional.
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_excludes_second_acquisition_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let held = StoreLock::acquire(dir.path()).unwrap();

        // A second open descriptor must not get the lock while it is held.
        let path = dir.path().join(LOCK_FILE_NAME);
        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(probe.try_lock().is_err());

        drop(held);
        assert!(probe.try_lock().is_ok());
    }
}
