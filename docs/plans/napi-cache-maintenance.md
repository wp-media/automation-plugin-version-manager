# NAPI cache maintenance — parity with `apvm cache`

Status: **planned** · Target version: **3.3.0** (from 3.2.0 — also claimed by the site-deployment plan; whichever lands second takes the next minor) · Author: Sandy Figueroa

---

## TL;DR

| | |
|---|---|
| **Problem** | Node consumers can build, bypass (`noCache`) and warm (`warmCache()`) the cache, but cannot inspect, clean, gc, verify, repair or clear it — they need the CLI pointed at the same directory. Verifying the CLI also surfaced three defects that hit both front ends (F4, F11, F12). |
| **Goal** | Every `apvm cache` action callable from Node with the same semantics, on the same directory, regardless of `cacheEnabled` — and the defects fixed once, at the root, for both front ends. |
| **Approach** | A core facade, `apvm_core::maintenance::CacheMaintenance`, owns every maintenance decision. The CLI and NAPI become thin adapters over it, so parity comes from shared code, not discipline. |
| **JS API** | `ApvmCache` with `info()`, `clean()`, `gc()`, `verify()`, `repair()`, `clear()` — 1:1 with the subcommands — from `apvm.cache()` (the dir that instance builds into) or `ApvmCache.open(config?)` (standalone, no GitHub client). Plus `apvm.cacheStatus()`. |
| **Errors** | Every `ApvmCache` rejection carries a stable `err.code`: `CacheCorrupted` (→ `repair()`), `InvalidArg`, `GenericFailure`. Delivered through a JS-thread callback, spike-verified on napi 3.13. |
| **Fixes** | **F4** input is validated before the missing-dir guard. **F11** an unusable cache is reported (status + build warning) and re-attached automatically once repaired. **F12** `gc` removes damaged entries (`--checksum` catches same-size corruption), which makes verify's hint true. |
| **Invariants** | Never creates a missing cache dir. Validates before touching disk. All I/O runs off the event loop. |
| **Deliberate differences** | `clear()` has no prompt. `verify()` resolves with the issue list instead of rejecting. |
| **CLI impact** | Output unchanged except the fixes: the F4 error, the F11 warning, `gc --checksum` + one gc line + a corrected hint (F12). SKILL.md is updated for exactly those. |
| **Deps** | `chrono` added to `apvm-core` (already a workspace dependency). Nothing new in the root `Cargo.toml`. |
| **Phases** | 1 facade + F4 → 2 F11 + F12 → 3 NAPI surface → 4 release 3.3.0. |

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
| F4 | **Bug:** the guard runs before validation — `clean --older-than 30y --project Bad/Name` exits 0 on a missing dir, 1 once the dir exists. For a library that is a CI-only failure (fresh runners have no cache). | ran |
| F5 | NAPI resolves `cacheDir` → `~/.apvm/cache`, then `APVM_CACHE_DIR` wins (set from JS, it beat an explicit `cacheDir`). NAPI never reads `~/.apvm/config.json`. | ran from Node + read |
| F6 | Existing rejections carry `err.code` = the napi `Status` (`"InvalidArg"` for an unknown project). | ran from Node |
| F7 | Spike on the locked versions (napi 3.13.0, derive 3.6.9, cli 3.10.5): a method can return another class ✅; a sync factory taking an optional object config ✅; `spawn_blocking` keeps the event loop live ✅; a bad string-enum value → `InvalidArg` ✅. A custom code through an `async fn` return type (`napi::Result<T, CustomStatus>`) does **not** compile — but `Env::spawn_future_with_callback` + `create_error` + a `code` property **does**: the promise rejects with `code: "CacheCorrupted"`, still `instanceof Error` with message and stack, 8 concurrent calls behave, and `ts_return_type` yields `Promise<T>`. | spike |
| F8 | Mutations hold `StoreLock` (per-descriptor `File::lock`, so two stores in one process exclude each other) plus WAL with a 5 s busy timeout. A second store beside a live `Apvm` behaves like `apvm cache` run from another process: builds degrade to a miss, never fail. | read + `lock_excludes_second_acquisition_until_dropped` |
| F9 | `repair()` on a healthy DB is a no-op. A garbage `apvm.db` next to `<project>/commits/<ver>/<7-hex>/<file>.zip` makes `repair()` re-index those builds → **offline test seeding through the public API**. | ran (2 builds adopted) |
| F10 | Unfiltered `clean` deletes everything without a prompt, so `clear`'s prompt is UI, not a safety boundary. | ran |
| F11 | **Bug (both front ends):** with a corrupt or unopenable cache every build is silently uncached. Only `warmCache` warns, and only "disabled or unavailable"; the one signal carrying the reason is a `tracing` log, hidden unless `RUST_LOG` is set. | ran CLI + read |
| F12 | **Bug:** damaged artifacts, via the storage API. *Missing file:* lookup misses, re-store heals, `gc` drops 0. *Size mismatch:* lookup misses, re-store heals, `gc` ignores it. *Same-size corruption:* **served as a cache hit, re-store reuses it, `gc` ignores it** — only `verify --checksum` sees it, and nothing short of `clean`/`clear` removes it. So verify's hint ("run `apvm cache gc` … or rebuild to heal them") has `gc` advice that fixes none of the three, and "rebuild" fails for the third. | probe crate |

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

