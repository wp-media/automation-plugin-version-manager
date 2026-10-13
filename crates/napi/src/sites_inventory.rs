//! `SiteInventory` — the site inventory (`~/.apvm/inventory.json`) from
//! Node.js: servers, the WordPress sites on them, groups of sites, and
//! default SSH settings (plan §11, P2.4).
//!
//! A thin adapter over `apvm_sites::inventory::io` and
//! [`apvm_sites::Inventory`], which own every rule (validation, locking,
//! atomic `0600` writes, conflict detection), so the CLI and Node cannot
//! drift apart. This module adds JS types, strict argument reading
//! ([`crate::sites_input`]), the default location and `err.code`s.
//!
//! # Synchronous by design
//!
//! Every method is synchronous. `update(fn)` must call `fn` on the JS
//! thread while it holds the inventory's lock, so it cannot run on a
//! worker pool; the other methods read or write a file of a few kilobytes.
//! Being synchronous also keeps the class clear of napi-rs's async-method
//! pitfalls (no native borrow outlives a call). The cost: a writer blocks
//! the event loop while it waits for another process's lock, at most
//! `io::LOCK_WAIT` (10 s), and only a stuck process holds it that long.
//!
//! # No lost edits
//!
//! An object remembers the file it was loaded from (or last saved to) and
//! the [`Fingerprint`] of what that file held. `save()` writes only when the
//! file still holds exactly that (`io::save_if_unchanged`), so another
//! process's edit made in between is never silently overwritten: the save
//! throws `InventoryChanged`, leaves the file as it is, and the object
//! keeps its own changes. After every save the object tracks the fingerprint the save
//! returned, never one from reloading the file (which could pair this
//! object's inventory with another process's later edit). `update(fn)` is
//! load → `fn` → save under the lock, so it never conflicts; the object
//! `fn` receives tracks the fingerprint the library loaded under that lock
//! (`io::update_tracked_with`), so the file is read once.
//!
//! Unlike the cache entry points, the class needs no single-copy guard
//! ([`crate::single_copy`]): it uses no SQLite, and the inventory lock is
//! taken on a file descriptor of its own by every call, so two copies of
//! the addon exclude each other like two processes do.
//!
//! | `err.code` | When |
//! |---|---|
//! | `InvalidArg` | a bad argument or record (wrong type, unknown key, a value the inventory rules refuse) |
//! | `InventoryInvalid` | the file is not a valid inventory |
//! | `InventoryUnsupported` | the file was written by a newer apvm, in a form this one cannot read (also refusing a forced save over it) |
//! | `InventoryReadOnly` | a change to (or save of) an inventory written by a newer apvm, or a forced save over one |
//! | `InventoryIo` | the file, its lock or its directory cannot be read, written or created, or is not a regular file; the path names no file |
//! | `InventoryLocked` | another writer held the lock for 10 s (including a save or update of the same file inside an `update()` callback); no inventory data was written |
//! | `InventoryChanged` | `save()`: the file changed since it was loaded; no inventory data was written |
//!
//! "No inventory data was written" is exact for the file's content; a
//! refused save or update may still have created the lock file
//! `<file>.lock` and missing directories (`0700`) when the refusal came
//! after the lock was taken (a busy lock, a change seen only under the
//! lock). Targets no write can use (a path that names no file, a symbolic
//! link, a directory) are refused before anything is created.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use apvm_sites::inventory::io::{self, Fingerprint, TrackedUpdate};
use apvm_sites::{Error as SitesError, Inventory, layout};
use napi::bindgen_prelude::{
    ClassInstance, Function, JavaScriptClassExt, JsObjectValue, JsValue, Object, Unknown,
};
use napi::{Env, ValueType};
use napi_derive::napi;

use crate::config::read_process_env;
use crate::error::{
    OrInvalidArg, clear_pending_exception, invalid_arg, js_error, sites_error_code,
};
use crate::sites_input;
use crate::sites_types::{JsDefaults, JsGroup, JsServer, JsSite};

/// `update()`'s callback as napi-rs calls it: one object argument.
type Callback<'env> = Function<'env, Object<'env>, Unknown<'env>>;

thread_local! {
    /// The files whose `update()` callback is running on this thread,
    /// innermost last: a save or update of one of them waits for the lock
    /// that `update()` holds, and its `InventoryLocked` says so.
    static UPDATING: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

/// The file an object is tied to, and what it held when the object last
/// loaded or saved it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tracked {
    /// The inventory file, absolute.
    path: PathBuf,
    /// What the file held then.
    fingerprint: Fingerprint,
}

/// The site inventory: servers, sites, groups and default SSH settings,
/// always valid, plus the file it came from.
///
/// Get one with `SiteInventory.load(path?)`, `SiteInventory.empty()` or
/// `SiteInventory.update(fn, path?)`. Edits change only this object until
/// `save()`. Records are plain objects in the camelCase form of
/// `inventory.json`; an `upsert…()` replaces the whole record, so spread the
/// current one to change a single field.
///
/// The default file is `APVM_INVENTORY` (read from `process.env` at each
/// call; set and non-empty), else `~/.apvm/inventory.json`, the CLI's.
///
/// # TypeScript
///
/// ```typescript
/// const inventory = SiteInventory.load();
/// inventory.upsertServer({ id: 'eu-1', host: '203.0.113.10' });
/// inventory.upsertSite({ id: 'shop', server: 'eu-1', path: '/var/www/shop' });
/// try {
///   inventory.save();
/// } catch (e) {
///   if ((e as { code?: string }).code !== 'InventoryChanged') throw e;
///   // Another process saved meanwhile: reload, reapply, save.
/// }
///
/// // Or edit in one locked step, which never conflicts:
/// SiteInventory.update((inv) => inv.upsertGroup({ id: 'qa', sites: ['shop'] }));
/// ```
#[napi]
pub struct SiteInventory {
    /// The inventory, valid by construction.
    inventory: Inventory,
    /// The file it is tied to; `None` for `empty()` until its first save.
    tracked: Option<Tracked>,
}

