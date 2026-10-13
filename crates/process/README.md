# apvm-process

Child processes that never outlive the work that started them. Every
program apvm runs goes through this crate: apvm-core's git, `gh` and build
steps, and apvm-sites' `ssh` (and the local shell its tests use). It has no
`apvm-*` dependencies (a unit test checks `Cargo.toml`), so both crates can
share it.

## API

| Item | What it does |
|---|---|
| `command(program)` | The only `tokio::process::Command` constructor (the workspace `clippy.toml` bans tokio's own) |
| `spawn(&mut command)` | Starts it (stdio as the caller set it): returns its `Pipes` and a `Running` guard, and records the tree in the current `ProcessScope` |
| `Running::wait()` | Waits for and reaps the child (cancel-safe); the tree leaves its scope |
| `Running::kill()` | Kills the child and everything it started now (a timeout) |
| `output(&mut command)` | Runs to completion with stdin closed and stdout/stderr captured |
| `ProcessScope` | Records every tree started by work run in it (`scope.run(work)`); `kill_all()` kills them synchronously and refuses new ones; `ProcessScope::current()` lets work carry the scope into a spawned task |

## Guarantees

If the future awaiting a child is dropped (a cancelled build or `ssh` call,
a Node.js worker exiting), or `Running::kill` is called, the child **and
every process it started** are killed, best-effort:

- **Unix:** the tree is found by process ancestry (`/proc` on Linux,
  `/bin/ps` elsewhere; no `kill` or `ps` program needed on Linux), stopped
  (`SIGSTOP`) so it cannot grow, then killed (`SIGKILL`) children first, so
  no member's PID is freed and reused before its kill. Not by process group:
  a terminal's Ctrl+C must still reach the children.
- **Windows:** the child only (`taskkill /F`); `taskkill /T` matches children
  by a parent PID Windows never clears.
- **Scopes:** Node.js `process.exit()` drops no futures, so the Node
  bindings run every call in a `ProcessScope` and call `kill_all()` at exit.
  The scope is task-local: a `tokio::spawn`ed task is outside it unless it
  runs in `ProcessScope::current()` too.

Limits:

- A process that left the tree (a daemon, an ssh `ControlPersist` master),
  descendants when the process table cannot be read (a sandbox that forbids
  `/bin/ps`), and processes apvm may not signal are not killed.
- If the killing process itself ends between the stop and the kill (killed,
  or a `process.exit()` while another thread is mid-kill), what was stopped
  stays stopped. The window is the search for the tree (a few process-table
  snapshots); within a scope, `kill_all` at exit covers it.
- A failed `wait` (something else reaped the child) lets the child go: its
  PID may already be another process's, so it is never signalled again, and
  the tokio handle is forgotten so its `kill_on_drop` cannot fire.
- A child that something else reaps while it is still awaited (another
  part of the host calling `waitpid(-1)`, or `SIGCHLD` ignored) is signalled
  through its pidfd on Linux (5.3+), so a reused PID is never hit. Elsewhere
  it is signalled by PID: that can only reach another process if the PID is
  reused in between.
- On Windows a killed process exits with status 1 (`TerminateProcess`): a
  caller sees an exit status, not a signal.

## Tests

```sh
cargo test -p apvm-process
```

The kill tests run real shells whose scripts record their PIDs; each PID
must disappear (`kill(pid, 0)`), so a tree that was only stopped fails.
Other crates use the same helpers through the `test-support` feature, as a
dev-dependency only (`apvm_process::test_support`: `heartbeat`,
`assert_all_gone`, `tree_visible`, …). Descendant checks are skipped where
the process table cannot be read.
