# NAPI cache maintenance — parity with `apvm cache`

Status: **Phases 1–2 implemented** (Phase 1 audited; Phases 3–4 planned) · Target version: **3.3.0** (from 3.2.0 — also claimed by the site-deployment plan; whichever lands second takes the next minor) · Author: Sandy Figueroa

---

## TL;DR

| | |
|---|---|
| **Problem** | Node consumers can build, bypass (`noCache`) and warm (`warmCache()`) the cache, but cannot inspect, clean, gc, verify, repair or clear it — they need the CLI pointed at the same directory. Verifying the CLI also surfaced three defects that hit both front ends (F4, F11, F12). |
| **Goal** | Every `apvm cache` action callable from Node with the same semantics, on the same directory, regardless of `cacheEnabled` — and the defects fixed once, at the root, for both front ends. |
| **Approach** | A core facade, `apvm_core::maintenance::CacheMaintenance`, owns every maintenance decision. The CLI and NAPI become thin adapters over it, so parity comes from shared code, not discipline. |
| **JS API** | `ApvmCache` with `info()`, `clean()`, `gc()`, `verify()`, `repair()`, `clear()` — 1:1 with the subcommands — from `apvm.cache()` (the dir that instance builds into) or `ApvmCache.open(config?)` (standalone, no GitHub client). Plus `apvm.cacheStatus()`. |
| **Errors** | Every `ApvmCache` rejection carries a stable `err.code`: `CacheCorrupted` (→ `repair()`), `InvalidArg`, `GenericFailure`. Delivered through a JS-thread callback, spike-verified on napi 3.13. |
| **Fixes** | **F4** input is validated before the missing-dir guard, and an uninspectable path is an error, not "empty". **F11** an unusable cache is reported (status + build warning) and re-attached automatically once repaired. **F12** `gc` removes damaged entries (`--checksum` catches same-size corruption), which makes verify's hint true. |
| **Invariants** | Never creates a missing cache dir. Validates before touching disk. All I/O runs off the event loop. |
| **Deliberate differences** | `clear()` has no prompt. `verify()` resolves with the issue list instead of rejecting. |
| **CLI impact** | Output unchanged except the fixes: the F4 error, the F11 warning, `gc --checksum` + one gc line + a corrected hint (F12). SKILL.md is updated for exactly those. |
| **Deps** | `chrono` added to `apvm-core` (already a workspace dependency). Nothing new in the root `Cargo.toml`. |
| **Audit (Phase 1)** | Adversarial audits, a mutation campaign and load stress found seven storage hazards, all fixed in Phase 1 (F13–F19): gc deleting foreign or symlinked data, races on concurrent opens and repair (including an in-process SIGBUS), unrepairable rows, open handles stranded on a quarantined copy, and a busy timeout that silently became 0. The first F15 fix was incomplete; load stress exposed the remaining WAL-switch race. |
| **Phases** | 1 facade + F4 + audit fixes (F13–F19) → 2 F11 + F12 → 3 NAPI surface → 4 release 3.3.0. |

---

## 1. The gap

| CLI | NAPI today | NAPI after |
|---|---|---|
| `build --no-cache` / `--warm-cache` | `noCache` / `warmCache()` ✅ | — |
| `config set cache false` · `cache-dir` · `APVM_CACHE_DIR` | `cacheEnabled` · `cacheDir` · `APVM_CACHE_DIR` ✅ | — |
| `cache info` | ❌ | `info()` |
| `cache clean [--older-than D] [--project P] [--dry-run] [--builds\|--releases]` | ❌ | `clean({ olderThan, project, dryRun, target })` |
| `cache gc [--checksum]` (flag new, F12) | ❌ | `gc({ checksum })` |
| `cache verify [--checksum]` | ❌ | `verify({ checksum })` |
| `cache repair` | ❌ | `repair()` |
| `cache clear [-y]` | ❌ | `clear()` |
| — | ❌ | `apvm.cacheStatus()` |

Confirmed at runtime on the shipped binary: `Apvm.prototype` has only `build*`, `download*`, `hasToken`, `listProjects`, `tokenSource` and `warmCache`.

---

## 2. Verified facts

