//! Progress reporting bridge between Rust and Node.js.
//!
//! Implements the [`ProgressReporter`] trait using a N-API [`ThreadsafeFunction`],
//! allowing build events to be delivered to a JavaScript callback function
//! from any Rust thread without blocking.
//!
//! The callback is invoked in non-blocking mode, meaning events are queued
//! on the Node.js event loop and delivered asynchronously. This is safe
//! to call from the tokio runtime threads that execute build operations.
//!
//! The callback is best-effort: whatever it throws, and whatever a promise it
//! returns rejects with, is discarded — it never fails the build and never
//! reaches Node's `uncaughtException` / `unhandledRejection` handling.

use apvm_core::build::progress::{BuildEvent, ProgressReporter};
use napi::bindgen_prelude::{CallbackContext, PromiseRaw, Unknown};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Env, JsValue};

use crate::error::clear_pending_exception;
use crate::types::JsBuildEvent;

/// A progress reporter that forwards events to a JavaScript callback.
///
/// This bridges the Rust [`ProgressReporter`] trait with Node.js by using
/// a [`ThreadsafeFunction`] to safely invoke a JS callback from any thread.
///
/// Events are serialized to [`JsBuildEvent`] objects before being sent
/// to the JavaScript side, ensuring all data is safely copied across
/// the thread boundary.
///
/// # Thread Safety
///
/// `ThreadsafeFunction` is `Send + Sync`, so this reporter can be shared
/// across async tasks and tokio worker threads, matching the requirements
/// of the `ProgressReporter` trait.
pub struct JsProgressReporter {
    /// The thread-safe callback function that receives build events.
    callback: ThreadsafeFunction<JsBuildEvent>,
}

impl JsProgressReporter {
    /// Create a new progress reporter wrapping a JavaScript callback.
    ///
    /// # Arguments
    ///
    /// * `callback` - A thread-safe reference to a JavaScript function
    ///   that accepts a single `JsBuildEvent` argument.
    pub fn new(callback: ThreadsafeFunction<JsBuildEvent>) -> Self {
        Self { callback }
    }
}

impl ProgressReporter for JsProgressReporter {
    /// Forward a build event to the JavaScript callback.
    ///
    /// The event is converted to a [`JsBuildEvent`] and queued
    /// on the Node.js event loop via `NonBlocking` mode. This means:
    ///
    /// - The call returns immediately without waiting for JS execution
    /// - Events are delivered in order on the next event loop tick
    /// - If the JS callback throws, or returns a promise that rejects (an
    ///   `async` callback), the error is discarded (not propagated)
    ///
    /// This is intentional — build progress is informational, and a
    /// failing callback should never abort the build itself.
    fn report(&self, event: &BuildEvent) {
        let js_event = JsBuildEvent::from(event);
        // `call_with_return_value`, not `call`: with plain `call`, napi-rs
        // re-raises a throwing callback through `napi_fatal_exception` — an
        // uncaught exception, fatal unless the app installed an
        // `uncaughtException` handler. This variant clears the exception and
        // hands it to the closure as `Err`, which drops it; a returned value
        // arrives as `Ok` and is checked for a promise to settle quietly.
        // The queueing status is ignored for the same reason: progress is
        // best-effort and must never fail the build. (If converting the event
        // itself failed — only under OOM or env teardown — napi-rs would call
        // the callback with an error instead, outside this capture.)
        let _ = self.callback.call_with_return_value(
            Ok(js_event),
            ThreadsafeFunctionCallMode::NonBlocking,
            |outcome, env| {
                if let Ok(returned) = outcome {
                    discard_rejection(&returned, &env);
                }
                Ok(())
            },
        );
    }
}

/// Mark a promise returned by the callback as handled, so its rejection
/// is discarded instead of crashing Node as an unhandled rejection (the
/// default since Node 15). A non-promise return value is left alone.
///
/// Attaching runs JavaScript (the promise's `catch`), which a patched
/// `Promise.prototype` could make throw; any exception that leaves pending
/// is cleared, so this helper itself can never surface one.
///
/// # Arguments
///
/// * `returned` - What the callback returned
/// * `env` - The env of the JS thread the callback just ran on
fn discard_rejection(returned: &Unknown<'_>, env: &Env) {
    let attached = returned.is_promise().and_then(|is_promise| {
        if !is_promise {
            return Ok(());
        }
        PromiseRaw::<Unknown<'_>>::new(env.raw(), returned.raw())
            .catch(|_: CallbackContext<Unknown<'_>>| Ok(()))
            .map(drop)
    });
    if attached.is_err() {
        clear_pending_exception(env);
    }
}
