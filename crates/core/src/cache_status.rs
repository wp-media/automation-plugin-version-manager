//! The artifact cache an [`Apvm`](crate::Apvm) builds with: whether it is
//! usable right now ([`CacheStatus`]), and how a build gets it
//! ([`open_cache`]).
//!
//! Every build, warm and status check opens the cache afresh — one SQLite
//! open with its integrity check — and lets go of it when done. So a broken
//! cache is reported (status, plus one build warning) instead of silently
//! skipped, a repaired one comes back without re-creating the instance, and
//! an idle instance never holds the database: a long-lived handle would
//! keep serving cached pages of a file damaged in place (its integrity
//! check still passes — verified) and, by holding the file open, make a
//! repair of a database SQLite can no longer read fail with "in use" from
//! any process.

use std::path::Path;
use std::sync::Arc;

use apvm_storage::ArtifactStore;

use crate::Config;

/// Whether an [`Apvm`](crate::Apvm) instance reads and writes the artifact
/// cache right now. See [`Apvm::cache_status`](crate::Apvm::cache_status).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CacheStatus {
    /// Caching is on and the cache is open.
    Active,
    /// Caching is turned off ([`Config::cache_enabled`] is `false`).
    Disabled,
    /// The cache needs a repair: its database is corrupt, or was lost while
    /// cached builds remain. Builds run uncached until
    /// [`CacheMaintenance::repair`](crate::CacheMaintenance::repair) — `apvm
    /// cache repair`, or `apvm.cache().repair()` from Node — fixes it; the
    /// instance then resumes caching on its own.
    Corrupted {
        /// What is wrong, e.g. SQLite's `file is not a database`.
        details: String,
    },
    /// The cache cannot be used for another reason: e.g. its directory
    /// cannot be created, holds someone else's data, or was written by a
    /// newer apvm. Builds run uncached.
    Unavailable {
        /// Why, as reported by the store.
        details: String,
    },
}

impl CacheStatus {
    /// Whether builds read and write the cache.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }

    /// Classify why the store could not be opened: damage `repair` fixes is
    /// [`CacheStatus::Corrupted`], anything else
    /// [`CacheStatus::Unavailable`].
    pub(crate) fn from_open_error(err: &apvm_storage::Error) -> Self {
        use apvm_storage::Error as Storage;
        match err {
            Storage::DatabaseCorrupted { details, .. } => Self::Corrupted {
                details: details.clone(),
            },
            corrupt if corrupt.is_corruption() => Self::Corrupted {
                details: corrupt.detail(),
            },
            Storage::MissingDatabase {
                adoptable_builds, ..
            } => Self::Corrupted {
                details: format!(
                    "its database is missing, empty or half-repaired \
                     ({adoptable_builds} re-indexable build(s) on disk)"
                ),
            },
            other => Self::Unavailable {
                details: other.detail(),
            },
        }
    }

    /// The warning a build or warm emits for this status, naming the cache
    /// at `dir`: `Some` only when caching is on but the cache is unusable.
    pub(crate) fn warning(&self, dir: &Path) -> Option<String> {
        match self {
            Self::Corrupted { details } => Some(format!(
                "the artifact cache at {} needs repair: {details}. This run neither reads nor \
                 writes the cache; run `apvm cache repair` (Node: `apvm.cache().repair()`) \
                 and caching resumes on its own.",
                dir.display()
            )),
            Self::Unavailable { details } => Some(format!(
                "the artifact cache at {} is unavailable: {details}. This run neither reads nor \
                 writes the cache.",
                dir.display()
            )),
            Self::Active | Self::Disabled => None,
        }
    }
}

/// Open the cache an [`Apvm`](crate::Apvm) builds with, as it is now:
/// `(store, status)`, the store `Some` only when the status is
/// [`CacheStatus::Active`]. Opening creates the cache when it is missing,
/// as the constructors do. Blocking: an SQLite open with its integrity
/// check — which also waits for a repair running meanwhile to finish.
///
/// Called for every build, warm and status check, and the store is let go
/// of when that is done: an idle instance holds no handle, so a repair from
/// any process is never refused because of it, and damage made in place —
/// invisible to a handle serving cached pages — is reported by the next
/// fresh open.
pub(crate) fn open_cache(config: &Config) -> (Held, CacheStatus) {
    if !config.cache_enabled {
        return (None, CacheStatus::Disabled);
    }
    match ArtifactStore::open(&config.cache_dir) {
        Ok(store) => (Some(Arc::new(store)), CacheStatus::Active),
        Err(err) => {
            tracing::warn!(cache_dir = %config.cache_dir.display(), error = %err, "artifact cache unusable");
            (None, CacheStatus::from_open_error(&err))
        }
    }
}

