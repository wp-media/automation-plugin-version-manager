//! `ApvmCache` — artifact-cache maintenance for Node.js, at parity with
//! `apvm cache` (`info`, `clean`, `gc`, `verify`, `repair`, `clear`).
//!
//! A thin adapter over [`apvm_core::CacheMaintenance`], which owns every
//! decision (the no-cache guard, the duration and project rules), so the CLI
//! and the bindings cannot drift apart. This module adds JS types, strict
//! argument reading ([`crate::cache_input`]) and machine-readable error codes:
//!
//! | `err.code` | when | remedy |
//! |---|---|---|
//! | `CacheCorrupted` | the database is corrupt, unreadable, or missing/blank beside cached builds (every method but `repair()`) | `await cache.repair()`, then retry |
//! | `InvalidArg` | bad options (wrong type, unknown key, a bad `olderThan` / `project` / `target`), or an empty cache path | fix the input |
//! | `GenericFailure` | anything else (I/O, foreign data, a newer schema, a failed task) | report it |
//!
//! Every method returns a promise and reports every error by rejecting it;
//! only `ApvmCache.open()`, which is synchronous, throws (`InvalidArg`).
//!
//! Each call opens its own store on the blocking thread pool and closes it
//! before the promise settles, so no handle outlives a call and the event
//! loop never blocks. At most one call per CPU runs at a time (the rest
//! wait), so a burst of maintenance cannot starve builds of blocking
//! threads. Calls are safe beside builds on the same cache.

use std::sync::OnceLock;

use apvm_core::CacheMaintenance;
use napi::bindgen_prelude::{JsObjectValue, JsValue, PromiseRaw, ToNapiValue, Unknown};
use napi::{Env, Property, Status};
use napi_derive::napi;
use tokio::sync::Semaphore;

use crate::cache_input;
use crate::cache_types::{JsCacheUsage, JsCleanReport, JsGcReport, JsRepairReport, JsVerifyIssue};
use crate::config::{ApvmConfig, resolve_config};
use crate::error::core_error_status;
use crate::single_copy::ensure_single_copy;

/// Maintenance handle for one artifact cache directory.
///
/// Get one from an instance — `apvm.cache()`, the cache that instance builds
/// into — or standalone with `ApvmCache.open(config?)`, which needs no
/// GitHub client. Creating a handle touches nothing on disk, and every
/// method works whether caching is enabled or not.
///
/// A missing (or empty) cache directory is not an error: every method
/// resolves with an empty report and creates nothing; `info().exists`
/// tells "never created" apart from "empty".
///
/// # TypeScript
///
/// ```typescript
/// const cache = ApvmCache.open({ cacheDir: '/var/lib/apvm/cache' });
/// await cache.clean({ olderThan: '30d', target: JsCleanTarget.Builds });
/// let usage;
/// try {
///   usage = await cache.info();
/// } catch (e) {
///   if ((e as { code?: string }).code !== 'CacheCorrupted') throw e;
///   await cache.repair();
///   usage = await cache.info();
/// }
/// ```
#[napi]
pub struct ApvmCache {
    inner: CacheMaintenance,
}

impl ApvmCache {
    /// A handle for `inner`'s directory.
    pub fn new(inner: CacheMaintenance) -> Self {
        Self { inner }
    }
}

#[napi]
impl ApvmCache {
    /// Open the cache that `Apvm.create(config)` would build into: `cacheDir`
    /// (default `~/.apvm/cache`), overridden by `APVM_CACHE_DIR` when set,
    /// and made absolute. Reads only `cacheDir`; touches nothing on disk.
    ///
    /// # Throws
    ///
    /// `InvalidArg` when `config` is not an object, has an unknown key, or
    /// has a `cacheDir` that is not a string — including `undefined` /
    /// `null`: omit the key to mean the default cache. `GenericFailure` when
    /// another copy of this addon is already in use in the process.
    #[napi(factory, ts_args_type = "config?: ApvmConfig | undefined | null")]
    pub fn open(env: &Env, config: Option<Unknown<'_>>) -> napi::Result<Self> {
        ensure_single_copy(env)?;
        let cache_dir = cache_input::read_cache_dir(config)
            .map_err(|message| napi::Error::new(Status::InvalidArg, message))?;
        let config = resolve_config(Some(ApvmConfig {
            cache_dir,
            ..ApvmConfig::default()
        }));
        Ok(Self::new(CacheMaintenance::new(config.cache_dir)))
    }