#[napi]
impl SiteInventory {
    /// Load the inventory file: `path`, else `APVM_INVENTORY`, else
    /// `~/.apvm/inventory.json`. A missing file is an empty inventory; nothing
    /// is created. The object remembers the file (made absolute, so a later
    /// `process.chdir()` changes nothing) and what it held, for `save()`.
    ///
    /// # Throws
    ///
    /// `InvalidArg` (`path` is not a string); `InventoryInvalid`,
    /// `InventoryUnsupported`, `InventoryIo` (also a path that names no
    /// file: empty, or ending with a separator, `.` or `..`; or a file name
    /// ending with `.` or a space).
    #[napi(factory, ts_args_type = "path?: string | undefined | null")]
    pub fn load(env: &Env, path: Option<Unknown<'_>>) -> napi::Result<Self> {
        let path = sites_input::read_path(path, "SiteInventory.load()").or_invalid_arg(env)?;
        let path = target_path(env, path)?;
        let (inventory, fingerprint) =
            io::load_tracked(&path).map_err(|err| sites_error(env, &err))?;
        Ok(Self {
            inventory,
            tracked: Some(Tracked { path, fingerprint }),
        })
    }

    /// An empty inventory tied to no file. Its first `save()` creates the
    /// file and, unless forced, refuses to replace one that exists.
    #[napi(factory)]
    pub fn empty() -> Self {
        Self {
            inventory: Inventory::new(),
            tracked: None,
        }
    }

    /// The file this object was loaded from or last saved to (absolute), or
    /// `null` for `empty()` before its first save.
    #[napi(getter)]
    pub fn path(&self) -> Option<String> {
        self.tracked
            .as_ref()
            .map(|tracked| tracked.path.to_string_lossy().into_owned())
    }

    /// Whether the inventory was written by a newer apvm: it can be read but
    /// not changed or saved (`InventoryReadOnly`).
    #[napi(getter)]
    pub fn read_only(&self) -> bool {
        self.inventory.is_read_only()
    }

    /// Settings every server inherits.
    #[napi]
    pub fn defaults(&self) -> JsDefaults {
        JsDefaults::from(self.inventory.defaults())
    }

    /// Replace the default settings.
    ///
    /// # Throws
    ///
    /// `InvalidArg`; `InventoryReadOnly`.
    #[napi(ts_args_type = "defaults: JsDefaults")]
    pub fn set_defaults(&mut self, env: &Env, defaults: Unknown<'_>) -> napi::Result<()> {
        let defaults = sites_input::read_defaults(defaults).or_invalid_arg(env)?;
        self.edit(env, |inventory| inventory.set_defaults(defaults).map(drop))
    }

    /// Every server, in inventory order.
    #[napi]
    pub fn servers(&self) -> Vec<JsServer> {
        self.inventory
            .servers()
            .iter()
            .map(JsServer::from)
            .collect()
    }

    /// Add a server, or replace the one with the same id.
    ///
    /// # Throws
    ///
    /// `InvalidArg` (including a value the inventory rules refuse);
    /// `InventoryReadOnly`.
    #[napi(ts_args_type = "server: JsServer")]
    pub fn upsert_server(&mut self, env: &Env, server: Unknown<'_>) -> napi::Result<()> {
        let server = sites_input::read_server(server).or_invalid_arg(env)?;
        self.edit(env, |inventory| match inventory.server(&server.id) {
            Some(_) => inventory.update_server(server).map(drop),
            None => inventory.add_server(server),
        })
    }

    /// Remove a server. While sites are on it, the removal is refused unless
    /// `cascade` is `true`: then those sites go too, and leave their groups.
    ///
    /// # Returns
    ///
    /// `false` when there is no such server.
    ///
    /// # Throws
    ///
    /// `InvalidArg` (bad arguments, or sites still on the server);
    /// `InventoryReadOnly`.
    #[napi(ts_args_type = "id: string, cascade?: boolean | undefined | null")]
    pub fn remove_server(
        &mut self,
        env: &Env,
        id: Unknown<'_>,
        cascade: Option<Unknown<'_>>,
    ) -> napi::Result<bool> {
        const WHAT: &str = "removeServer()";
        let id = sites_input::read_id(id, WHAT).or_invalid_arg(env)?;
        let cascade = sites_input::read_flag(cascade, WHAT, "cascade").or_invalid_arg(env)?;
        self.remove(env, |inventory| {
            inventory.remove_server(&id, cascade).map(drop)
        })
    }

    /// Every site, in inventory order.
    #[napi]
    pub fn sites(&self) -> Vec<JsSite> {
        self.inventory.sites().iter().map(JsSite::from).collect()
    }

    /// Add a site, or replace the one with the same id (it may move to
    /// another server). Its server must exist.
    ///
    /// # Throws
    ///
    /// `InvalidArg` (including a value the inventory rules refuse or an
    /// unknown server); `InventoryReadOnly`.
    #[napi(ts_args_type = "site: JsSite")]
    pub fn upsert_site(&mut self, env: &Env, site: Unknown<'_>) -> napi::Result<()> {
        let site = sites_input::read_site(site).or_invalid_arg(env)?;
        self.edit(env, |inventory| match inventory.site(&site.id) {
            Some(_) => inventory.update_site(site).map(drop),
            None => inventory.add_site(site),
        })
    }