/// [`open_cache`] on the blocking thread pool, so an async caller never
/// blocks its runtime (the open may wait for a running repair).
pub(crate) async fn open_cache_async(config: &Config) -> (Held, CacheStatus) {
    let config = config.clone();
    match tokio::task::spawn_blocking(move || open_cache(&config)).await {
        Ok(outcome) => outcome,
        Err(err) => (
            None,
            CacheStatus::Unavailable {
                details: format!("the cache check did not complete: {err}"),
            },
        ),
    }
}

/// Let go of `store` on the blocking thread pool: closing the last handle
/// may checkpoint the database's WAL, which is I/O.
pub(crate) async fn close_async(store: Held) {
    if let Some(store) = store {
        // A failed close task only means the handle was dropped there.
        let _ = tokio::task::spawn_blocking(move || drop(store)).await;
    }
}

/// An open store, shared by the parts of one build.
pub(crate) type Held = Option<Arc<ArtifactStore>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn corruption_met_after_opening_is_corrupted() {
        let err = apvm_storage::Error::Database(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
            Some("file is not a database".to_string()),
        ));
        assert!(matches!(
            CacheStatus::from_open_error(&err),
            CacheStatus::Corrupted { .. }
        ));
        let busy = apvm_storage::Error::Database(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        ));
        assert!(matches!(
            CacheStatus::from_open_error(&busy),
            CacheStatus::Unavailable { .. }
        ));
    }

    /// A temp root plus a cache path inside it that does not exist yet.
    fn temp_cache() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("cache");
        (root, dir)
    }

    /// Create `dir` holding an `apvm.db` that is not a SQLite database.
    fn corrupt_database(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("apvm.db"), b"not a sqlite database").unwrap();
    }

    #[test]
    fn disabled_caching_opens_and_creates_nothing() {
        let (_root, dir) = temp_cache();
        let (store, status) = open_cache(&Config::new(dir.clone()).set_cache_enabled(false));
        assert!(store.is_none());
        assert_eq!(status, CacheStatus::Disabled);
        assert!(!dir.exists());
    }

    #[test]
    fn an_enabled_cache_is_created_and_opened() {
        let (_root, dir) = temp_cache();
        let (store, status) = open_cache(&Config::new(dir.clone()));
        assert_eq!(status, CacheStatus::Active);
        assert_eq!(store.unwrap().base_dir(), dir);
        assert!(dir.join("apvm.db").exists());
    }

    #[test]
    fn a_corrupt_cache_is_reported_then_active_after_repair() {
        let (_root, dir) = temp_cache();
        corrupt_database(&dir);
        let config = Config::new(dir.clone());

        let (store, status) = open_cache(&config);
        assert!(store.is_none());
        assert_eq!(
            status,
            CacheStatus::Corrupted {
                details: "file is not a database".to_string()
            }
        );

        ArtifactStore::repair(&dir).unwrap();
        let (store, status) = open_cache(&config);
        assert_eq!(status, CacheStatus::Active);
        assert!(store.is_some());
    }

    #[test]
    fn damage_made_in_place_is_reported_by_the_next_check() {
        // Audit C1: a store held between builds kept serving cached pages
        // of a database damaged in place, so the status stayed `Active`.
        let (_root, dir) = temp_cache();
        let config = Config::new(dir.clone());
        let (store, status) = open_cache(&config);
        assert_eq!(status, CacheStatus::Active);
        drop(store);
        std::fs::write(dir.join("apvm.db"), b"damaged header, in place").unwrap();
        for name in ["apvm.db-wal", "apvm.db-shm"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        assert!(matches!(
            open_cache(&config),
            (None, CacheStatus::Corrupted { .. })
        ));
    }

    #[test]
    fn a_deleted_database_with_builds_left_needs_repair() {
        let (_root, dir) = temp_cache();
        let open = ArtifactStore::open(&dir).unwrap();
        let src = tempfile::tempdir().unwrap();
        let zip = src.path().join("a.zip");
        std::fs::write(&zip, b"zip").unwrap();
        open.store(
            &apvm_storage::BuildMetadata::new(
                "wp-rocket",
                "3.17.4",
                apvm_storage::BuildSource::PullRequest(1),
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "develop".to_string(),
            ),
            &[apvm_storage::SourceArtifact {
                variant_id: None,
                path: zip,
                target_name: "a.zip".to_string(),
            }],
        )
        .unwrap();
        drop(open);
        for name in ["apvm.db", "apvm.db-wal", "apvm.db-shm"] {
            let _ = std::fs::remove_file(dir.join(name));
        }

        let (store, status) = open_cache(&Config::new(dir.clone()));
        assert!(store.is_none());
        assert_eq!(
            status,
            CacheStatus::Corrupted {
                details: "its database is missing, empty or half-repaired \
                          (1 re-indexable build(s) on disk)"
                    .to_string()
            }
        );
    }

    #[tokio::test]
    async fn the_async_open_agrees_with_the_blocking_one() {
        let (_root, dir) = temp_cache();
        corrupt_database(&dir);
        let config = Config::new(dir.clone());
        let (store, status) = open_cache_async(&config).await;
        assert!(store.is_none());
        assert!(matches!(status, CacheStatus::Corrupted { .. }));

        ArtifactStore::repair(&dir).unwrap();
        let (store, status) = open_cache_async(&config).await;
        assert_eq!(status, CacheStatus::Active);
        assert!(store.is_some());

        let (store, status) =
            open_cache_async(&Config::new(dir.clone()).set_cache_enabled(false)).await;
        assert!(store.is_none());
        assert_eq!(status, CacheStatus::Disabled);
    }

    #[tokio::test]
    async fn closing_lets_go_of_the_last_handle() {
        let (_root, dir) = temp_cache();
        let (store, _) = open_cache_async(&Config::new(dir)).await;
        let store = store.unwrap();
        let watcher = Arc::downgrade(&store);
        close_async(Some(store)).await;
        assert_eq!(watcher.strong_count(), 0, "the store is closed");
        close_async(None).await;
    }

    #[test]
    fn open_errors_are_classified_by_their_remedy() {
        use apvm_storage::Error as Storage;
        let path = PathBuf::from("/c");
        let classify = |err: Storage| CacheStatus::from_open_error(&err);

        assert_eq!(
            classify(Storage::DatabaseCorrupted {
                path: path.clone(),
                details: "malformed".to_string(),
            }),
            CacheStatus::Corrupted {
                details: "malformed".to_string()
            }
        );
        assert_eq!(
            classify(Storage::MissingDatabase {
                path: path.clone(),
                adoptable_builds: 3,
            }),
            CacheStatus::Corrupted {
                details: "its database is missing, empty or half-repaired \
                          (3 re-indexable build(s) on disk)"
                    .to_string()
            }
        );
        assert_eq!(
            classify(Storage::Io {
                context: "failed to create store directory /c".to_string(),
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
            }),
            CacheStatus::Unavailable {
                details: "failed to create store directory /c: denied".to_string()
            }
        );
        let foreign = Storage::ForeignDirectory {
            path,
            entry: "tools".to_string(),
        };
        let expected = foreign.to_string();
        assert_eq!(
            classify(foreign),
            CacheStatus::Unavailable { details: expected }
        );
    }

    #[test]
    fn only_an_unusable_cache_warns_and_the_text_names_the_remedy() {
        let dir = Path::new("/c");
        assert_eq!(CacheStatus::Active.warning(dir), None);
        assert_eq!(CacheStatus::Disabled.warning(dir), None);
        assert_eq!(
            CacheStatus::Corrupted {
                details: "file is not a database".to_string()
            }
            .warning(dir)
            .unwrap(),
            "the artifact cache at /c needs repair: file is not a database. This run neither \
             reads nor writes the cache; run `apvm cache repair` (Node: `apvm.cache().repair()`) \
             and caching resumes on its own."
        );
        assert_eq!(
            CacheStatus::Unavailable {
                details: "denied".to_string()
            }
            .warning(dir)
            .unwrap(),
            "the artifact cache at /c is unavailable: denied. This run neither reads nor writes \
             the cache."
        );
    }

    #[test]
    fn only_active_counts_as_active() {
        assert!(CacheStatus::Active.is_active());
        for status in [
            CacheStatus::Disabled,
            CacheStatus::Corrupted {
                details: String::new(),
            },
            CacheStatus::Unavailable {
                details: String::new(),
            },
        ] {
            assert!(!status.is_active(), "{status:?}");
        }
    }
}
