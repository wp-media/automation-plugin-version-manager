//! Cancelling a JS environment's in-flight calls when it is torn down.
//!
//! A Node.js worker thread runs its own environment (env). When the worker
//! exits, the promises of its pending calls can never settle — yet their
//! futures would keep running on the shared Tokio runtime whenever the addon
//! stays loaded elsewhere (e.g. the main thread): a whole build, with its
//! git and tool processes, for nobody.
//!
//! Each env gets one [`EnvCalls`]. Every call registers with it
//! ([`begin_call`]) and runs under the returned [`CallGuard`], inside the
//! env's [`ProcessScope`], which records every process tree the call starts
//! (git, `gh`, build steps). When the env goes away:
//!
//! - its cleanup hook (worker exit, `worker.terminate()`) kills those trees
//!   at once, then cancels the calls and waits, at most [`TEARDOWN_WAIT`],
//!   until they have stopped (dropping their work and temp files);
//! - its `process` `exit` event kills those trees too (best-effort, and
//!   only on a real exit): the main thread's `process.exit()`, uncaught
//!   exceptions and unhandled rejections run no cleanup hooks and drop no
//!   futures, but do emit `exit`.
//!
//! Only this env's processes are touched: a worker exiting never stops the
//! main thread's builds.
//!
//! An env lives on exactly one JS thread (the main thread or one worker),
//! and calls into the addon for it always arrive on that thread, so the
//! registry is a thread-local — keyed by env, as a thread may host more than
//! one (an embedder's choice).

use std::cell::RefCell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use apvm_core::ProcessScope;
use napi::Env;
use napi::bindgen_prelude::{Function, Unknown};
use tokio::sync::watch;

use crate::error::{clear_pending_exception, exception_pending};

/// Longest an env teardown waits for its cancelled calls to stop. Stopping
/// is quick (kill the child processes, drop the work), but dropping a build
/// may remove a large temp workspace; past this the teardown proceeds.
pub(crate) const TEARDOWN_WAIT: Duration = Duration::from_secs(2);

thread_local! {
    /// The calls registry of each env on this JS thread, by env address:
    /// added on the env's first call, removed by that env's cleanup hook.
    static ENV_CALLS: RefCell<Vec<(usize, Arc<EnvCalls>)>> = const { RefCell::new(Vec::new()) };
}

/// The in-flight calls of one env, and the switch that cancels them.
pub(crate) struct EnvCalls {
    /// Flipped to `true` once, at env teardown.
    cancelled: watch::Sender<bool>,
    /// Number of calls begun and not yet finished.
    active: Mutex<usize>,
    /// Signalled whenever `active` drops to zero.
    idle: Condvar,
    /// Every process tree the env's calls started and that still runs.
    processes: ProcessScope,
    /// Whether the env's `process` `exit` hook is in place.
    exit_hooked: AtomicBool,
}