    /// Remove a site; it also leaves every group that lists it.
    ///
    /// # Returns
    ///
    /// `false` when there is no such site.
    ///
    /// # Throws
    ///
    /// `InvalidArg`; `InventoryReadOnly`.
    #[napi(ts_args_type = "id: string")]
    pub fn remove_site(&mut self, env: &Env, id: Unknown<'_>) -> napi::Result<bool> {
        let id = sites_input::read_id(id, "removeSite()").or_invalid_arg(env)?;
        self.remove(env, |inventory| inventory.remove_site(&id).map(drop))
    }

    /// Every group, in inventory order.
    #[napi]
    pub fn groups(&self) -> Vec<JsGroup> {
        self.inventory.groups().iter().map(JsGroup::from).collect()
    }

    /// Add a group, or replace the one with the same id. Its sites must
    /// exist, each listed once.
    ///
    /// # Throws
    ///
    /// `InvalidArg`; `InventoryReadOnly`.
    #[napi(ts_args_type = "group: JsGroup")]
    pub fn upsert_group(&mut self, env: &Env, group: Unknown<'_>) -> napi::Result<()> {
        let group = sites_input::read_group(group).or_invalid_arg(env)?;
        self.edit(env, |inventory| match inventory.group(&group.id) {
            Some(_) => inventory.update_group(group).map(drop),
            None => inventory.add_group(group),
        })
    }

    /// Remove a group; its sites stay.
    ///
    /// # Returns
    ///
    /// `false` when there is no such group.
    ///
    /// # Throws
    ///
    /// `InvalidArg`; `InventoryReadOnly`.
    #[napi(ts_args_type = "id: string")]
    pub fn remove_group(&mut self, env: &Env, id: Unknown<'_>) -> napi::Result<bool> {
        let id = sites_input::read_id(id, "removeGroup()").or_invalid_arg(env)?;
        self.remove(env, |inventory| inventory.remove_group(&id).map(drop))
    }

    /// Save the inventory to `path`, else to the file this object tracks,
    /// else to the default (`APVM_INVENTORY`, else `~/.apvm/inventory.json`).
    ///
    /// Without `force`, the save writes only if the file still holds what
    /// this object last loaded or saved there, or, for any other file, if
    /// it does not exist yet: `empty().save()` never replaces an inventory.
    /// Otherwise it throws `InventoryChanged`, writes no inventory data
    /// (see the module docs for the lock file), and the object keeps its
    /// changes: load again, reapply them, and save. Paths are compared once
    /// made absolute (see [`absolute`]), not resolved: another spelling of
    /// the tracked file (a symbolic link, `/tmp` vs `/private/tmp` on macOS)
    /// counts as another file, so its save is refused (safe).
    ///
    /// With `force: true` the file is replaced whatever it holds (e.g. to
    /// restore a backup over a damaged file), discarding any change another
    /// process made, except an inventory a newer apvm wrote: that one is
    /// never replaced, with the code `update()` gives for it
    /// (`InventoryReadOnly` when this build reads it, else
    /// `InventoryUnsupported`), and the message says to move the file aside
    /// by hand to restore a backup over it. The file is read first to
    /// check that, so one this process cannot read (no read permission) is
    /// refused too (`InventoryIo`; make it readable or move it aside). A
    /// `version` that is not a whole number (`"2"`, `2.0`, `-2`, larger
    /// than 4294967295) makes the file damaged, so it is replaced; with
    /// duplicate `version` keys the last one counts.
    ///
    /// On success the object tracks the file it wrote and what it wrote, so
    /// saves chain.
    ///
    /// # Returns
    ///
    /// The absolute path written.
    ///
    /// # Throws
    ///
    /// `InventoryChanged`; `InventoryReadOnly` (always, for an inventory
    /// written by a newer apvm; with `force`, also for a target holding
    /// one); `InventoryUnsupported` (with `force`, a target holding a newer
    /// inventory this build cannot read); `InventoryLocked`; `InventoryIo`
    /// (also a path that names no file: empty, or ending with a separator,
    /// `.` or `..`; a file name ending with `.` or a space; with `force`, a
    /// target that cannot be read); `InvalidArg`
    /// for bad arguments.
    #[napi(
        ts_args_type = "path?: string | undefined | null, options?: SaveOptions | undefined | null"
    )]
    pub fn save(
        &mut self,
        env: &Env,
        path: Option<Unknown<'_>>,
        options: Option<Unknown<'_>>,
    ) -> napi::Result<String> {
        let path = sites_input::read_path(path, "save()").or_invalid_arg(env)?;
        let force = sites_input::read_save_force(options).or_invalid_arg(env)?;
        let target = self.save_target(env, path)?;
        // A refusal of the target's content, not of this object's own.
        let forced = force && !self.inventory.is_read_only();
        let fingerprint = self
            .write(&target, force)
            .map_err(|err| file_error(env, &err, &target, forced))?;
        let saved = target.to_string_lossy().into_owned();
        self.tracked = Some(Tracked {
            path: target,
            fingerprint,
        });
        Ok(saved)
    }

    /// Edit the inventory file in one locked step: load it (`path`, else
    /// `APVM_INVENTORY`, else `~/.apvm/inventory.json`), call `fn` with it,
    /// and save what `fn` left, all while holding the file's lock. Concurrent
    /// `update()`s from threads, workers or processes (the CLI included) all
    /// land, in turn; none conflicts.
    ///
    /// `fn` must be synchronous: a function that returns a promise (or any
    /// other thenable: an object or function with a callable `then`) is
    /// refused with `InvalidArg`. A rejection of that promise is handled
    /// (ignored), so an async `fn` that throws never becomes an unhandled
    /// rejection: the handler is attached with the `Promise.prototype.then`
    /// captured when the addon loaded, so a promise subclass overriding
    /// `then` or `catch`, or a later replaced `Promise.prototype.then`,
    /// cannot prevent it. Only a subclass whose species constructor throws
    /// can (see [`crate::intrinsics`]). The file is written only when `fn` returns and changed
    /// something. If `fn` throws, nothing is written and the exception
    /// propagates unchanged.
    ///
    /// When `update()` writes nothing (`fn` threw or was refused), the
    /// object `fn` received still holds whatever edits `fn` made, and
    /// tracks the file as `update()` read it: if `fn` kept it, its `save()`
    /// is guarded like any save (it writes only if the file still holds
    /// what `update()` read).
    ///
    /// Keep `fn` quick: other writers wait for it, for at most 10 s. Inside
    /// `fn`, saving or updating the same file (`inventory.save()`,
    /// `SiteInventory.update(…, samePath)`) waits for the lock `update` holds
    /// and throws `InventoryLocked` after 10 s; it never deadlocks.
    ///
    /// # Returns
    ///
    /// The object `fn` received, now tracking the saved file, ready for
    /// more edits and `save()`.
    ///
    /// # Throws
    ///
    /// What `fn` throws (and what reading the `then` of the value it
    /// returned throws); `InvalidArg` (`fn` is not a function, or returned
    /// a thenable; `path` is not a string); `InventoryLocked`;
    /// `InventoryReadOnly` (`fn` changed an inventory written by a newer
    /// apvm); `InventoryInvalid`, `InventoryUnsupported`, `InventoryIo`.
    /// Never `InventoryChanged`: the file is read and written under one
    /// lock.
    #[napi(
        ts_args_type = "fn: (inventory: SiteInventory) => void, path?: string | undefined | null",
        ts_return_type = "SiteInventory"
    )]
    pub fn update<'env>(
        env: &'env Env,
        callback: Unknown<'env>,
        path: Option<Unknown<'_>>,
    ) -> napi::Result<ClassInstance<'env, SiteInventory>> {
        let callback = sites_input::read_callback(callback).or_invalid_arg(env)?;
        let path = sites_input::read_path(path, "SiteInventory.update()").or_invalid_arg(env)?;
        let target = target_path(env, path)?;
        let outcome = io::update_tracked_with(&target, |inventory, loaded| {
            let tracked = Tracked {
                path: target.clone(),
                fingerprint: loaded.clone(),
            };
            run_callback(env, &callback, tracked, inventory)
        });
        finish_update(env, outcome, target)
    }
}