    /// The cache directory this handle operates on.
    #[napi]
    pub fn dir(&self) -> String {
        self.inner.dir().to_string_lossy().into_owned()
    }

    /// Usage totals and a per-project breakdown (`apvm cache info`).
    ///
    /// # Throws
    ///
    /// `CacheCorrupted`; `InvalidArg` (empty path); `GenericFailure`.
    #[napi(ts_return_type = "Promise<JsCacheUsage>")]
    pub fn info<'env>(&self, env: &'env Env) -> napi::Result<PromiseRaw<'env, JsCacheUsage>> {
        run(env, &self.inner, Action::Inspect, |cache| {
            Ok(JsCacheUsage::new(cache.dir(), cache.usage()?))
        })
    }

    /// Remove the entries `options` selects (`apvm cache clean`); with no
    /// options, everything. With `dryRun`, only reports them. Options are
    /// validated before the disk is touched, whatever its state.
    ///
    /// # Throws
    ///
    /// `InvalidArg` for bad options — not an object, an unknown key, a wrong
    /// type, a key set to `undefined` / `null` (omit it instead), a bad
    /// `olderThan`, `project` or `target` — or an empty path;
    /// `CacheCorrupted`; `GenericFailure`.
    #[napi(
        ts_args_type = "options?: CleanOptions | undefined | null",
        ts_return_type = "Promise<JsCleanReport>"
    )]
    pub fn clean<'env>(
        &self,
        env: &'env Env,
        options: Option<Unknown<'_>>,
    ) -> napi::Result<PromiseRaw<'env, JsCleanReport>> {
        let request = cache_input::read_clean_fields(options).and_then(cache_input::clean_request);
        run(env, &self.inner, Action::Modify, move |cache| {
            let request = request.map_err(apvm_core::Error::Config)?;
            Ok(JsCleanReport::new(cache.clean(&request)?, request.dry_run))
        })
    }

    /// Remove every build and cached release (`apvm cache clear`). There is
    /// no prompt: the call is the consent.
    ///
    /// # Throws
    ///
    /// `CacheCorrupted`; `InvalidArg` (empty path); `GenericFailure`.
    #[napi(ts_return_type = "Promise<JsCleanReport>")]
    pub fn clear<'env>(&self, env: &'env Env) -> napi::Result<PromiseRaw<'env, JsCleanReport>> {
        run(env, &self.inner, Action::Modify, |cache| {
            Ok(JsCleanReport::new(cache.clear()?, false))
        })
    }

    /// Reconcile the database with disk (`apvm cache gc [--checksum]`):
    /// remove records whose files are missing or damaged (as `verify()` at
    /// the same depth finds them) with those files, plus orphan directories
    /// and stale temp files. Files it cannot read are kept and listed in
    /// `failures`. The next build caches a removed entry again.
    ///
    /// # Throws
    ///
    /// `InvalidArg` for bad options or an empty path; `CacheCorrupted`;
    /// `GenericFailure`.
    #[napi(
        ts_args_type = "options?: GcOptions | undefined | null",
        ts_return_type = "Promise<JsGcReport>"
    )]
    pub fn gc<'env>(
        &self,
        env: &'env Env,
        options: Option<Unknown<'_>>,
    ) -> napi::Result<PromiseRaw<'env, JsGcReport>> {
        let mode = cache_input::read_verify_mode(options, "gc() options");
        run(env, &self.inner, Action::Modify, move |cache| {
            let mode = mode.map_err(apvm_core::Error::Config)?;
            Ok(JsGcReport::from(cache.gc(mode)?))
        })
    }

    /// Check stored files (`apvm cache verify [--checksum]`). Resolves with
    /// the problems found — `[]` means healthy; it does not reject for them.
    /// Reports only: `gc()` at the same depth removes what it finds. A
    /// snapshot: an entry removed meanwhile (by `clean()` or a build) may
    /// still be reported as `missing`.
    ///
    /// # Throws
    ///
    /// `InvalidArg` for bad options or an empty path; `CacheCorrupted`;
    /// `GenericFailure`.
    #[napi(
        ts_args_type = "options?: VerifyOptions | undefined | null",
        ts_return_type = "Promise<JsVerifyIssue[]>"
    )]
    pub fn verify<'env>(
        &self,
        env: &'env Env,
        options: Option<Unknown<'_>>,
    ) -> napi::Result<PromiseRaw<'env, Vec<JsVerifyIssue>>> {
        let mode = cache_input::read_verify_mode(options, "verify() options");
        run(env, &self.inner, Action::Inspect, move |cache| {
            let mode = mode.map_err(apvm_core::Error::Config)?;
            let issues = cache.verify(mode)?.unwrap_or_default();
            Ok(issues.iter().map(JsVerifyIssue::from).collect())
        })
    }

    /// Recover the cache (`apvm cache repair`): reset a corrupt database in
    /// place or clear an unreadable one (keeping a copy of either), or
    /// rebuild a missing or blank one; then re-index the builds on disk. A
    /// no-op on a healthy cache. Never renames the database, so other
    /// processes using it cannot be crashed. Interrupted (e.g. the process
    /// exited), it leaves the cache needing repair — never half-indexed — so
    /// running it again completes it. `Apvm` instances of this process let go
    /// of the cache first and resume caching on their next build.
    ///
    /// # Throws
    ///
    /// `InvalidArg` (empty path); `GenericFailure` — never `CacheCorrupted`,
    /// since repair is that remedy — including when another process (or a
    /// build running here) holds a corrupt database open: nothing is
    /// changed; retry once it is done.
    #[napi(ts_return_type = "Promise<JsRepairReport>")]
    pub fn repair<'env>(&self, env: &'env Env) -> napi::Result<PromiseRaw<'env, JsRepairReport>> {
        run(env, &self.inner, Action::Repair, |cache| {
            Ok(JsRepairReport::from(cache.repair()?))
        })
    }
}

