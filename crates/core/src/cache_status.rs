//! The artifact cache an [`Apvm`](crate::Apvm) builds with: whether it is
//! usable right now ([`CacheStatus`]), and the slot that keeps it open.
//!
//! The slot is re-checked before every build, warm and status call, so a
//! broken cache is reported instead of silently skipped, and a repaired one
//! comes back without re-creating the instance:
//!
//! - **Nothing open** (caching on): open the cache — one SQLite open with
//!   its integrity check. This succeeds once the cache is repaired, by any
//!   front end or process.
//! - **Open:** keep the store while its database file is still the one at
//!   the cache path ([`ArtifactStore::is_current`]); otherwise — the
//!   database was deleted or replaced (repair never does that: it resets the
//!   file in place) — drop it and open again.
//!
//! An open handle is not re-checked for damage made in place: SQLite serves
//! its cached pages, so its integrity check still passes (verified), while
//! the next fresh open — any new process, or a repair — reports it. A
//! repair in this process first makes every slot let go of the cache
//! ([`release_stores`]), since a corrupt database is only reset once nobody
//! has it open.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

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
                    "its database is missing or empty while {adoptable_builds} cached build(s) remain"
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

/// The open store of an [`Apvm`](crate::Apvm), shared by its concurrent
/// builds (each holds its own `Arc`, so replacing the slot never pulls a
/// store from under a running build).
#[derive(Debug)]
pub(crate) struct StoreSlot(Arc<Mutex<Held>>);

/// What a [`StoreSlot`] holds.
type Held = Option<Arc<ArtifactStore>>;

/// Every [`StoreSlot`] of this process, so a repair can make them let go of
/// a database it must reset ([`release_stores`]).
static SLOTS: Mutex<Vec<Weak<Mutex<Held>>>> = Mutex::new(Vec::new());

impl Default for StoreSlot {
    fn default() -> Self {
        Self::new(None)
    }
}

impl StoreSlot {
    /// A slot holding `store`, registered for [`release_stores`].
    fn new(store: Held) -> Self {
        let slot = Arc::new(Mutex::new(store));
        let mut slots = SLOTS.lock().unwrap_or_else(PoisonError::into_inner);
        slots.retain(|weak| weak.strong_count() > 0);
        slots.push(Arc::downgrade(&slot));
        drop(slots);
        Self(slot)
    }

    /// Re-check the cache (see the [module docs](self)) and return the store
    /// to use with its status. Blocking: a stat, plus an open when nothing
    /// usable is held.
    pub(crate) fn refresh(&self, config: &Config) -> (Held, CacheStatus) {
        refresh(&self.0, config.cache_enabled, &config.cache_dir)
    }

    /// [`StoreSlot::refresh`] on the blocking thread pool — where closing a
    /// replaced store happens too.
    pub(crate) async fn refresh_async(&self, config: &Config) -> (Held, CacheStatus) {
        let (slot, enabled, dir) = (
            Arc::clone(&self.0),
            config.cache_enabled,
            config.cache_dir.clone(),
        );
        match tokio::task::spawn_blocking(move || refresh(&slot, enabled, &dir)).await {
            Ok(outcome) => outcome,
            Err(err) => (
                None,
                CacheStatus::Unavailable {
                    details: format!("the cache check did not complete: {err}"),
                },
            ),
        }
    }

    /// The store currently held, if any.
    #[cfg(test)]
    pub(crate) fn current(&self) -> Held {
        lock(&self.0).clone()
    }
}

/// Refresh `slot` for a cache at `dir` (`enabled` = caching on). A disabled
/// cache releases its store. Otherwise the outcome is stored only if the
/// slot still holds what this refresh started from: a concurrent refresh
/// that settled first wins, so a stale store is never put back. Replaced
/// stores are closed after the lock is released.
fn refresh(slot: &Mutex<Held>, enabled: bool, dir: &Path) -> (Held, CacheStatus) {
    if !enabled {
        let released = lock(slot).take();
        drop(released);
        return (None, CacheStatus::Disabled);
    }
    let start = lock(slot).clone();
    let outcome = check_or_open(dir, start.clone());
    let replaced = settle(slot, &start, outcome.0.clone());
    drop((replaced, start));
    outcome
}

/// Put `store` in `slot` if it still holds `start`, returning what it held
/// (for the caller to drop outside the lock); `None` when another refresh
/// settled first, whose store is then kept.
fn settle(slot: &Mutex<Held>, start: &Held, store: Held) -> Option<Held> {
    let mut held = lock(slot);
    let unchanged = match (&*held, start) {
        (Some(now), Some(then)) => Arc::ptr_eq(now, then),
        (None, None) => true,
        _ => false,
    };
    unchanged.then(|| std::mem::replace(&mut *held, store))
}