impl SiteInventory {
    /// The file `save(path)` writes: `path` (made absolute), else the
    /// tracked file, else the default.
    ///
    /// # Errors
    ///
    /// Those of [`target_path`].
    fn save_target(&self, env: &Env, path: Option<String>) -> napi::Result<PathBuf> {
        match (path, &self.tracked) {
            (None, Some(tracked)) => Ok(tracked.path.clone()),
            (path, _) => target_path(env, path),
        }
    }

    /// Write the inventory to `target`: guarded by what this object last saw
    /// there, or blindly (`force`) except over a newer inventory.
    ///
    /// # Returns
    ///
    /// The fingerprint of what was written.
    ///
    /// # Errors
    ///
    /// The library's refusals (see `save()`).
    fn write(&self, target: &Path, force: bool) -> apvm_sites::Result<Fingerprint> {
        if force {
            return io::save(target, &self.inventory);
        }
        let expected = expected_fingerprint(self.tracked.as_ref(), target);
        io::save_if_unchanged(target, &self.inventory, &expected)
    }

    /// Apply `change` to the inventory (which changes nothing when it fails).
    ///
    /// # Errors
    ///
    /// The change's error, with its `err.code`.
    fn edit(
        &mut self,
        env: &Env,
        change: impl FnOnce(&mut Inventory) -> apvm_sites::Result<()>,
    ) -> napi::Result<()> {
        change(&mut self.inventory).map_err(|err| sites_error(env, &err))
    }

    /// Apply a removal; a missing record is `false`, not an error.
    ///
    /// # Errors
    ///
    /// The removal's other errors, with their `err.code`.
    fn remove(
        &mut self,
        env: &Env,
        removal: impl FnOnce(&mut Inventory) -> apvm_sites::Result<()>,
    ) -> napi::Result<bool> {
        removed(removal(&mut self.inventory)).map_err(|err| sites_error(env, &err))
    }
}

// =============================================================================
// update(): calling `fn` under the lock
// =============================================================================

/// Why an `update()` wrote nothing.
enum UpdateError {
    /// The library refused (lock, file, rules).
    Sites(SitesError),
    /// The callback was refused: it returned a promise or another thenable.
    Thenable,
    /// A JS call failed; when `fn` threw, its exception is still pending, so
    /// returning this error rethrows it unchanged.
    Js(napi::Error),
}

impl From<SitesError> for UpdateError {
    /// A library error.
    fn from(err: SitesError) -> Self {
        Self::Sites(err)
    }
}

impl From<napi::Error> for UpdateError {
    /// A failed JS call.
    fn from(err: napi::Error) -> Self {
        Self::Js(err)
    }
}

