//! Progress reporting bridge between Rust and Node.js.
//!
//! Implements the [`ProgressReporter`] trait using a N-API [`ThreadsafeFunction`],
//! allowing build events to be delivered to a JavaScript callback function
//! from any Rust thread without blocking or crashing.
//!
//! The callback is invoked in non-blocking mode, meaning events are queued
//! on the Node.js event loop and delivered asynchronously. This is safe
//! to call from the tokio runtime threads that execute build operations.

use apvm_core::build::progress::{BuildEvent, ProgressReporter};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

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
    /// - If the JS callback throws, the error is swallowed (not propagated)
    ///
    /// This is intentional — build progress is informational, and a
    /// failing callback should never abort the build itself.
    fn report(&self, event: &BuildEvent) {
        let js_event = JsBuildEvent::from(event);
        // NonBlocking: queue on the event loop, don't wait for JS to process.
        // Ignoring the result is intentional — progress callbacks are
        // best-effort and must not abort the build on JS-side errors.
        let _ = self
            .callback
            .call(Ok(js_event), ThreadsafeFunctionCallMode::NonBlocking);
    }
}