// =============================================================================
// Running an action: blocking pool → promise, with an error code
// =============================================================================

/// What a method does — which decides whether a failure may name
/// `repair()` as its remedy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Reads the cache (`info`, `verify`).
    Inspect,
    /// Changes the cache (`clean`, `clear`, `gc`).
    Modify,
    /// `repair` itself: must not name itself, or a caller following the
    /// recipe would loop.
    Repair,
}

impl Action {
    /// Whether a failure of this action may point to `repair()`.
    fn offers_repair(self) -> bool {
        self != Self::Repair
    }
}

/// Limits concurrent maintenance calls to one per CPU, so they cannot take
/// every thread of the blocking pool that builds also use.
fn slots() -> &'static Semaphore {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| {
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        Semaphore::new(cpus)
    })
}

/// Run `op` on the blocking thread pool (waiting for a [`slots`] permit)
/// and return a promise for its result. On the JS thread, an error rejects
/// the promise with a JS `Error` carrying [`error_code`] as `code` and
/// [`error_message`] as `message`.
///
/// A plain `async fn` cannot do this: its error type must be `napi::Error`
/// with a `Status` code, so the custom `CacheCorrupted` would be lost.
fn run<'env, V, F>(
    env: &'env Env,
    cache: &CacheMaintenance,
    action: Action,
    op: F,
) -> napi::Result<PromiseRaw<'env, V>>
where
    V: ToNapiValue + Send + 'static,
    F: FnOnce(&CacheMaintenance) -> apvm_core::Result<V> + Send + 'static,
{
    let cache = cache.clone();
    let task = async move {
        // The semaphore is never closed, so acquiring cannot fail.
        let _permit = slots().acquire().await.ok();
        Ok(tokio::task::spawn_blocking(move || op(&cache)).await)
    };
    env.spawn_future_with_callback(task, move |env, joined| match joined {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(js_error(
            env,
            error_code(&err, action),
            &error_message(&err, action),
        )),
        // The task panicked or was cancelled: report it, never crash.
        Err(join) => Err(napi::Error::new(
            Status::GenericFailure,
            format!("the cache task did not complete: {join}"),
        )),
    })
}