/// What `update()` returns: on success the object `fn` received, holding
/// the saved inventory and tracking the file with what it holds now.
///
/// # Errors
///
/// The library's refusal with its `err.code`, a refused callback
/// (`InvalidArg`), or `fn`'s own exception, rethrown as is.
fn finish_update<'env>(
    env: &Env,
    outcome: Result<TrackedUpdate<ClassInstance<'env, SiteInventory>>, UpdateError>,
    target: PathBuf,
) -> napi::Result<ClassInstance<'env, SiteInventory>> {
    match outcome {
        Ok(updated) => {
            let mut instance = updated.value;
            instance.inventory = updated.inventory;
            instance.tracked = Some(Tracked {
                path: target,
                fingerprint: updated.fingerprint,
            });
            Ok(instance)
        }
        Err(UpdateError::Sites(err)) => Err(file_error(env, &err, &target, false)),
        Err(UpdateError::Thenable) => Err(invalid_arg(env, REFUSED_THENABLE)),
        Err(UpdateError::Js(err)) => Err(err),
    }
}

/// Inside the lock: hand `fn` a `SiteInventory` over the locked copy, then
/// keep what it left.
///
/// The object `fn` gets tracks the file with the fingerprint of the bytes
/// the library loaded under the lock (`tracked`), so a `save()` of it inside
/// `fn` waits for the lock and fails with `InventoryLocked`, as the docs
/// say, and one after a failed `update()` is guarded like any save.
///
/// `fn` is called with `apply`, which leaves a thrown exception pending:
/// returning its error out of the method makes napi-rs rethrow the
/// pending exception itself, so any thrown value (an `Error`, a string,
/// `undefined`) reaches the caller unchanged. No JS call is made after it
/// threw.
///
/// # Errors
///
/// See [`UpdateError`].
fn run_callback<'env>(
    env: &'env Env,
    callback: &Callback<'env>,
    tracked: Tracked,
    inventory: &mut Inventory,
) -> Result<ClassInstance<'env, SiteInventory>, UpdateError> {
    let path = tracked.path.clone();
    let locked = SiteInventory {
        inventory: inventory.clone(),
        tracked: Some(tracked),
    };
    let instance = locked.into_instance(env)?;
    let returned = {
        let _updating = Updating::enter(path);
        callback.apply((), instance.as_object(env))?
    };
    if returns_later(env, returned)? {
        return Err(UpdateError::Thenable);
    }
    inventory.clone_from(&instance.inventory);
    Ok(instance)
}

/// Why `update()` refuses a callback that returned a thenable.
const REFUSED_THENABLE: &str = "SiteInventory.update(): `fn` returned a promise (or another \
    thenable); it must be synchronous, since edits after an `await` would run outside the \
    lock. update() wrote nothing; the object `fn` received keeps the edits it made, and a \
    save() of it is guarded like any save";

/// Whether `fn` returned a value that settles later: a promise, or any
/// thenable (an object or function with a callable `then`), which
/// `update()` refuses. A native promise first gets a rejection handler that
/// ignores the reason: an `async fn` that throws (before or after an
/// `await`) must not become an unhandled rejection, which ends the process
/// under Node's default policy.
///
/// # Errors
///
/// What reading `then` throws (pending: rethrown as is).
fn returns_later(env: &Env, returned: Unknown<'_>) -> napi::Result<bool> {
    if returned.is_promise()? {
        ignore_rejection(env, returned);
        return Ok(true);
    }
    if !matches!(
        returned.get_type()?,
        ValueType::Object | ValueType::Function
    ) {
        return Ok(false);
    }
    let then: Unknown<'_> = returned.coerce_to_object()?.get_named_property("then")?;
    Ok(then.get_type()? == ValueType::Function)
}

/// Handle a native promise's rejection (ignore it), best effort, with the
/// `Promise.prototype.then` captured when the addon loaded (see
/// [`crate::intrinsics`]): a `then` or `catch` the promise overrides is
/// never called. If even that fails (the promise's species constructor,
/// user code, threw), its exception is cleared and the refusal goes on.
fn ignore_rejection(env: &Env, promise: Unknown<'_>) {
    if crate::intrinsics::ignore_rejection(env, promise).is_err() {
        clear_pending_exception(env);
    }
}

/// Marks a file's `update()` callback as running on this thread, from
/// [`Updating::enter`] until dropped (also when `fn` throws).
struct Updating;

impl Updating {
    /// Record that `path`'s `update()` callback is running on this thread.
    fn enter(path: PathBuf) -> Self {
        UPDATING.with_borrow_mut(|paths| paths.push(path));
        Self
    }

    /// Whether an `update()` callback of `path` is running on this thread
    /// (the file is compared as spelled, made absolute).
    fn holds(path: &Path) -> bool {
        UPDATING.with_borrow(|paths| paths.iter().any(|held| held == path))
    }
}

impl Drop for Updating {
    /// The callback returned or threw: forget the innermost file.
    fn drop(&mut self) {
        UPDATING.with_borrow_mut(|paths| paths.pop());
    }
}

// =============================================================================
// Paths
// =============================================================================

/// The inventory file a call works on: `path`, else `APVM_INVENTORY` from
/// the calling thread's `process.env`, else `~/.apvm/inventory.json`;
/// checked as given (see `io::check_path`), then made absolute.
///
/// # Errors
///
/// `InventoryIo` when the path names no file (empty, `/`, or ending with a
/// separator, `.` or `..`), its file name ends with `.` or a space, it
/// cannot be made absolute, or, with no path
/// given, the home directory is unknown.
fn target_path(env: &Env, path: Option<String>) -> napi::Result<PathBuf> {
    let path = match path {
        Some(path) => PathBuf::from(path),
        None => default_target(env)?,
    };
    // Before `absolute()`, which would turn `x/.` into `x` (and, on
    // Windows, `x/..` into the parent): the CLI refuses those paths too.
    io::check_path(&path).map_err(|err| sites_error(env, &err))?;
    absolute(&path).map_err(|source| {
        let err = SitesError::InventoryIo {
            context: "cannot make the inventory path absolute",
            path,
            source,
        };
        sites_error(env, &err)
    })
}