| # | Fact | How verified |
|---|---|---|
| F1 | `apvm cache` calls `apvm_storage::ArtifactStore` directly; core has no maintenance API. NAPI would have to copy the CLI glue: missing-dir guard, `--older-than` grammar (`parse_duration`), target mapping, corruption hint. | read |
| F2 | On a missing dir, all six actions print "Cache is empty", exit 0 and never create the dir. `ArtifactStore::open`/`repair` both `create_dir_all`, so the guard must precede any open. | ran all six |
| F3 | `Apvm.create()` with caching on creates `apvm.db` (+WAL); with `cacheEnabled: false` it creates nothing; on a corrupt DB it **resolves** with caching off, and the store is never reopened for that instance. | ran from Node + read |
| F4 | **Bug:** the guard runs before validation — `clean --older-than 30y --project Bad/Name` exits 0 on a missing dir, 1 once the dir exists. For a library that is a CI-only failure (fresh runners have no cache). Same class: the guard's `Path::exists` hides errors, so a path that cannot be inspected (below a regular file, permission denied) is reported as "Cache is empty", exit 0. | ran |
| F5 | NAPI resolves `cacheDir` → `~/.apvm/cache`, then `APVM_CACHE_DIR` wins (set from JS, it beat an explicit `cacheDir`). NAPI never reads `~/.apvm/config.json`. | ran from Node + read |
| F6 | Existing rejections carry `err.code` = the napi `Status` (`"InvalidArg"` for an unknown project). | ran from Node |
| F7 | Spike on the locked versions (napi 3.13.0, derive 3.6.9, cli 3.10.5): a method can return another class ✅; a sync factory taking an optional object config ✅; `spawn_blocking` keeps the event loop live ✅; a bad string-enum value → `InvalidArg` ✅. A custom code through an `async fn` return type (`napi::Result<T, CustomStatus>`) does **not** compile — but `Env::spawn_future_with_callback` + `create_error` + a `code` property **does**: the promise rejects with `code: "CacheCorrupted"`, still `instanceof Error` with message and stack, 8 concurrent calls behave, and `ts_return_type` yields `Promise<T>`. | spike |
| F8 | Mutations hold `StoreLock` (per-descriptor `File::lock`, so two stores in one process exclude each other) plus WAL with a 5 s busy timeout. A second store beside a live `Apvm` behaves like `apvm cache` run from another process: builds degrade to a miss, never fail. | read + `lock_excludes_second_acquisition_until_dropped` |
| F9 | `repair()` on a healthy DB is a no-op. A garbage `apvm.db` next to `<project>/commits/<ver>/<7-hex>/<file>.zip` makes `repair()` re-index those builds → **offline test seeding through the public API**. | ran (2 builds adopted) |
| F10 | Unfiltered `clean` deletes everything without a prompt, so `clear`'s prompt is UI, not a safety boundary. | ran |
| F11 | **Bug (both front ends):** with a corrupt or unopenable cache every build is silently uncached. Only `warmCache` warns, and only "disabled or unavailable"; the one signal carrying the reason is a `tracing` log, hidden unless `RUST_LOG` is set. | ran CLI + read |
| F12 | **Bug:** damaged artifacts, via the storage API. *Missing file:* lookup misses, re-store heals, `gc` drops 0. *Size mismatch:* lookup misses, re-store heals, `gc` ignores it. *Same-size corruption:* **served as a cache hit, re-store reuses it, `gc` ignores it** — only `verify --checksum` sees it, and nothing short of `clean`/`clear` removes it. So verify's hint ("run `apvm cache gc` … or rebuild to heal them") has `gc` advice that fixes none of the three, and "rebuild" fails for the third. | probe crate |

| F13 | **Bug (audit):** `gc` on a directory that is not a cache deleted user data at store-shaped paths (`my-plugin/releases/1.0/…`, `tools/commits/2024/q1/…`, empty `notes/`) and created `apvm.db` there. On a real cache whose database was deleted, `gc` removed every build. Read-only actions also created `apvm.db` in any existing directory. | ran |
| F14 | **Bug (audit):** `gc` followed a symlinked `commits`/`releases` directory and deleted outside the cache. | ran |
| F15 | **Bug (audit):** concurrent first opens of a new database failed (`table builds already exists` / `database is locked`; 43–84 of 320 in-process opens). The WAL switch alone still failed 16 of 120,000 racing opens: SQLite fails that lock upgrade at once, without the busy handler. `repair` racing concurrent opens failed after quarantining, leaving builds unindexed (~0.5% of rounds). A load stress run exposed the WAL case after the first fix. `repair` racing concurrent opens failed after quarantining, leaving builds unindexed (~0.5% of rounds). | probe |
| F16 | **Bug (audit):** a database with unreadable rows (e.g. an out-of-range timestamp) failed every read forever: `repair` reported "healthy", although the docs said it rebuilt the index. An empty cache path meant "missing" to maintenance but the current directory to the store. | ran |
| F17 | **Bug (audit):** a connection that opened `apvm.db` just before repair renamed it, and first read it just after, paired the quarantined file with the new database's WAL. Distinct layouts gave `database disk image is malformed`; lookalike layouts (the F16 case) let its writes reach the new database. SQLite checks for a moved file only on rollback-journal writes and on close. **In the same process**, POSIX locks are shared, so the ghost took the new `-shm` for its own and truncated it under live connections: SIGBUS in `walIndexReadHdr` in 3 of 4 race-probe runs. Windows is immune: an open database cannot be renamed there (no `FILE_SHARE_DELETE`). | probe + crash report + source |
| F18 | **Bug (audit, pre-existing):** `StoreOptions::busy_timeout` above `i32::MAX` ms saturated to `u64::MAX`, which SQLite silently reads as 0, so the store never waited. | ran |
| F19 | **Bug (audit):** F16's repair renamed a database with unreadable rows. Such a database opens fine, so a long-lived handle (`Apvm` keeps its store for its lifetime; a running build) stayed on the renamed copy: its reads stayed broken, its writes were lost, and `gc` later collected its builds as orphans. | ran |