impl EnvCalls {
    /// An empty registry, not cancelled.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            cancelled: watch::Sender::new(false),
            active: Mutex::new(0),
            idle: Condvar::new(),
            processes: ProcessScope::new(),
            exit_hooked: AtomicBool::new(false),
        })
    }

    /// The env is going away: kill every process tree its calls started,
    /// then cancel the calls and wait (at most `timeout`) until they stopped.
    ///
    /// # Arguments
    ///
    /// * `timeout` - Longest time to wait for the calls
    ///
    /// # Returns
    ///
    /// Whether every call had stopped in time.
    pub(crate) fn tear_down(&self, timeout: Duration) -> bool {
        self.processes.kill_all();
        self.cancel_and_wait(timeout)
    }

    /// Register a call; it counts as active until its guard is dropped.
    ///
    /// # Returns
    ///
    /// The guard to run the call's work under ([`CallGuard::run`]).
    pub(crate) fn begin(self: &Arc<Self>) -> CallGuard {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        CallGuard {
            calls: Arc::clone(self),
            cancelled: self.cancelled.subscribe(),
        }
    }

    /// Cancel every call (including any begun later) and wait until none is
    /// active, or until `timeout` passes.
    ///
    /// # Arguments
    ///
    /// * `timeout` - Longest time to wait
    ///
    /// # Returns
    ///
    /// Whether every call had stopped in time.
    pub(crate) fn cancel_and_wait(&self, timeout: Duration) -> bool {
        self.cancelled.send_replace(true);
        let deadline = Instant::now() + timeout;
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        while *active > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            active = self
                .idle
                .wait_timeout(active, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Mark one call finished, waking a waiting teardown when it was the
    /// last.
    fn end(&self) {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        *active = active.saturating_sub(1);
        if *active == 0 {
            self.idle.notify_all();
        }
    }
}

/// The env that made a call was torn down before the call finished.
///
/// Kept separate from `napi::Error` so the cancellation logic is testable
/// outside Node.js; converted at the napi boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cancelled;

impl From<Cancelled> for napi::Error {
    /// A `Cancelled` napi error. Its promise can no longer settle (the env
    /// is gone), so no JavaScript ever sees it.
    fn from(_: Cancelled) -> Self {
        napi::Error::new(
            napi::Status::Cancelled,
            "the JavaScript environment that made this call was torn down",
        )
    }
}

/// One registered call. Dropping it marks the call finished.
pub(crate) struct CallGuard {
    /// The registry this call counts in.
    calls: Arc<EnvCalls>,
    /// Becomes `true` when the env is torn down.
    cancelled: watch::Receiver<bool>,
}

impl CallGuard {
    /// Run `work` until it finishes or the env is torn down, whichever comes
    /// first. On teardown `work` is dropped — stopping the processes it
    /// started — before this returns.
    ///
    /// # Arguments
    ///
    /// * `work` - The call's future
    ///
    /// # Returns
    ///
    /// `work`'s result.
    ///
    /// # Errors
    ///
    /// `work`'s error, or [`Cancelled`] (converted into `E`) when the env was
    /// torn down.
    pub(crate) async fn run<T, E: From<Cancelled>>(
        mut self,
        work: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let processes = self.calls.processes.clone();
        tokio::select! {
            // Checked first, so a call begun after teardown never starts.
            biased;
            _ = self.cancelled.wait_for(|cancelled| *cancelled) => Err(Cancelled.into()),
            result = processes.run(work) => result,
        }
    }
}

impl Drop for CallGuard {
    /// The call finished, or its future was dropped (even unpolled): it no
    /// longer counts as active.
    fn drop(&mut self) {
        self.calls.end();
    }
}

/// Register a call on the env of the current JS thread: on the env's first
/// call, set up its teardown (cleanup hook); until it is in place, its
/// `process` `exit` hook too.
///
/// # Arguments
///
/// * `env` - The calling env (the current JS thread's)
///
/// # Returns
///
/// The guard to run the call's work under.
///
/// # Errors
///
/// When the env cleanup hook cannot be registered.
pub(crate) fn begin_call(env: &Env) -> napi::Result<CallGuard> {
    let calls = match registered(env.raw().addr()) {
        Some(calls) => calls,
        None => register_env(env)?,
    };
    hook_process_exit(env, &calls);
    Ok(calls.begin())
}

/// Create the registry of the current env, torn down by its cleanup hook.
///
/// # Arguments
///
/// * `env` - The calling env (the current JS thread's)
///
/// # Returns
///
/// The env's new registry.
///
/// # Errors
///
/// When the env cleanup hook cannot be registered.
fn register_env(env: &Env) -> napi::Result<Arc<EnvCalls>> {
    let key = env.raw().addr();
    let calls = EnvCalls::new();
    env.add_env_cleanup_hook((key, Arc::clone(&calls)), |(key, calls)| {
        calls.tear_down(TEARDOWN_WAIT);
        forget(key);
    })?;
    ENV_CALLS.with(|registry| registry.borrow_mut().push((key, Arc::clone(&calls))));
    Ok(calls)
}

/// Make the env kill its calls' processes when its `process` really exits
/// ([`EXIT_HOOK_INSTALLER`]) — best-effort: without it only a main-thread
/// exit (`process.exit()`, a crash) escapes cleanup, the cleanup hook
/// covering every other. A failed attempt is retried on the next call.
///
/// # Arguments
///
/// * `env` - The calling env (the current JS thread's)
/// * `calls` - The env's registry
fn hook_process_exit(env: &Env, calls: &EnvCalls) {
    // Only this JS thread reads or writes the flag.
    if calls.exit_hooked.load(Ordering::Relaxed) {
        return;
    }
    // An exception already pending is not ours to clear: leave the hook for
    // a later call.
    if exception_pending(env) {
        return;
    }
    // Set first, so a call made back into the addon from the JavaScript
    // below does not hook a second time.
    calls.exit_hooked.store(true, Ordering::Relaxed);
    if install_exit_hook(env, calls.processes.clone()).is_err() {
        calls.exit_hooked.store(false, Ordering::Relaxed);
        // Ours (e.g. from a patched `process`): never surface it from a call
        // that itself goes on.
        clear_pending_exception(env);
    }
}

/// JavaScript that hooks `exit` with `kill`, given as its argument.
///
/// - A stray `process.emit('exit')` is ignored: `process._exiting` is
///   `false` then, and `true` on any real exit (Node.js 18 to 26); without
///   the property, every `exit` counts as real.
/// - A host already at its listener limit gets no `MaxListenersExceededWarning`
///   for it: Node.js's documented bump-and-restore of `setMaxListeners`.
/// - It throws only when no listener was added, so failing means retrying.
const EXIT_HOOK_INSTALLER: &str = r#"(function installApvmExitHook(kill) {
  'use strict';
  const listener = function apvmKillChildProcesses() {
    if (process._exiting !== false) kill();
  };
  const max = process.getMaxListeners();
  const full = max > 0 && process.listenerCount('exit') >= max;
  if (full) process.setMaxListeners(max + 1);
  try {
    process.on('exit', listener);
  } finally {
    if (full) {
      try { process.setMaxListeners(max); } catch {}
    }
  }
})"#;