/// `APVM_INVENTORY` (from `process.env`), else `~/.apvm/inventory.json`.
///
/// # Errors
///
/// `InventoryIo` when the variable is unset and the home directory is
/// unknown.
fn default_target(env: &Env) -> napi::Result<PathBuf> {
    if let Some(path) = js_inventory_override(env) {
        return Ok(path);
    }
    default_inventory_path(home_dir()).ok_or_else(|| {
        js_error(
            env,
            "InventoryIo",
            "cannot locate the default inventory: the home directory is unknown; \
             pass a path or set APVM_INVENTORY",
        )
    })
}

/// `APVM_INVENTORY` from the calling thread's `process.env` (`None` when
/// unset or empty, as in the library); the process environment where there
/// is no readable `process.env`. A worker thread's `process.env` is its own
/// copy, which the process environment never sees.
fn js_inventory_override(env: &Env) -> Option<PathBuf> {
    match read_process_env(env, io::INVENTORY_ENV) {
        Ok(value) => non_empty(value).map(PathBuf::from),
        Err(_) => {
            // A throwing `process.env` (e.g. a getter) leaves its exception
            // pending; the fallback is the answer, so it must not surface
            // from a call that goes on.
            clear_pending_exception(env);
            io::env_override()
        }
    }
}

/// `value` unless it is empty: an accidental `APVM_INVENTORY=` must not
/// move the inventory to the current directory.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

/// The user's home directory, when known.
fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

/// The CLI's inventory file under `home`: `<home>/.apvm/inventory.json`.
fn default_inventory_path(home: Option<PathBuf>) -> Option<PathBuf> {
    home.map(|home| home.join(".apvm").join(layout::INVENTORY_FILE_NAME))
}

/// `path` made absolute against the current directory, as
/// `std::path::absolute` does, so a later `process.chdir()` changes
/// nothing. Symbolic links are never resolved. Per platform:
///
/// - Unix: `.` components and doubled separators are dropped; `..` stays as
///   written (`/a/../x` stays, since `a` may be a link).
/// - Windows: the path is normalized as `GetFullPathNameW` does: `.` and
///   `..` are resolved lexically, `/` becomes `\`, and a path rooted
///   without a drive (`\x.json`) gets the current drive.
///
/// # Errors
///
/// When the current directory is unknown (deleted) or the path is empty.
fn absolute(path: &Path) -> std::io::Result<PathBuf> {
    std::path::absolute(path)
}

/// What the file at `target` must hold for a guarded save: what this object
/// last loaded or saved there, or nothing when it tracks another file (or
/// none), so a save never replaces a file it has not seen.
fn expected_fingerprint(tracked: Option<&Tracked>, target: &Path) -> Fingerprint {
    match tracked {
        Some(tracked) if tracked.path == target => tracked.fingerprint.clone(),
        _ => Fingerprint::missing(),
    }
}

// =============================================================================
// Errors
// =============================================================================

/// A removal's outcome: `true` when removed, `false` when there was no such
/// record.
///
/// # Errors
///
/// The removal's other errors.
fn removed(outcome: apvm_sites::Result<()>) -> apvm_sites::Result<bool> {
    match outcome {
        Ok(()) => Ok(true),
        Err(SitesError::NotFound { .. }) => Ok(false),
        Err(err) => Err(err),
    }
}

/// What a refused call was doing, for its hint (see [`error_message`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Situation {
    /// The call ran inside an `update()` callback of the same file, on this
    /// thread (so it waited for that lock).
    inside_update: bool,
    /// The call was a forced save of a writable inventory (so a read-only
    /// or unsupported refusal is about the target's content).
    forced_save: bool,
}

/// A library error as a JS `Error` with its `err.code` (see
/// [`sites_error_code`]) and message (see [`error_message`]), for a call
/// that does not write the file (loading, editing records).
fn sites_error(env: &Env, err: &SitesError) -> napi::Error {
    let message = error_message(err, Situation::default());
    js_error(env, &sites_error_code(err.code()), &message)
}

/// [`sites_error`] for a call that writes `target` (`save()`,
/// `update()`): its `InventoryLocked` names the cause when an `update()`
/// callback of the same file is running on this thread, and a forced save
/// refused over a newer inventory (`forced_save`) says how to restore a
/// backup anyway.
fn file_error(env: &Env, err: &SitesError, target: &Path, forced_save: bool) -> napi::Error {
    let situation = Situation {
        inside_update: Updating::holds(target),
        forced_save,
    };
    js_error(
        env,
        &sites_error_code(err.code()),
        &error_message(err, situation),
    )
}

