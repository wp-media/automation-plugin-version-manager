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
//! import { Apvm, type JsCleanTarget } from 'apvm-napi';
//!
//! // All config fields are optional — cacheDir defaults to ~/.apvm/cache
//! const apvm = await Apvm.create({});
//!
//! const output = await apvm.build({
//!   project: 'wp-rocket',
//!   gitRef: 'pr:456',
//!   outputDir: '/tmp/output',
//!   onProgress: (event) => console.log(event.type, event.message),
//! });
//!
//! console.log(`Built ${output.artifacts.length} artifacts`);
//!
//! // Prime the cache without producing output — same pipeline, no outputDir.
//! // A later build of the same ref is then served from the cache.
//! await apvm.warmCache({ project: 'wp-rocket', gitRef: 'branch:develop' });
//!
//! // Maintain the same cache: drop builds unused for 30 days, then check it.
//! const cache = apvm.cache();
//! await cache.clean({ olderThan: '30d', target: 'Builds' as JsCleanTarget });
//! const issues = await cache.verify(); // [] = healthy
//! ```

mod apvm;
mod cache;
mod cache_input;
mod cache_types;
mod config;
mod error;
mod progress;
mod single_copy;
mod types;