/// A JS `Error` with `message` and a custom `code`, as a `napi::Error` that
/// rejects the promise with that very object (so `code` survives).
///
/// `code` is defined as an own property, not assigned, so an accessor or a
/// read-only `code` on `Error.prototype` cannot swallow it.
fn js_error(env: &Env, code: &str, message: &str) -> napi::Error {
    let build = || -> napi::Result<napi::Error> {
        let mut error = env.create_error(napi::Error::new(Status::GenericFailure, message))?;
        let code = env.create_string(code)?;
        error.define_properties(&[Property::new().with_utf8_name("code")?.with_value(&code)])?;
        Ok(napi::Error::from(error.to_unknown()))
    };
    // Creating the object can only fail if the VM is shutting down; keep the
    // message and the closest status code then.
    build().unwrap_or_else(|_| napi::Error::new(Status::GenericFailure, message.to_string()))
}

/// The `err.code` for a failed action (see the [module docs](self)): pure,
/// so the mapping is unit-tested for every variant.
fn error_code(err: &apvm_core::Error, action: Action) -> &'static str {
    if action.offers_repair() && needs_repair(err) {
        return "CacheCorrupted";
    }
    match core_error_status(err) {
        Status::InvalidArg => "InvalidArg",
        _ => "GenericFailure",
    }
}

/// Whether `repair()` is the remedy: a corrupt database (however it was
/// detected), unreadable rows, or a database missing, blank or half-repaired
/// while cached builds remain.
fn needs_repair(err: &apvm_core::Error) -> bool {
    match err {
        apvm_core::Error::Storage(apvm_storage::Error::MissingDatabase { .. }) => true,
        apvm_core::Error::Storage(inner) => inner.is_corruption(),
        _ => false,
    }
}