Baseline at `1f22e28`: fmt, clippy and doc are clean; 631 tests pass.

---

## 3. Design

### 3.1 Shape

```
before   CLI   commands/cache.rs ───────────────────────────────────────► apvm_storage::ArtifactStore
         NAPI  (nothing)

after    CLI   commands/cache.rs   (print, prompt, exit code) ─┐
                                                               ├─► apvm_core::maintenance::CacheMaintenance ─► ArtifactStore
         NAPI  ApvmCache           (JS types, error codes)   ─┘
```

The facade owns every *decision* (guard, validation, grammar, mapping); the adapters own only *presentation*. Maintenance never reuses `Apvm`'s store — that one is unusable in exactly the cases where maintenance matters (F3).

### 3.2 Core facade — `crates/core/src/maintenance.rs`

```rust
pub struct CacheMaintenance { dir: PathBuf }             // Send + Sync; never holds an open store

impl CacheMaintenance {                                  // Ok(None) = no cache here (missing or empty dir)
    pub fn new(dir: impl Into<PathBuf>) -> Self;
    pub fn dir(&self) -> &Path;
    pub fn exists(&self) -> Result<bool>;                // true only if the cache DB is present; never creates
    pub fn usage(&self) -> Result<Option<UsageReport>>;
    pub fn clean(&self, request: &CleanRequest) -> Result<Option<CleanReport>>;
    pub fn clear(&self) -> Result<Option<CleanReport>>;
    pub fn gc(&self) -> Result<Option<GcReport>>;        // gains a VerifyMode in Phase 2 (F12)
    pub fn verify(&self, mode: VerifyMode) -> Result<Option<Vec<VerifyIssue>>>;
    pub fn repair(&self) -> Result<Option<RepairReport>>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanRequest {                                // CLI flags ≡ JS options, 1:1; chainable setters
    pub older_than: Option<String>,                      // "30d" — parse_duration grammar
    pub project: Option<String>,
    pub target: CleanTarget,                             // apvm_storage::CleanTarget (default All)
    pub dry_run: bool,
}
impl CleanRequest {
    pub fn to_options(&self, now: DateTime<Utc>) -> Result<CleanOptions>; // pure: resolves + validates
}

pub fn parse_duration(spec: &str) -> std::result::Result<chrono::Duration, String>; // moved verbatim from the CLI

pub use apvm_storage::{CleanReport, CleanTarget, GcReport, RepairReport, StoreState, UsageReport, VerifyMode, …};
```