/// Run [`EXIT_HOOK_INSTALLER`] in `env` with a function that kills
/// `processes` — `exit` listeners run synchronously on this thread, the one
/// moment the main thread's `process.exit()` offers: it skips env cleanup
/// hooks and drops no futures.
///
/// # Arguments
///
/// * `env` - The env whose `process` to hook
/// * `processes` - The env's process scope
///
/// # Errors
///
/// When the script cannot run or throws; a JS exception may then be left
/// pending in `env`.
fn install_exit_hook(env: &Env, processes: ProcessScope) -> napi::Result<()> {
    let install: Function<'_, Function<'_, (), ()>, Unknown<'_>> =
        env.run_script(EXIT_HOOK_INSTALLER)?;
    let kill = env.create_function_from_closure("apvmKill", move |_| {
        processes.kill_all();
        Ok(())
    })?;
    install.call(kill)?;
    Ok(())
}

/// The registry of the env at `key` on this thread, if it made a call yet.
///
/// # Arguments
///
/// * `key` - The env's address
///
/// # Returns
///
/// The env's registry, or `None` before its first call.
fn registered(key: usize) -> Option<Arc<EnvCalls>> {
    ENV_CALLS.with(|registry| {
        registry
            .borrow()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, calls)| Arc::clone(calls))
    })
}