/// Lock the slot. A poisoned lock is recovered: it guards only an
/// `Option`, which a panicking holder cannot leave half-written.
fn lock(slot: &Mutex<Held>) -> MutexGuard<'_, Held> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Make every [`Apvm`](crate::Apvm) of this process drop its open store of
/// the cache at `dir`, closing its connection (unless a running build still
/// holds the store). Each reopens on its next build or status check.
///
/// A repair calls this first: resetting a corrupt database in place needs
/// it closed everywhere, and idle instances would otherwise keep it open —
/// and the repair refused — for as long as they live. Blocking (closing a
/// store may checkpoint its WAL).
pub(crate) fn release_stores(dir: &Path) {
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let live: Vec<_> = {
        let slots = SLOTS.lock().unwrap_or_else(PoisonError::into_inner);
        slots.iter().filter_map(Weak::upgrade).collect()
    };
    for slot in live {
        let released = {
            let mut held = lock(&slot);
            if held.as_ref().is_some_and(|store| store.base_dir() == dir) {
                held.take()
            } else {
                None
            }
        };
        drop(released);
    }
}

/// Keep `current` while it still serves the cache at `dir` (same directory,
/// [current](ArtifactStore::is_current) database file), else open the
/// cache there (creating it when missing, as the constructors do). Blocking.
fn check_or_open(dir: &Path, current: Held) -> (Held, CacheStatus) {
    if let Some(store) = current {
        if store.base_dir() == dir && store.is_current() {
            return (Some(store), CacheStatus::Active);
        }
        tracing::warn!(
            db = %store.db_path().display(),
            "the cache directory changed, or its database was renamed, deleted or replaced; \
             opening the cache again"
        );
    }
    match ArtifactStore::open(dir) {
        Ok(store) => (Some(Arc::new(store)), CacheStatus::Active),
        Err(err) => {
            tracing::warn!(cache_dir = %dir.display(), error = %err, "artifact cache unusable");
            (None, CacheStatus::from_open_error(&err))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A temp root plus a cache path inside it that does not exist yet.
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

    #[test]
    fn release_stores_releases_only_the_cache_named() {
        let (_root_a, dir_a) = temp_cache();
        let (_root_b, dir_b) = temp_cache();
        let open = |dir: &Path| Some(Arc::new(ArtifactStore::open(dir).unwrap()));
        let slot_a = StoreSlot::new(open(&dir_a));
        let slot_b = StoreSlot::new(open(&dir_b));
        release_stores(&dir_a);
        assert!(slot_a.current().is_none(), "the named cache is released");
        assert!(slot_b.current().is_some(), "another cache is kept");
        // A released slot reopens on its next check.
        let (held, status) = slot_a.refresh(&Config::new(dir_a.clone()));
        assert!(held.is_some() && status.is_active());
    }

    #[test]
    fn release_stores_keeps_a_store_a_running_build_holds() {
        let (_root, dir) = temp_cache();
        let store = Arc::new(ArtifactStore::open(&dir).unwrap());
        let slot = StoreSlot::new(Some(Arc::clone(&store)));
        release_stores(&dir);
        assert!(slot.current().is_none());
        // The build's own handle is untouched: it finishes on its store.
        assert!(store.is_current());
    }

    fn temp_cache() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("cache");
        (root, dir)
    }

    fn corrupt_database(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("apvm.db"), b"not a sqlite database").unwrap();
    }

    #[test]
    fn disabled_caching_opens_and_creates_nothing() {
        let (_root, dir) = temp_cache();
        let slot = StoreSlot::default();
        let (store, status) = slot.refresh(&Config::new(dir.clone()).set_cache_enabled(false));
        assert!(store.is_none());
        assert_eq!(status, CacheStatus::Disabled);
        assert!(!dir.exists());
    }

    #[test]
    fn nothing_open_opens_the_cache_and_holds_it() {
        let (_root, dir) = temp_cache();
        let slot = StoreSlot::default();
        let (store, status) = slot.refresh(&Config::new(dir.clone()));
        assert_eq!(status, CacheStatus::Active);
        let held = slot.current().expect("held after a successful open");
        assert!(Arc::ptr_eq(&held, &store.unwrap()));
    }

    #[test]
    fn a_current_store_is_kept_not_reopened() {
        let (_root, dir) = temp_cache();
        let open = Arc::new(ArtifactStore::open(&dir).unwrap());
        let slot = StoreSlot::new(Some(Arc::clone(&open)));
        let (store, status) = slot.refresh(&Config::new(dir.clone()));
        assert_eq!(status, CacheStatus::Active);
        assert!(Arc::ptr_eq(&store.unwrap(), &open));
    }

    #[test]
    fn a_corrupt_cache_is_reported_then_reattached_after_repair() {
        let (_root, dir) = temp_cache();
        corrupt_database(&dir);
        let slot = StoreSlot::default();
        let config = Config::new(dir.clone());

        let (store, status) = slot.refresh(&config);
        assert!(store.is_none());
        assert_eq!(
            status,
            CacheStatus::Corrupted {
                details: "file is not a database".to_string()
            }
        );
        assert!(slot.current().is_none());

        ArtifactStore::repair(&dir).unwrap();
        let (store, status) = slot.refresh(&config);
        assert_eq!(status, CacheStatus::Active);
        assert!(store.is_some() && slot.current().is_some());
    }

    // Unix only: Windows cannot rename or delete an open database.
    #[cfg(unix)]
    #[test]
    fn a_store_whose_database_was_renamed_aside_is_replaced() {
        let (_root, dir) = temp_cache();
        let old = Arc::new(ArtifactStore::open(&dir).unwrap());
        let slot = StoreSlot::new(Some(Arc::clone(&old)));
        // What an older apvm's repair (or anyone) may do to the file.
        for suffix in ["", "-wal", "-shm"] {
            let from = dir.join(format!("apvm.db{suffix}"));
            if from.exists() {
                std::fs::rename(&from, dir.join(format!("apvm.db.corrupt-1{suffix}"))).unwrap();
            }
        }

        let (store, status) = slot.refresh(&Config::new(dir.clone()));
        assert_eq!(
            status,
            CacheStatus::Active,
            "the empty dir gets a new cache"
        );
        let store = store.unwrap();
        assert!(!Arc::ptr_eq(&store, &old));
        assert!(store.is_current());
    }

    #[cfg(unix)]
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
        let slot = StoreSlot::new(Some(Arc::new(open)));
        for name in ["apvm.db", "apvm.db-wal", "apvm.db-shm"] {
            let _ = std::fs::remove_file(dir.join(name));
        }

        let (store, status) = slot.refresh(&Config::new(dir.clone()));
        assert!(store.is_none() && slot.current().is_none());
        assert_eq!(
            status,
            CacheStatus::Corrupted {
                details: "its database is missing or empty while 1 cached build(s) remain"
                    .to_string()
            }
        );
    }

    #[test]
    fn a_store_for_another_directory_is_replaced() {
        // Audit A3: a held store survived a changed `cache_dir`.
        let root = tempfile::tempdir().unwrap();
        let (old_dir, new_dir) = (root.path().join("a"), root.path().join("b"));
        let old = Arc::new(ArtifactStore::open(&old_dir).unwrap());
        let slot = StoreSlot::new(Some(Arc::clone(&old)));
        let (store, status) = slot.refresh(&Config::new(new_dir.clone()));
        assert_eq!(status, CacheStatus::Active);
        assert_eq!(store.unwrap().base_dir(), new_dir);
        assert!(new_dir.join("apvm.db").exists());
    }

    #[test]
    fn disabling_caching_releases_the_store() {
        // Audit A4: the database stayed open after caching was turned off.
        let (_root, dir) = temp_cache();
        let slot = StoreSlot::new(Some(Arc::new(ArtifactStore::open(&dir).unwrap())));
        slot.refresh(&Config::new(dir.clone()).set_cache_enabled(false));
        assert!(slot.current().is_none());
    }

    #[test]
    fn a_concurrent_refresh_that_settled_first_is_not_overwritten() {
        // Audit H1: last-writer-wins could put a stale store back.
        let (_root, dir) = temp_cache();
        let open = || Some(Arc::new(ArtifactStore::open(&dir).unwrap()));
        let (stale, newer, late) = (open(), open(), open());
        let slot = StoreSlot::new(stale.clone());

        // Unchanged since `stale` was read: replaced, and the old one handed back.
        let handed_back = settle(&slot.0, &stale, newer.clone()).expect("replaced");
        assert!(Arc::ptr_eq(
            handed_back.as_ref().unwrap(),
            stale.as_ref().unwrap()
        ));
        // A refresh that also started from `stale` settles too late: no-op.
        assert!(settle(&slot.0, &stale, late).is_none());
        assert!(Arc::ptr_eq(
            &slot.current().unwrap(),
            newer.as_ref().unwrap()
        ));
        // From an empty slot, only an empty start may fill it.
        let empty = StoreSlot::default();
        assert!(settle(&empty.0, &stale, newer.clone()).is_none());
        assert!(settle(&empty.0, &None, newer).is_some());
        assert!(empty.current().is_some());
    }

    #[tokio::test]
    async fn the_async_refresh_agrees_with_the_blocking_one() {
        let (_root, dir) = temp_cache();
        corrupt_database(&dir);
        let slot = StoreSlot::default();
        let config = Config::new(dir.clone());
        let (store, status) = slot.refresh_async(&config).await;
        assert!(store.is_none());
        assert!(matches!(status, CacheStatus::Corrupted { .. }));

        ArtifactStore::repair(&dir).unwrap();
        let (store, status) = slot.refresh_async(&config).await;
        assert_eq!(status, CacheStatus::Active);
        assert!(Arc::ptr_eq(&store.unwrap(), &slot.current().unwrap()));

        let (store, status) = slot
            .refresh_async(&Config::new(dir.clone()).set_cache_enabled(false))
            .await;
        assert!(store.is_none());
        assert_eq!(status, CacheStatus::Disabled);
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
                details: "its database is missing or empty while 3 cached build(s) remain"
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