impl CacheMaintenance {
    pub fn new(dir: impl Into<PathBuf>) -> Self;
    pub fn dir(&self) -> &Path;
    pub fn exists(&self) -> bool;                         // the F2 guard; never creates
    pub fn usage(&self) -> Result<UsageReport>;
    pub fn clean(&self, request: &CleanRequest) -> Result<CleanReport>;
    pub fn clear(&self) -> Result<CleanReport>;
    pub fn gc(&self, mode: VerifyMode) -> Result<GcReport>;          // mode: Phase 2 (F12)
    pub fn verify(&self, mode: VerifyMode) -> Result<Vec<VerifyIssue>>;
    pub fn repair(&self) -> Result<RepairReport>;
}

#[derive(Debug, Clone, Default)]
pub struct CleanRequest {                                // CLI flags ≡ JS options, 1:1
    pub older_than: Option<String>,                      // "30d" — parse_duration grammar
    pub project: Option<String>,
    pub target: CleanTarget,                             // apvm_storage::CleanTarget (default All)
    pub dry_run: bool,
}

pub fn parse_duration(spec: &str) -> std::result::Result<chrono::Duration, String>; // moved verbatim from the CLI
```

- **Order:** validate → guard → open → operate. Validation checks the duration first, then the project (today's order); the project rule is a new public `apvm_storage::CleanOptions::validate()`, which `clean()` itself also calls — one rule, one place.
- **Missing dir:** the report type's `Default` (`clean` keeps `dry_run`). **Corrupt DB:** `Error::Storage(DatabaseCorrupted)` untouched; each adapter adds its own hint or code.
- **Blocking by design,** like storage: the CLI calls it directly, NAPI wraps it. The age cutoff (`now − duration`) is a private pure fn taking `now`, so age tests are deterministic.

### 3.3 F11 — an unusable cache is visible and heals itself

- **A slot, not a fixed `Option`:** `Apvm` keeps its store in a private `Mutex` slot (poison-tolerant, like `ArtifactStore::conn`). While caching is enabled but the store is not open, each `build`, `warm_cache` and `cache_status` call retries the open once — one SQLite open, the cost the constructors already pay. A `repair()` from anywhere (NAPI, the CLI, another process) therefore re-enables caching on live instances; nothing has to be re-created.
- **Status:** `pub fn cache_status(&self) -> CacheStatus` = `Active | Disabled | Corrupted { details } | Unavailable { details }`, classified from the open error. `cache_active()` (used only by tests today) is derived from it.
- **Warning:** when caching is enabled but unavailable, `build` and `warm_cache` emit exactly one `BuildEvent::Warning` with the reason and the remedy (for `Corrupted`, naming both `apvm cache repair` and `ApvmCache.repair()`). `Apvm` hands the status to `BuildCommand` through a new non-breaking setter, so the 13 existing `BuildCommand::new` call sites are untouched and the generic warm warning fires only for the disabled case, with its text unchanged (pinned by `cache_e2e.rs:584`). The CLI already prints warnings as `⚠ …`; NAPI delivers them to `onProgress`.

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

### Phase 1 — Core facade; CLI on top (pure refactor + F4)

- **New:** `crates/core/src/maintenance.rs` (§3.2; `gc` takes no mode yet) and `pub mod maintenance`; `chrono = { workspace = true }` in `crates/core/Cargo.toml`; public `CleanOptions::validate()` in storage.
- **Changed:** `crates/cli/src/commands/cache.rs` keeps only printing, `human_bytes`, the `clear` prompt, the corruption hint and verify's exit 1. `parse_duration` and its 2 tests move to core (the CLI keeps `chrono` for `update_check.rs`).
- **Tests (core, temp dirs, offline):**
  - The six ops on a missing dir → default report, dir still absent.
  - F4 inputs rejected on a missing dir; with both invalid, the duration error wins (today's order).
  - Seeded store (`ArtifactStore::store`, as the CLI tests do) → `usage`; `clean` by project / target / age (injected `now`) / dry-run; `clear`; `gc`; `verify` in both modes.
  - Corrupt DB → `DatabaseCorrupted` from every op except `repair`, which quarantines and adopts.
- **Exit:** the 7 remaining CLI tests pass unmodified, plus 1 for F4. On copies of one cache seeded offline (F9), every action's output diffs identical between the pre- and post-change binaries.

### Phase 2 — F11 and F12 (core, storage, CLI; no JS change)

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