/// The message of a library error, plus the next step in JS terms where
/// there is one (library messages name no front-end remedy).
///
/// # Arguments
///
/// * `err` - The library error
/// * `situation` - What the refused call was doing
fn error_message(err: &SitesError, situation: Situation) -> String {
    let newer = matches!(
        err.code(),
        apvm_sites::Code::InventoryReadOnly | apvm_sites::Code::InventoryUnsupported
    );
    let hint = match err {
        SitesError::InventoryChanged { .. } => "Load it again, reapply your edits and save.",
        SitesError::InventoryLocked { .. } if situation.inside_update => {
            "This call ran inside an update() callback of the same file, which holds that lock \
             until the callback returns: edit the object the callback received instead; \
             update() saves it."
        }
        SitesError::InventoryIo { context, .. } if *context == io::VERSION_CHECK_CONTEXT => {
            "A forced save reads the file it replaces first, so it never overwrites an \
             inventory a newer apvm wrote unseen: make the file readable, or move it aside by \
             hand, then save again."
        }
        _ if newer && situation.forced_save => {
            "Even a forced save never replaces an inventory written by a newer apvm. To restore \
             a backup over it, move that file aside by hand first, then save again."
        }
        SitesError::ServerInUse { .. } => "Pass cascade = true to remove those sites too.",
        _ => return err.to_string(),
    };
    format!("{err}\n  {hint}")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use apvm_sites::{Code, EntityKind, Server};

    use super::*;

    /// Inside an `update()` callback of the same file.
    const INSIDE: Situation = Situation {
        inside_update: true,
        forced_save: false,
    };
    /// A forced save of a writable inventory.
    const FORCED: Situation = Situation {
        inside_update: false,
        forced_save: true,
    };

    #[test]
    fn a_forced_save_that_cannot_read_the_file_says_what_to_do() {
        let err = SitesError::InventoryIo {
            context: io::VERSION_CHECK_CONTEXT,
            path: PathBuf::from("/x/inventory.json"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let message = error_message(&err, FORCED);
        assert!(message.starts_with(&err.to_string()), "{message}");
        assert!(
            message.ends_with("make the file readable, or move it aside by hand, then save again."),
            "{message}"
        );
        // Other read failures get no such hint.
        let other = SitesError::InventoryIo {
            context: "cannot read the inventory",
            path: PathBuf::from("/x/inventory.json"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        assert_eq!(error_message(&other, FORCED), other.to_string());
    }

    #[test]
    fn a_forced_save_over_a_newer_inventory_says_how_to_restore_a_backup() {
        let read_only = SitesError::InventoryReadOnly {
            found: 2,
            supported: 1,
        };
        let unsupported = SitesError::InventoryFile {
            path: PathBuf::from("/x/inventory.json"),
            source: Box::new(SitesError::InventoryUnsupported {
                found: 2,
                supported: 1,
                source: Box::new(SitesError::InventoryInvalid {
                    problems: Vec::new(),
                }),
            }),
        };
        for err in [&read_only, &unsupported] {
            let message = error_message(err, FORCED);
            assert!(message.starts_with(&err.to_string()), "{message}");
            assert!(
                message.ends_with("move that file aside by hand first, then save again."),
                "{message}"
            );
            // Not for other calls: the object itself may be the read-only one.
            assert_eq!(error_message(err, Situation::default()), err.to_string());
            assert_eq!(error_message(err, INSIDE), err.to_string());
        }
        let changed = SitesError::InventoryChanged {
            path: PathBuf::from("/x"),
        };
        assert!(!error_message(&changed, FORCED).contains("aside"));
    }

    /// The fingerprint of a real file holding `servers` servers, from the
    /// library (fingerprints are opaque).
    fn saved_fingerprint(dir: &Path, servers: usize) -> Fingerprint {
        let path = dir.join(format!("inventory-{servers}.json"));
        let mut inventory = Inventory::new();
        for n in 0..servers {
            inventory
                .add_server(Server::new(format!("s{n}"), "h"))
                .unwrap();
        }
        io::save(&path, &inventory).unwrap()
    }

    #[test]
    fn conflicts_name_the_js_remedy_without_forcing() {
        let changed = SitesError::InventoryChanged {
            path: PathBuf::from("/x/inventory.json"),
        };
        for inside_update in [false, true] {
            let situation = Situation {
                inside_update,
                forced_save: false,
            };
            let message = error_message(&changed, situation);
            assert!(message.starts_with(&changed.to_string()), "{message}");
            assert!(
                message.ends_with("\n  Load it again, reapply your edits and save."),
                "{message}"
            );
            // Forcing would discard the other process's change: never offered.
            assert!(!message.contains("force"), "{message}");
        }
    }

    #[test]
    fn a_lock_names_the_update_callback_only_when_inside_one() {
        let locked = SitesError::InventoryLocked {
            path: PathBuf::from("/x/inventory.json.lock"),
            waited: Duration::from_secs(10),
        };
        let outside = error_message(&locked, Situation::default());
        assert_eq!(outside, locked.to_string());
        assert!(outside.contains("held it for 10 s"), "{outside}");
        let inside = error_message(&locked, INSIDE);
        assert!(inside.starts_with(&outside), "{inside}");
        assert!(
            inside.contains("This call ran inside an update() callback of the same file"),
            "{inside}"
        );
    }

    #[test]
    fn a_server_in_use_names_cascade() {
        let in_use = SitesError::ServerInUse {
            id: "eu-1".into(),
            sites: vec!["shop".into()],
        };
        assert!(
            error_message(&in_use, Situation::default())
                .ends_with("Pass cascade = true to remove those sites too.")
        );
    }

    #[test]
    fn other_messages_pass_through_unchanged() {
        let err = SitesError::NotFound {
            kind: EntityKind::Site,
            id: "shop".into(),
        };
        for situation in [Situation::default(), INSIDE, FORCED] {
            assert_eq!(error_message(&err, situation), err.to_string());
        }
    }

    #[test]
    fn update_callbacks_are_tracked_per_file_and_nested() {
        let outer = PathBuf::from("/x/outer.json");
        let inner = PathBuf::from("/x/inner.json");
        assert!(!Updating::holds(&outer));
        {
            let _outer = Updating::enter(outer.clone());
            assert!(Updating::holds(&outer));
            assert!(!Updating::holds(&inner));
            {
                let _inner = Updating::enter(inner.clone());
                assert!(Updating::holds(&outer) && Updating::holds(&inner));
            }
            assert!(!Updating::holds(&inner));
            assert!(Updating::holds(&outer));
        }
        assert!(!Updating::holds(&outer));
        // Per thread: another thread sees none of this one's callbacks.
        let _held = Updating::enter(outer.clone());
        let seen = std::thread::spawn(move || Updating::holds(&outer))
            .join()
            .unwrap();
        assert!(!seen);
    }

    #[test]
    fn a_missing_record_is_not_removed_and_not_an_error() {
        assert_eq!(removed(Ok(())).ok(), Some(true));
        let missing = SitesError::NotFound {
            kind: EntityKind::Group,
            id: "qa".into(),
        };
        assert_eq!(removed(Err(missing)).ok(), Some(false));
        let in_use = SitesError::ServerInUse {
            id: "eu-1".into(),
            sites: vec!["shop".into()],
        };
        assert_eq!(
            removed(Err(in_use)).map_err(|e| e.code()),
            Err(Code::InvalidInput)
        );
    }

    #[test]
    fn a_save_to_the_tracked_file_expects_what_it_held() {
        let dir = tempfile::tempdir().unwrap();
        let fingerprint = saved_fingerprint(dir.path(), 1);
        let tracked = Tracked {
            path: dir.path().join("inventory.json"),
            fingerprint: fingerprint.clone(),
        };
        assert_eq!(
            expected_fingerprint(Some(&tracked), &dir.path().join("inventory.json")),
            fingerprint
        );
        assert!(!fingerprint.is_missing());
    }

    #[test]
    fn a_save_to_another_file_expects_none() {
        let dir = tempfile::tempdir().unwrap();
        let tracked = Tracked {
            path: dir.path().join("inventory.json"),
            fingerprint: saved_fingerprint(dir.path(), 2),
        };
        let other = dir.path().join("other.json");
        assert!(expected_fingerprint(Some(&tracked), &other).is_missing());
        assert!(expected_fingerprint(None, &other).is_missing());
        // Another spelling is another file (safe: the save is refused).
        let spelled = dir.path().join("sub").join("..").join("inventory.json");
        assert!(expected_fingerprint(Some(&tracked), &spelled).is_missing());
    }

    #[test]
    fn tracked_paths_compare_by_component() {
        // Doubled separators and `.` components name the same file.
        let dir = tempfile::tempdir().unwrap();
        let fingerprint = saved_fingerprint(dir.path(), 3);
        let tracked = Tracked {
            path: absolute(&dir.path().join("inventory.json")).unwrap(),
            fingerprint: fingerprint.clone(),
        };
        let spelled = absolute(&dir.path().join(".").join("inventory.json")).unwrap();
        assert_eq!(expected_fingerprint(Some(&tracked), &spelled), fingerprint);
        let doubled = PathBuf::from(format!("{}//inventory.json", dir.path().display()));
        assert_eq!(expected_fingerprint(Some(&tracked), &doubled), fingerprint);
    }

    #[cfg(unix)]
    #[test]
    fn relative_paths_are_pinned_to_the_current_directory() {
        let cwd = std::env::current_dir().unwrap();
        let absolute = |path: &str| absolute(Path::new(path)).unwrap();
        assert_eq!(absolute("inv.json"), cwd.join("inv.json"));
        assert_eq!(absolute("/abs/inv.json"), PathBuf::from("/abs/inv.json"));
        // Not resolved: `..` and links stay as written.
        assert_eq!(absolute("/a/../inv.json"), PathBuf::from("/a/../inv.json"));
        // `.` components and doubled separators go.
        assert_eq!(absolute("/a/./b//inv.json"), PathBuf::from("/a/b/inv.json"));
        // An empty path cannot be made absolute (`check_path` refuses it first).
        assert!(super::absolute(Path::new("")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn relative_paths_are_pinned_to_the_current_directory() {
        let cwd = std::env::current_dir().unwrap();
        let absolute = |path: &str| absolute(Path::new(path)).unwrap();
        assert_eq!(absolute("inv.json"), cwd.join("inv.json"));
        // Rooted without a drive: the current drive is added.
        let rooted = absolute(r"\abs\inv.json");
        assert!(rooted.is_absolute(), "{rooted:?}");
        assert!(rooted.ends_with(r"abs\inv.json"), "{rooted:?}");
        assert_eq!(rooted.components().next(), cwd.components().next());
        // Normalized: `..`, `.` and `/` are resolved lexically.
        assert_eq!(absolute(r"C:\a\..\inv.json"), PathBuf::from(r"C:\inv.json"));
        assert_eq!(absolute("C:/a/./inv.json"), PathBuf::from(r"C:\a\inv.json"));
        assert!(super::absolute(Path::new("")).is_err());
    }

    #[test]
    fn the_default_file_is_the_clis() {
        assert_eq!(
            default_inventory_path(Some(PathBuf::from("/home/me"))),
            Some(PathBuf::from("/home/me/.apvm/inventory.json"))
        );
        assert_eq!(default_inventory_path(None), None);
    }

    #[test]
    fn an_empty_override_is_ignored() {
        assert_eq!(non_empty(None), None);
        assert_eq!(non_empty(Some(String::new())), None);
        assert_eq!(non_empty(Some("/x.json".into())), Some("/x.json".into()));
        assert_eq!(non_empty(Some(" ".into())), Some(" ".into()));
    }
}
