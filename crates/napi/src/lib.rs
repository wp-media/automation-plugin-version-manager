//! APVM Node.js Bindings
//!
//! This crate provides N-API bindings for the APVM (Automation Plugin Version Manager)
//! core library, exposing build operations to Node.js consumers.
//!
//! # Features
//!
//! - **Async/Promise-based**: All build operations return JavaScript Promises,
//!   enabling concurrent builds without blocking the Node.js event loop.
//! - **Progress callbacks**: Build progress events are delivered via a
//!   thread-safe callback function, allowing real-time build monitoring.
//! - **Type-safe**: Full TypeScript type definitions are generated automatically.
//! - **Error-safe**: All Rust errors are converted to JavaScript exceptions
//!   without crashing the Node.js process.
//! - **Cache maintenance**: `ApvmCache` (from `apvm.cache()` or
//!   `ApvmCache.open()`) inspects, cleans, verifies, garbage-collects and
//!   repairs the artifact cache, at parity with `apvm cache`; its rejections
//!   carry an `err.code` (`CacheCorrupted`, `InvalidArg`, `GenericFailure`).
//! - **Site inventory**: `SiteInventory` reads and edits the servers, sites
//!   and groups of `~/.apvm/inventory.json` (or `APVM_INVENTORY`), the
//!   CLI's file. It is synchronous; `save()` never overwrites another
//!   process's edit (`InventoryChanged`), and `SiteInventory.update(fn)`
//!   edits the file in one locked step. Errors carry an `err.code`
//!   (`InvalidArg`, `InventoryChanged`, `InventoryLocked`, …).
//!
//! # Supported Projects
//!
//! - **BackWPup** (`backwpup`) — Private repository, version required
//! - **WP Rocket** (`wp-rocket`) — Public repository, version auto-detected
//! - **Imagify** (`imagify`) — Public repository, version auto-detected
//!
//! # Quick Start (JavaScript/TypeScript)
//!
//! ```typescript
//! import { Apvm, JsCleanTarget, SiteInventory } from 'apvm-napi';
//!
//! // All config fields are optional — cacheDir defaults to ~/.apvm/cache
//! const apvm = await Apvm.create({});
//!
//! const output = await apvm.build(
//!   { project: 'wp-rocket', gitRef: 'pr:456', outputDir: '/tmp/output' },
//!   (err, event) => {
//!     if (!err) console.log(event.type, event.message);
//!   },
//! );
//!
//! console.log(`Built ${output.result.artifacts.length} artifacts`);
//!
//! // Prime the cache without producing output — same pipeline, no outputDir.
//! // A later build of the same ref is then served from the cache.
//! await apvm.warmCache({ project: 'wp-rocket', gitRef: 'branch:develop' });
//!
//! // Maintain the same cache: drop builds unused for 30 days, then check it.
//! const cache = apvm.cache();
//! await cache.clean({ olderThan: '30d', target: JsCleanTarget.Builds });
//! const issues = await cache.verify(); // [] = healthy
//!
//! // Edit the site inventory in one locked step (never conflicts).
//! SiteInventory.update((inventory) => {
//!   inventory.upsertServer({ id: 'eu-1', host: '203.0.113.10' });
//!   inventory.upsertSite({ id: 'shop', server: 'eu-1', path: '/var/www/shop' });
//! });
//! ```

mod apvm;
mod cache;
mod cache_input;
mod cache_types;
mod config;
mod error;
mod intrinsics;
mod progress;
mod single_copy;
mod sites_input;
mod sites_inventory;
mod sites_types;
mod teardown;
mod types;

use napi::Env;
use napi::bindgen_prelude::Object;
use napi_derive::napi;

/// Runs when the addon is loaded into a JS environment (the main thread or
/// a worker): captures the built-ins the `intrinsics` module relies on, before later
/// user code can replace them. A failure must not stop the addon from
/// loading: its exception is cleared, and only what the capture serves
/// (handling the rejection of a promise `SiteInventory.update()` refuses)
/// becomes best effort.
///
/// # Errors
///
/// None: always `Ok`.
#[napi(module_exports)]
pub fn init(_exports: Object<'_>, env: Env) -> napi::Result<()> {
    if intrinsics::capture(&env).is_err() {
        error::clear_pending_exception(&env);
    }
    Ok(())
}