/// Drop the registry of the torn-down env at `key` (whose address a later
/// env may reuse). Never panics: it runs in a cleanup hook, possibly late in
/// the thread's exit.
///
/// # Arguments
///
/// * `key` - The env's address
fn forget(key: usize) {
    let _ = ENV_CALLS.try_with(|registry| {
        if let Ok(mut registry) = registry.try_borrow_mut() {
            registry.retain(|(k, _)| *k != key);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// Sets its flag when dropped — proves cancelled work was dropped.
    struct SetOnDrop(Arc<AtomicBool>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn a_call_that_finishes_returns_its_result_and_ends() {
        let calls = EnvCalls::new();
        let result = calls.begin().run(async { Ok::<_, Cancelled>(42) }).await;
        assert_eq!(result, Ok(42));
        assert!(calls.cancel_and_wait(Duration::ZERO), "nothing is active");
    }

    #[tokio::test]
    async fn teardown_cancels_pending_calls_and_drops_their_work() {
        let calls = EnvCalls::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = SetOnDrop(Arc::clone(&dropped));
        let guard = calls.begin();
        let pending = tokio::spawn(guard.run(async move {
            let _flag = flag;
            std::future::pending::<Result<(), Cancelled>>().await
        }));

        // The hook runs on the JS thread while the runtime keeps working.
        let stopped = {
            let calls = Arc::clone(&calls);
            tokio::task::spawn_blocking(move || calls.cancel_and_wait(Duration::from_secs(5)))
                .await
                .unwrap()
        };

        assert!(stopped, "the cancelled call ended in time");
        assert!(dropped.load(Ordering::SeqCst), "its work was dropped");
        assert_eq!(pending.await.unwrap(), Err(Cancelled));
    }

    #[tokio::test]
    async fn a_call_begun_after_teardown_never_runs_its_work() {
        let calls = EnvCalls::new();
        assert!(calls.cancel_and_wait(Duration::ZERO));
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let result = calls
            .begin()
            .run(async move {
                flag.store(true, Ordering::SeqCst);
                Ok::<_, Cancelled>(())
            })
            .await;
        assert_eq!(result, Err(Cancelled));
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[test]
    fn teardown_gives_up_after_the_timeout() {
        // A call that never ends (its guard is never dropped) must not block
        // the env teardown forever.
        let calls = EnvCalls::new();
        let _stuck = calls.begin();
        let started = Instant::now();
        assert!(!calls.cancel_and_wait(Duration::from_millis(100)));
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn tear_down_closes_the_process_scope_before_waiting() {
        // Killing comes first: a call that does not stop in time must not
        // delay the kill of its processes by the whole wait.
        let calls = EnvCalls::new();
        let stuck = calls.begin();
        let tearing = {
            let calls = Arc::clone(&calls);
            std::thread::spawn(move || calls.tear_down(Duration::from_secs(10)))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !calls.processes.is_closed() {
            assert!(
                Instant::now() < deadline,
                "the scope was not closed in time"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let still_waiting = !tearing.is_finished();
        drop(stuck);
        assert!(still_waiting, "the scope closed only after the wait");
        assert!(tearing.join().unwrap(), "the teardown ended with the call");
        assert!(*calls.cancelled.borrow(), "later calls are refused");
    }

    /// A call's own error, or its cancellation.
    #[derive(Debug, PartialEq, Eq)]
    enum Outcome {
        Failed,
        Cancelled,
    }

    impl From<Cancelled> for Outcome {
        fn from(_: Cancelled) -> Self {
            Outcome::Cancelled
        }
    }

    #[tokio::test]
    async fn a_failing_call_returns_its_own_error_and_ends() {
        let calls = EnvCalls::new();
        let result = calls
            .begin()
            .run(async { Err::<(), _>(Outcome::Failed) })
            .await;
        assert_eq!(result, Err(Outcome::Failed));
        assert!(calls.cancel_and_wait(Duration::ZERO), "nothing is active");
    }

    #[tokio::test]
    async fn teardown_waits_for_every_concurrent_call() {
        let calls = EnvCalls::new();
        let pending: Vec<_> = (0..3)
            .map(|_| {
                let guard = calls.begin();
                tokio::spawn(guard.run(std::future::pending::<Result<(), Outcome>>()))
            })
            .collect();
        let stopped = {
            let calls = Arc::clone(&calls);
            tokio::task::spawn_blocking(move || calls.tear_down(Duration::from_secs(5)))
                .await
                .unwrap()
        };
        assert!(stopped, "every call ended in time");
        for call in pending {
            assert_eq!(call.await.unwrap(), Err(Outcome::Cancelled));
        }
    }

    #[test]
    fn a_call_dropped_before_it_ever_ran_still_ends() {
        // E.g. `spawn_future` failing: the future is dropped unpolled.
        let calls = EnvCalls::new();
        let call = calls.begin().run(async { Ok::<_, Cancelled>(()) });
        drop(call);
        assert!(calls.cancel_and_wait(Duration::ZERO));
    }

    #[test]
    fn each_env_on_a_thread_has_its_own_registry() {
        let (first, second) = (EnvCalls::new(), EnvCalls::new());
        ENV_CALLS.with(|registry| {
            registry
                .borrow_mut()
                .extend([(1, Arc::clone(&first)), (2, Arc::clone(&second))]);
        });
        assert!(registered(1).is_some_and(|calls| Arc::ptr_eq(&calls, &first)));
        assert!(registered(2).is_some_and(|calls| Arc::ptr_eq(&calls, &second)));
        assert!(registered(3).is_none());

        forget(1);
        assert!(registered(1).is_none(), "the torn-down env is forgotten");
        assert!(registered(2).is_some(), "the other env is kept");
        forget(2);
        forget(2); // idempotent
        assert!(registered(2).is_none());
    }

    #[test]
    fn calls_are_counted_until_their_guards_drop() {
        let calls = EnvCalls::new();
        let first = calls.begin();
        let second = calls.begin();
        drop(first);
        assert!(!calls.cancel_and_wait(Duration::from_millis(10)));
        drop(second);
        assert!(calls.cancel_and_wait(Duration::ZERO));
    }
}