/// The message for a failed action: the error with its I/O cause (which a
/// storage I/O error's own message leaves out), plus the next step where
/// there is one — the CLI's hints, in JS terms. Input errors are shown as
/// is, without the core's "Configuration error:" prefix.
fn error_message(err: &apvm_core::Error, action: Action) -> String {
    use apvm_storage::Error as Storage;
    let base = match err {
        apvm_core::Error::Storage(inner) => format!("Storage error: {}", inner.detail()),
        apvm_core::Error::Config(message) => message.clone(),
        other => other.to_string(),
    };
    let hint = match err {
        apvm_core::Error::Storage(Storage::MissingDatabase { .. }) if action.offers_repair() => {
            "The cache database is missing or empty — call repair() to re-index the cache from disk."
        }
        _ if action.offers_repair() && needs_repair(err) => {
            "The cache database is corrupt — call repair() to recover it."
        }
        apvm_core::Error::Storage(Storage::ForeignDirectory { .. }) => {
            "Check the cache location: APVM_CACHE_DIR if set, else cacheDir, else ~/.apvm/cache."
        }
        _ => return base,
    };
    format!("{base}\n  {hint}")
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;

    use super::*;

    const ACTIONS: [Action; 3] = [Action::Inspect, Action::Modify, Action::Repair];

    fn storage(err: apvm_storage::Error) -> apvm_core::Error {
        apvm_core::Error::Storage(err)
    }

    fn corrupted() -> apvm_core::Error {
        storage(apvm_storage::Error::DatabaseCorrupted {
            path: PathBuf::from("/c/apvm.db"),
            details: "file is not a database".to_string(),
        })
    }

    fn data() -> apvm_core::Error {
        storage(apvm_storage::Error::Data {
            details: "bad timestamp".to_string(),
        })
    }

    fn missing_db() -> apvm_core::Error {
        storage(apvm_storage::Error::MissingDatabase {
            path: PathBuf::from("/c"),
            adoptable_builds: 2,
        })
    }

    fn foreign() -> apvm_core::Error {
        storage(apvm_storage::Error::ForeignDirectory {
            path: PathBuf::from("/c"),
            entry: "notes".to_string(),
        })
    }

    fn storage_io() -> apvm_core::Error {
        storage(apvm_storage::Error::Io {
            context: "failed to open /c".to_string(),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "Permission denied"),
        })
    }

    fn invalid_project() -> apvm_core::Error {
        storage(apvm_storage::Error::InvalidInput {
            what: "project",
            value: "Bad/Name".to_string(),
            reason: "not allowed".to_string(),
        })
    }

    // ---- actions --------------------------------------------------------

    #[test]
    fn only_repair_withholds_the_repair_remedy() {
        assert!(Action::Inspect.offers_repair());
        assert!(Action::Modify.offers_repair());
        assert!(!Action::Repair.offers_repair());
    }

    #[test]
    fn maintenance_slots_allow_at_least_one_call() {
        assert!(slots().available_permits() >= 1);
    }

    // ---- error codes ----------------------------------------------------

    #[test]
    fn damage_repair_fixes_is_cache_corrupted() {
        for action in [Action::Inspect, Action::Modify] {
            for err in [corrupted(), data(), missing_db()] {
                assert_eq!(error_code(&err, action), "CacheCorrupted", "{err}");
            }
        }
    }

    #[test]
    fn corruption_met_after_opening_is_cache_corrupted() {
        let not_a_database = storage(apvm_storage::Error::Database(
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
                Some("file is not a database".to_string()),
            ),
        ));
        assert_eq!(
            error_code(&not_a_database, Action::Inspect),
            "CacheCorrupted"
        );
        let in_use = storage(apvm_storage::Error::DatabaseInUse {
            path: PathBuf::from("/c/apvm.db"),
        });
        assert_eq!(error_code(&in_use, Action::Repair), "GenericFailure");
    }

    #[test]
    fn repair_never_names_itself_as_the_remedy() {
        for err in [corrupted(), data(), missing_db()] {
            assert_eq!(error_code(&err, Action::Repair), "GenericFailure", "{err}");
            assert!(
                !error_message(&err, Action::Repair).contains("repair()"),
                "{err}"
            );
        }
    }

    #[test]
    fn bad_input_is_invalid_arg_for_every_action() {
        let bad_options = apvm_core::Error::Config("'30y' has an unknown unit".to_string());
        for action in ACTIONS {
            assert_eq!(error_code(&invalid_project(), action), "InvalidArg");
            assert_eq!(error_code(&bad_options, action), "InvalidArg");
        }
    }

    #[test]
    fn every_other_storage_and_core_error_is_a_generic_failure() {
        let others = [
            foreign(),
            storage_io(),
            storage(apvm_storage::Error::UnsupportedSchema {
                found: 9,
                supported: 1,
            }),
            storage(apvm_storage::Error::StaleHandle {
                path: PathBuf::from("/c/apvm.db"),
            }),
            apvm_core::Error::Io(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
            apvm_core::Error::Cache("x".to_string()),
        ];
        for action in ACTIONS {
            for err in &others {
                assert_eq!(error_code(err, action), "GenericFailure", "{err}");
            }
        }
    }

    // ---- messages -------------------------------------------------------

    #[test]
    fn corruption_messages_point_to_repair() {
        for err in [corrupted(), data()] {
            let message = error_message(&err, Action::Inspect);
            assert!(message.starts_with("Storage error: "), "{message}");
            assert!(
                message
                    .ends_with("\n  The cache database is corrupt — call repair() to recover it."),
                "{message}"
            );
        }
        assert!(
            error_message(&missing_db(), Action::Modify)
                .ends_with("call repair() to re-index the cache from disk.")
        );
    }

    #[test]
    fn messages_name_no_rust_api() {
        for err in [corrupted(), data(), missing_db()] {
            for action in ACTIONS {
                let message = error_message(&err, action);
                assert!(!message.contains("ArtifactStore"), "{message}");
            }
        }
    }

    #[test]
    fn foreign_directory_message_explains_the_cache_location() {
        for action in ACTIONS {
            let message = error_message(&foreign(), action);
            assert!(message.contains("refusing to use /c"), "{message}");
            assert!(
                message.ends_with("else cacheDir, else ~/.apvm/cache."),
                "{message}"
            );
        }
    }

    #[test]
    fn storage_io_messages_keep_their_cause() {
        assert_eq!(
            error_message(&storage_io(), Action::Inspect),
            "Storage error: failed to open /c: Permission denied"
        );
    }

    #[test]
    fn input_messages_drop_the_configuration_prefix() {
        let err = apvm_core::Error::Config("'30y' has an unknown unit".to_string());
        assert_eq!(
            error_message(&err, Action::Modify),
            "'30y' has an unknown unit"
        );
    }

    #[test]
    fn other_messages_pass_through_unchanged() {
        let err = invalid_project();
        assert_eq!(error_message(&err, Action::Modify), err.to_string());
    }
}