- **Order:** validate → guard → open → operate. Validation checks the duration first, then the project (today's order); the project rule is a new public `apvm_storage::CleanOptions::validate()`, which `clean()` itself also calls — one rule, one place.
- **No cache:** a missing or empty directory is `Ok(None)` rather than a zeroed report, so "nothing cached" stays distinguishable from "empty": the CLI prints its "Cache is empty" note, NAPI renders an empty report. Classification comes from storage `ArtifactStore::inspect` (`StoreState`); only a present database is opened, via the non-creating `open_existing`, so no action but `repair` ever creates a file (F13).
- **Errors, never "empty":** someone else's data (`ForeignDirectory`), a lost database (`MissingDatabase` — `repair` rebuilds it), a regular file, an empty path (`Error::Config`, F16), or a path `try_exists` cannot inspect (`Error::Io` with its kind, F4). **Corrupt DB** (`DatabaseCorrupted`, or unreadable rows: `Data`): returned untouched inside `Error::Storage`; each adapter adds its own hint or code.
- **Blocking by design,** like storage: the CLI calls it directly, NAPI wraps it. The age cutoff (`now − duration`) is a private pure fn taking `now`, so age tests are deterministic.

### 3.3 F11 — an unusable cache is visible and heals itself

- **A slot, not a fixed `Option`:** `Apvm` keeps its store in a private `Mutex` slot (poison-tolerant, like `ArtifactStore::conn`). While caching is enabled but the store is not open, each `build`, `warm_cache` and `cache_status` call retries the open once — one SQLite open, the cost the constructors already pay. A `repair()` from anywhere (NAPI, the CLI, another process) therefore re-enables caching on live instances; nothing has to be re-created. An **open** store survives repair of unreadable rows (cleared in place, F19). A file-level corrupt database is renamed aside, though, and a store still open on it stays on the old file, so the slot must also drop its store after a corruption error.
- **Status:** `pub fn cache_status(&self) -> CacheStatus` = `Active | Disabled | Corrupted { details } | Unavailable { details }`, classified from the open error. `cache_active()` (used only by tests today) is derived from it.
- **Warning:** when caching is enabled but unavailable, `build` and `warm_cache` emit exactly one `BuildEvent::Warning` with the reason and the remedy (for `Corrupted`, `apvm cache repair`; Phase 3 adds `ApvmCache.repair()` once it exists). `Apvm` hands the status to `BuildCommand` through a new non-breaking setter, so the 13 existing `BuildCommand::new` call sites are untouched and the generic warm warning fires only for the disabled case, with its text unchanged (pinned by `cache_e2e.rs:584`). The CLI already prints warnings as `⚠ …`; NAPI delivers them to `onProgress`.

### 3.4 F12 — `gc` removes what `verify` reports

- **Storage:** `gc_with(mode: VerifyMode)`, with `gc()` ≡ `gc_with(Size)` (non-breaking). Beyond today's work it drops artifact/asset records whose file is missing, has the wrong size, or — in `Checksum` mode — the wrong content, deleting the bad file. A build or release left without artifacts loses its row, and its dir falls to the existing orphan sweep. Records with an invalid recorded size are dropped too; files it cannot read stay in place and are reported. `Checksum` mode hashes without the lock (like `verify`), then re-checks only the candidates under the lock before deleting.
- `GcReport` gains `damaged_artifacts`, `damaged_bytes_removed` and `failures`; two internal queries add per-file deletes for artifacts and assets.
- **CLI:** `apvm cache gc [--checksum]`, new output lines (damaged entries; failures, if any), and a verify hint derived from the problems found — `gc`, or `gc --checksum` when checksums mismatch — stating that the next build re-caches what was removed.

### 3.5 NAPI — what JS sees

```ts
class ApvmCache {
  static open(config?: ApvmConfig): ApvmCache              // same dir resolution as Apvm.create(config);
                                                           // reads only cacheDir; touches nothing on disk
  dir(): string
  info(): Promise<JsCacheUsage>
  clean(options?: CleanOptions): Promise<JsCleanReport>    // no options = remove everything, as in the CLI
  gc(options?: GcOptions): Promise<JsGcReport>
  verify(options?: VerifyOptions): Promise<JsVerifyIssue[]>  // [] = healthy
  repair(): Promise<JsRepairReport>
  clear(): Promise<JsCleanReport>
}
class Apvm {
  cache(): ApvmCache
  cacheStatus(): Promise<JsCacheStatus>                    // re-checks; re-attaches a repaired cache (§3.3)
}

interface CleanOptions  { olderThan?: string; project?: string; dryRun?: boolean; target?: JsCleanTarget }
interface GcOptions     { checksum?: boolean }
interface VerifyOptions { checksum?: boolean }
const enum JsCleanTarget { All = 'All', Builds = 'Builds', Releases = 'Releases' }
```

| Output | Fields |
|---|---|
| `JsCacheUsage` | `cacheDir`, `exists`, `totalBytes`, `buildsBytes`, `releasesBytes`, `buildCount`, `releaseCount`, `fileCount`, `databaseBytes`, `oldestBuild?`, `newestBuild?`, `projects[]` (`project`, `buildCount`, `releaseCount`, `buildsBytes`, `releasesBytes`) |
| `JsCleanReport` | `buildsDeleted`, `releasesDeleted`, `bytesFreed`, `dryRun`, `failures: string[]` |
| `JsGcReport` | `staleBuildRows`, `staleReleaseRows`, `orphanDirsRemoved`, `orphanBytesRemoved`, `staleTempFilesRemoved`, `damagedArtifacts`, `damagedBytesRemoved`, `failures: string[]` |
| `JsVerifyIssue` | `project`, `kind` (`build`\|`release`), `version?`, `commit?`, `tag?`, `filename`, `path`, `problem` (`missing`\|`size_mismatch`\|`checksum_mismatch`\|`unreadable`), `expectedSize?`, `actualSize?`, `expectedSha256?`, `actualSha256?`, `details?` — a flattened union, like `JsBuildEvent` |
| `JsRepairReport` | `quarantinedDatabase?` (absent = the DB was healthy), `buildsAdopted`, `artifactsAdopted`, `entriesSkipped`, `orphanReleaseDirs` |
| `JsCacheStatus` | `state` (`active`\|`disabled`\|`corrupted`\|`unavailable`), `reason?` |

Numbers are `f64` (the `JsProducedArtifact.size` precedent: exact to 2⁵³, no truncating casts); timestamps are ISO-8601 strings (no `chrono_date` napi feature needed).

| `err.code` | Raised when | Remedy |
|---|---|---|
| `CacheCorrupted` | the DB is corrupt (every method except `repair()`) | `await cache.repair()`, then retry |
| `InvalidArg` | bad `olderThan`, `project` or `target` | fix the input |
| `GenericFailure` | anything else — I/O, a newer schema, a failed task | report it |

**Mechanism (F7):** one private helper runs `spawn_future_with_callback(spawn_blocking(facade call))`; on the JS thread, `Ok` becomes the value and `Err` becomes `create_error` + `code`. The core-error → code mapping is a pure, unit-tested function; each method declares `ts_return_type = "Promise<T>"`.

```ts
const apvm = await Apvm.create({ cacheDir });
await apvm.cache().clean({ olderThan: '30d', project: 'backwpup', target: JsCleanTarget.Builds });
const issues = await apvm.cache().verify({ checksum: true });
const usage  = await ApvmCache.open({ cacheDir }).info();   // maintenance-only script: no GitHub client
```

### 3.6 Behavior contract

1. **Independent of `cacheEnabled`**, as in the CLI.
2. **Same directory:** `apvm.cache()` uses the dir that instance builds into (captured at `create()`, after `APVM_CACHE_DIR`); `ApvmCache.open(cfg)` resolves the same way at call time. One `resolve_config()` replaces the conversion + env override that both factories duplicate today.
3. **Missing dir:** every method resolves with an empty report and creates nothing; `info().exists` separates "never created" (often a wrong dir) from "empty".
4. **Validation first (F4):** bad input → `InvalidArg`, whatever the disk state.
5. **`verify()` resolves** with the issues — a rejection is not the JS analogue of an exit code. The CLI keeps exiting 1.
6. **`clear()` has no prompt:** the call is the consent (F10), matching the site-deployment rule that no confirmation callback crosses FFI.
7. **Corruption:** `catch (e) { if (e.code === 'CacheCorrupted') { await cache.repair(); /* retry */ } }`. `repair()` is a no-op on a healthy DB (F9), and live `Apvm` instances resume caching on their next build (§3.3).
8. **Threading:** each call opens its own store inside `spawn_blocking` and drops it before resolving, so no handle outlives the call; a `JoinError` → `GenericFailure`, never a panic. Safe beside builds on the same instance (F8).

### 3.7 Rejected alternatives

| Alternative | Why not |
|---|---|
| Only six `*Cache()` methods on `Apvm` | Needs an `Apvm` (GitHub client; `create()` creates the cache as a side effect, F3) just to inspect a cache. `apvm.cache()` keeps the same-dir guarantee without that coupling. |
| Reuse `Apvm`'s store | Unusable when disabled or broken — exactly when maintenance matters. |
| NAPI calls `apvm_storage` directly | Copies the F1 glue: parity by discipline, not construction. |
| `async fn` returning `napi::Result<T, CustomStatus>` | Does not compile (F7). |
| `olderThan` as ms or a `Date` | One grammar, one parser, identical errors across CLI and JS. |
| Heal same-size corruption in `store()` | Would hash every reused file on every partial build, and lookups would still serve the file until then. `gc --checksum` removes it where `verify --checksum` finds it. |
| Auto-repair inside `Apvm.create()` | A constructor silently quarantining a DB is a destructive side effect. Status + warning + one explicit `repair()` keeps the caller in control. |

---

## 4. Phases

Every phase ends green on the full CLAUDE.md validation suite, run with `APVM_CACHE_DIR="$(mktemp -d)"`. From Phase 3 on, also run `npm run build:debug && npm run typecheck && npm test`. The locally generated `index.js`, `index.d.ts` and `*.node` are never committed; CI owns them.

### Phase 1 — Core facade; CLI on top (+ F4 and the audit fixes) — **implemented**

- **Core:** `crates/core/src/maintenance.rs` (§3.2; `gc` takes no mode yet), `pub mod maintenance` + root re-exports; `chrono = { workspace = true }`.
- **Storage (audit fixes):**
  - **F13:** new `layout.rs` — `inspect()` → `StoreState` (Missing / Empty / Present / Orphaned / Foreign), read-only and symlink-safe. `open` creates a store only in a missing or empty directory; it refuses foreign data (`ForeignDirectory`) and a lost database (`MissingDatabase`). New non-creating `open_existing()`.
  - **F13/F14 — gc:** removes only store-produced names (hex build leaves, sanitized-tag release leaves), never traverses symlinks, and prunes only store-shaped project dirs.
  - **F15:** migrations re-check `user_version` inside `BEGIN IMMEDIATE`; the WAL switch retries on `SQLITE_BUSY` until the busy timeout elapses; a database is only ever created under the store lock, re-checked there.
  - **F16/F19:** `repair` rebuilds a lost database from disk and refuses foreign directories. For unreadable rows (`Error::Data`) it copies the index aside (`VACUUM INTO apvm.db.corrupt-<ms>`), then empties and re-fills it in place, so open handles keep working. Only a non-UTF-8 base path, which SQL cannot name, falls back to renaming. `CleanOptions::validate()` is public.
  - **F17:** `db::open` records the file's identity (device, inode) before opening and again after the first read. If it changed, it reopens, at most 3 times. Within the process, a lock keeps the quarantine rename and a connection's start apart.
  - **F18:** the busy timeout saturates at `i32::MAX` ms.
- **CLI:** `crates/cli/src/commands/cache.rs` is presentation only. All I/O goes through an injectable `Console` (stdout, stderr, confirmation), and every report is a pure renderer. Hints are per error (corrupt/unreadable → repair; missing → repair; foreign → where the cache location comes from). `clear` checks the cache before prompting. `parse_duration` and its 2 tests moved to core verbatim.
- **Tests (all offline):** 695 pass (631 before): storage +25, core +24 (2 moved from the CLI), CLI +14 net, 1 doctest.
  - core: 24 facade tests — every action in every directory state, exact messages, Unix-gated error kinds;
  - CLI: 23 cache tests — exact renderer output, streams, scripted prompts, flag mapping, hints;
  - storage: every `StoreState`, refusals leaving directories byte-identical, gc name and symlink rules, lost-DB rebuild, unreadable-row repair, concurrent first opens and repair-vs-open races, a WAL switch waiting for (and timing out on) a rival writer, a file replaced / moved / kept replaced mid-open, the in-process swap lock (both sides), in-place repair keeping an open handle working, unreadable release rows, busy-timeout saturation, creation waiting for the lock.
- **Verified:**
  - **Clap definitions and the 7 original CLI tests:** byte-identical to `HEAD`.
  - **Mutation testing:** 50 of 51 mutants caught under full-suite load, each by the test written for it. Runs use `--no-fail-fast`, since one "catch" turned out to be the F15 flake; a timing-based guard test that let a mutant through under load was made deterministic. The 51st ("repair offers itself as remedy") is equivalent in practice: repair can only surface a corruption error if its own fresh database fails the integrity check.
  - **Race probes:** 0 failures in 1,600 first opens, 1,000 repair rounds and 120,000 racing WAL switches; before the fixes, 43–84 of 320, ~0.5% and 16 of 120,000 failed. 0 of 360 storage-suite runs under 6× parallel load failed; 1 of 240 failed before the WAL fix. 10 of 10 race-probe processes exit cleanly (16,000 first opens, 10,000 repair rounds); without the in-process lock, 4 of 4 die of SIGBUS.
  - **Old vs new CLI:** 57 scenarios, with a clean control run. 38 of the original 42 are byte-identical. The 4 intended deltas are the three F4 cases and the neutral out-of-range message (`'100000000w' is out of range`, no CLI flag name in core). All 15 new foreign / empty / deleted-DB / symlink scenarios changed as intended: nothing written or deleted, errors with next-step hints, `repair` re-indexing.
- **Other intended deltas:**
  - Invalid `clean` input now beats a corrupt database (validation first).
  - An existing empty directory is reported as empty, with no `apvm.db` created.
  - `clear` on a broken cache errors before prompting.
- **Docs:** SKILL.md (cache section and error rows), CLI/core/storage/root READMEs.

### Phase 2 — F11 and F12 (core, storage, CLI; no JS change) — **implemented**

As built (the plan below was followed except where noted):

- **Storage (F12):** `gc_with(VerifyMode)`, with `gc()` ≡ `gc_with(Size)`. It judges each file the way `verify` does (shared `check_file`), deletes damaged files and then drops their records. A record goes only once its file is gone, so a failed delete stays visible. Builds or releases left without files lose their rows (counted in `stale_*_rows`), and their dirs go to the orphan sweep. Files are deleted only inside store-shaped dirs reached without symlinks (`layout::owned_dir`, valid filename). A file another kept record names up to case is never deleted. Otherwise the record is dropped, the file is untouched, and the case is listed. `Checksum` mode hashes without the lock and re-hashes only the suspects under it. `GcReport` gains `damaged_artifacts`, `damaged_bytes_removed` and `failures`.
- **Storage, also changed:**
  - A file that cannot be *inspected* (e.g. a non-searchable dir) is now `Unreadable` in `verify` and kept by `gc`, instead of `Missing` (which would have made gc drop a record whose file exists).
  - Orphan dirs that gc fails to remove are listed in `failures` (before, they were only logged).
  - `Unreadable` details keep the I/O cause ("…: Permission denied").
- **F11, deviation — `is_current()` instead of error-based detection:** an open handle does not notice a database file damaged in place. SQLite serves the cached pages, so its `quick_check` and reads still pass; only a fresh open reports it (verified with a probe). So the slot does not run a per-build integrity check, which would cost time and catch nothing. It drops its store when `ArtifactStore::is_current()` (a stat against the file identity recorded at open) says the file was renamed aside by a repair, deleted or replaced, then reopens. A store is held only after a successful open, and every refresh reopens when nothing is held, so a corrupt cache is always reported (status + warning) and re-attached once repaired.
- **Core:** `CacheStatus` (`#[non_exhaustive]`): `MissingDatabase` maps to `Corrupted` (its remedy is `repair`), and an I/O error's details include the cause. `BuildCommand::with_cache_status` exists; `Apvm::build` / `warm_cache` re-check the slot on `spawn_blocking`. The facade's `gc` takes a `VerifyMode` (`gc(mode)`, mirroring `verify(mode)`); Phase 1's `gc()` was never released.
- **CLI:** `apvm cache gc [--checksum]` gets the new line `Dropped N damaged file record(s) (X deleted)`. What gc left in place goes to stderr (`Left N item(s) in place:`), exit 0, like `clean`. The `verify` hint names `gc` or `gc --checksum`, plus a permissions line when some files are unreadable.
- **Audit (adversarial).** Round 1 had 4 independent auditors; two ran out of budget. Round 2 re-ran those two and added a third to attack the fixes, all on the fixed tree. The concurrency stress (6 handles; store, gc, gc --checksum, clean, repair and damage at once; about 31k ops) gave 0 errors, a clean final checksum verify and an idempotent second gc. Every finding was reproduced by a regression test before being fixed. Round 2 added:
  - The build warning no longer names the Phase 3 `ApvmCache.repair()`.
  - The `verify` hint omits `gc` when only unreadable files remain.
  - `clean`/`clear` failure remedies no longer promise that `gc` removes symlinked dirs.
  - SKILL.md documents the "could not be stored" warning, and its corrupt-database row quotes the real text.
  - Store paths are made absolute when a store opens (`std::path::absolute`), so a cwd change cannot break `is_current` or the lock.

  Round 1:
  - **N1 (new in Phase 2):** gc deleted through symlinked dirs. Destructive paths in gc, clean, `delete_build` and `delete_release` now resolve through `layout::owned_dir` (store-shaped, no symlinks). `resolve_within_base` was removed.
  - **N2 (new):** on case-insensitive filesystems, records spelled differently share a dir, and gc emptying one wiped the other. Root fix: build and release dir allocation is case-insensitive (release collision → `{first100}-{hash8}`), and a re-store in another case replaces the old file record. Defense: the gc sweep matches live dirs case-insensitively, recomputed after commit; gc and clean never delete a file or dir another record names up to case.
  - **N3:** a damaged record outside the store layout had its file deleted.
  - **N4 (pre-existing):** mutations through a stale handle. They now fail with the new `Error::StaleHandle` (`store`, `store_release`, `clean`, `gc_with`, `delete_*`).
  - **N5 (pre-existing):** a dir that could not be inspected counted as vanished. Its records are now kept and the failure listed.
  - **N6:** a failed delete left an invisible file. Files now go first.
  - **A1:** a failed cache write during a build or warm was silent. It now emits a warning (`commands/cache.rs`).
  - **A2:** an open landing mid-repair reported "needs repair". `create()` re-checks `Orphaned` under the store lock.
  - **A3/A4:** the slot kept a store for an old `cache_dir`, or after caching was disabled.
  - **A5 (pre-existing):** a negative `user_version` was unrepairable. It is now `DatabaseCorrupted`.
  - **H1/H2:** slot compare-and-swap; stores are closed on the blocking pool.
  - **Nits:** symlink byte counts; the docs now cover artifact-less rows.
  - **Residual, documented:**
    - On Windows, a repair cannot quarantine a database an instance still holds.
    - Someone who can write the cache dir and swaps in a symlink or FIFO between a check and a deletion or hash (TOCTOU) is not defended. A FIFO could block a checksum pass.
    - On case-sensitive filesystems, an outside-made dir that differs only in case from a recorded one is kept by gc (a bounded leak; the store never creates such pairs).
    - The case tests run only on case-insensitive filesystems.
- **Tests:** 752 pass (695 before): storage +34, core +19, CLI +4. The permission tests skip as root, where permissions are not enforced.
- **Mutation testing:** 24 mutants over the new logic, all caught, each by the test written for it. Under each mutant, the test that caught it:
  - gc verdicts, the empty-row drop, the suspects re-check, the layout guard, byte counting and failure listing — the storage gc tests;
  - `probe_file` — its unit test;
  - `is_current` — the handle tests;
  - the slot's keep / reopen / hold — the `cache_status` tests;
  - error classification and warning text — `cache_status`;
  - the warning wiring — the `BuildCommand` unit tests and the e2e;
  - the CLI hint, flag and stderr list — the CLI tests.

  Only the stderr list survived at first; a test was added. Hashing every file under the lock instead of only the suspects is an equivalent mutant (performance only) and was not counted.
- **Verified on the real binary:**
  - A seeded cache through `verify`, then the hinted `gc` / `gc --checksum`, ends with `verify --checksum` clean.
  - An unreadable file is kept and reported on stderr, exit 0.
  - Against a corrupt cache, `apvm build imagify tag:v2.3.4` prints exactly one `⚠ … needs repair …` line and builds. After `apvm cache repair`, the next build is built and cached with no warning, and the one after is served from the cache.

- **New:** §3.3 and §3.4, including `apvm cache gc --checksum`. SKILL.md (per CLAUDE.md): the gc row, flag and output, the verify → gc remedy, and a troubleshooting row for the build warning. CLI README: gc and verify sections.
- **Tests:**
  - **Storage:** one test per damage kind (missing / size / same-size) — the command the hint names → `verify(Checksum)` is clean → a re-store re-caches it (the F12 probe, turned into tests). Unreadable files are kept and reported (Unix-gated, `chmod 000`); a record with an invalid size is dropped; `gc()` ≡ `gc_with(Size)`.
  - **Core,** on the existing offline `cache_e2e.rs` fixture (local repo + `SingleBuilder`): corrupt DB → `cache_status()` is `Corrupted`, and a build succeeds uncached and emits the warning; after `repair`, the **same instance's** next build is cached; the disabled-case warm warning is unchanged.
  - **CLI:** `gc --checksum` parsing; the verify hint for each problem kind.
- **Exit:** suite green; SKILL.md reconciled against `apvm cache --help` and `apvm cache gc --help`.

### Phase 3 — NAPI surface

- **New:** `crates/napi/src/cache.rs` (`ApvmCache` and the error-code helper); `crates/napi/src/cache_types.rs` (the §3.5 types and their `From` impls).
- **Changed:** `config.rs` → `resolve_config()`, used by all three factories; `apvm.rs` → `cache()` and `cacheStatus()`; `lib.rs` → modules and crate docs.
- **Rust tests (pure):** every `From` conversion, including each `VerifyProblem` / `IssueContext` arm and `None` → absent; option defaults (`None` → `All`, `false`); `resolve_config` precedence; the error → code mapping for every core and storage variant.
- **Node tests — new `__tests__/cache.test.ts`, fully offline:**
  - **Isolation:** each test sets its own `process.env.APVM_CACHE_DIR` and restores it afterwards. It overrides `cacheDir` (F5), so isolating via `cacheDir` alone would silently share the suite's dir.
  - **Missing dir:** all seven methods resolve, `exists` is `false`, the dir stays absent. **Same dir:** `open()` and `apvm.cache()` agree; the env var takes precedence.
  - **Seeded (F9):** `info` counts; `clean` dry-run / by project / `Builds`; `verify` `[]` → delete the zip → one `missing` issue → `gc()` → `damagedArtifacts: 1` → `verify` `[]`; flip one byte → only `verify({ checksum: true })` sees it → `gc({ checksum: true })` removes it; `clear`.
  - **Codes:** corrupt DB → `info()` rejects with `CacheCorrupted`; `repair()` quarantines and a second call is a no-op; `InvalidArg` for `olderThan: '30y'`, `project: 'Bad/Name'`, `target: 'Nope'`.
  - **Status:** `disabled` for `cacheEnabled: false`; `corrupted` for a DB corrupt at `create()` → `apvm.cache().repair()` → `active` on the same instance.
  - **Existing warm e2e:** additionally asserts that `apvm.cache().info()` lists `wp-rocket` (same-dir proof, no extra build).
- **Docs:** `crates/napi/README.md` (Cache Maintenance section, types, error-code table, the §3.6 recipes), `crates/core/README.md` (facade, `cache_status`), and the NAPI blurb in the root README.
- **Exit:** the locally generated `index.d.ts` matches §3.5 exactly.

### Phase 4 — Release 3.3.0

- Bump the workspace `version` and `package.json` to 3.3.0, and both `3.2.0` mentions in `SKILL.md` (lines 18 and 394 — enforced by the `CARGO_PKG_VERSION` test in `skill/embedded.rs`).
- **Exit:** CI green on all three OSes; CI commits the regenerated bindings; the committed `index.d.ts` diff is reviewed against §3.5.

---

## 5. Out of scope

- **Observed, unchanged:** quarantined `apvm.db.corrupt-*` files are never removed by `clean`, `gc` or `clear`.
- **Follow-ups:**
  - NAPI reading `~/.apvm/config.json` — today the CLI and NAPI share a dir only through the default or `APVM_CACHE_DIR` (F5).
  - Progress and cancellation for long `checksum` runs and `repair()` (the CLI has neither).
  - Per-entry list/delete: `list_builds` and `delete_build` exist in storage but are exposed nowhere.
