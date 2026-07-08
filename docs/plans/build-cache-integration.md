# Build Cache Integration Plan

Integrate the `apvm-storage` `ArtifactStore` into the build pipeline so that
builds are served from a persistent cache when possible and warm the cache
when not. Caching is **on by default**, with a persistent config toggle and a
per-invocation override, across `apvm-core`, the CLI, and the NAPI bindings.

The orchestration lives in `apvm-core`; CLI and NAPI only supply configuration
and surface results. `apvm-core` already depends on `apvm-storage` and already
carries the `BuildOutput` → storage conversion helpers this design builds on.

---

## 1. Concepts and directory model

Two directories, kept strictly separate:

- **Cache directory** — the persistent `ArtifactStore` base
  (`{base}/apvm.db` + artifact files). One per machine/user.
- **Output directory** — where a given build's artifacts are delivered
  (CLI: current dir by default; NAPI: `outputDir` per call). Per-build.

`apvm-config` and `apvm-core` define **no default** for the cache directory —
the consumer supplies it. Defaults live in the consumers:

- **CLI**: `{apvm_dir}/cache` → `~/.apvm/cache`.
- **NAPI**: `~/.apvm/cache` when `cacheDir` is omitted (a real cache must
  persist; callers wanting isolation set `cacheDir` or disable caching).

The `Config` field is named `cache_dir` (config-file key `cache_dir`, CLI key
`cache-dir`, NAPI `cacheDir`). A separate boolean controls whether caching is
active at all: `cache_enabled` (CLI key `cache`, NAPI `cacheEnabled`), default
`true`.

---

## 2. Cache key and the "commit before clone" requirement

Builds are cached under `(project, version, commit)`. To decide hit/miss
**before** paying for a clone, the resolved commit SHA must be known during the
pre-clone resolution phase. Current state of `ResolvedRef.commit_sha`:

| Ref form | commit SHA pre-clone | Source |
|---|---|---|
| `commit:<sha>` / bare hex | Yes | GitHub commits API (short→full) |
| `tag:X`, `branch:X`, bare tag/branch | Yes | `git ls-remote` in `RefResolver` |
| `tag:latest-stable` … (keyword tags) | Yes* | resolves to concrete tag → `ls-remote` |
| `pr:123` / bare digits (PR) | **No** | `resolve_pr` / early-resolve set `None` |
| `release:*` | N/A | keyed by tag, not commit (see §6) |

\* Verify during implementation that the keyword-tag path carries `commit_sha`
through `resolve_tag`; if a gap exists, the fallback in §4 applies (no cache,
build normally — never a failure).

**Solution for the PR gap.** GitHub's pull-request response already includes
`head.sha` in the same API call the resolver already makes. Capture it:

1. `github::models::PullRequest` gains `pub head_sha: Option<String>`.
2. `GitHubClient::get_pull_request` populates it from `pr.head.sha`.
3. `RefResolver::resolve_pr` and `BuildCommand::try_early_resolve_github_ref`
   set `commit_sha: pr.head_sha` instead of `None`.

The pre-clone SHA is used only for the cache *lookup*. The authoritative commit
for *storing* is always the actual `HEAD` after checkout (`get_head_commit_pair`),
so a PR that receives a new push between resolve and clone simply misses the
cache and stores under the real built commit — no inconsistency.

**Fallback contract.** If a commit SHA is genuinely unavailable (no remote, an
unverifiable ref), the build proceeds with no cache lookup. Absence of a SHA is
never an error.

---

## 3. Version and variant semantics

**Invariant: every artifact returned by a single build request is the same
version.** The store enforces this naturally — one build row is one
`(version, commit)` directory — so a single-row hit is inherently
single-version. The logic below preserves the invariant across partial reuse.

### Inputs

- `version: Option<&str>` — the user's `--ver` (may be ignored by embedded
  builders; see below).
- `strict_version: bool` — new flag (`--strict-version`; NAPI `strictVersion`).
- requested variants — resolved to a concrete set of variant keys
  (`Vec<Option<String>>`, where `None` is the variant-less artifact of
  single-output plugins). Empty request → the builder's default/all variants.

### The build version is authoritative

The version that will actually be built is `BuildCommand::resolve_version()`'s
output, computed **after checkout**:

- `Embedded` builders (e.g. WP Rocket) derive it from source and **ignore**
  `--ver`; therefore `--ver` and `--strict-version` do not apply and are
  reported as ignored, exactly as `--ver` already is today.
- `Required` / `Optional` builders honor `--ver`; without it they use the
  builder default or auto-detect.

All cache reuse keys off this authoritative version.

### Behavior matrix

| # | version | strict | Cache state (same commit) | Result |
|---|---|---|---|---|
| A | X | any | all requested variants present at X | **Full hit @X** — copy, no clone |
| B | none | — | all requested variants present at one version V | **Full hit @V** — copy, no clone (no warning; user pinned nothing) |
| C | X | false | all requested variants present at a single version Y≠X | **Full hit @Y** + **version-mismatch warning** suggesting `--strict-version` |
| D | X | true | not all present at X | Reuse the X-version variants that exist; **build only the missing** at X; combine |
| E | X | false | mixed versions, no single version has all | Reuse the X-version variants that exist; build the rest at X; combine (all @X) |
| F | any | any | commit not cached / files damaged | Build all at the resolved version |

Cases A–C are the pre-clone fast path (one `lookup_build` call). Cases D–F
resolve post-checkout once the authoritative version is known.

### Provenance (per-artifact) and color model

Every produced artifact carries its origin so consumers can report exactly
where each file came from — even in a partial build (some reused, some built):

```rust
pub enum ArtifactOrigin { Cache, Built, Downloaded }   // core
```

- `Cache` — copied from the `ArtifactStore`.
- `Built` — freshly produced by the builder.
- `Downloaded` — fetched from a GitHub Release (release path, §6).

`ProducedArtifact` gains `pub origin: ArtifactOrigin`, set wherever an artifact
is created (build runner → `Built`; cache copy → `Cache`; release download →
`Downloaded`). `BuildOutput.from_cache` becomes a derived convenience
(`all origins == Cache`); the per-artifact `origin` is the source of truth.

**Color model — provenance is informational, warning color is for ambiguity
only.** A cache hit is expected and desirable, so it must not be dressed as a
warning (that would desensitize the user to real warnings). Therefore:

- Provenance is rendered in **neutral/accent** styling (e.g. dim/cyan
  `cache`, green `built`, blue `downloaded`) — scannable, not alarming.
- **Warning color (yellow) is reserved for the case-C version mismatch**: the
  user pinned `--ver X` but a lenient hit returned version `Y`. This is the one
  situation where the user could be surprised, so it is the one that draws
  attention and offers the remedy (`--strict-version`).

Case C is signaled two ways so it survives scrollback and is machine-readable:
`BuildEvent::Warning` (emitted live during the lookup) **and**
`BuildOutput.cache_version_mismatch: bool` (+ the requested vs actual version
on the output). The user is never silently handed a different version than
requested.

### CLI output (provenance + summary)

The final results block reports every artifact's origin and a one-line summary,
in both normal and verbose modes:

```text
Build complete: PR #8556 @ a1b2c3d
  Commit:  a1b2c3d
  Version: 3.17.4
  Artifacts:
    built     free.zip
    cache     pro-en.zip
  Source: 1 built, 1 from cache
```

A full hit shows `Source: all from cache`; a full build shows `all built`; a
release path shows `downloaded` / `from cache`. Coloring honors `NO_COLOR` and
non-TTY output (plain labels when color is unavailable). The case-C mismatch is
printed as a separate yellow block above the summary:

```text
⚠ Requested version 3.17.4 but the cache holds this commit as 3.16.0.
  Returning the cached 3.16.0 artifacts. Pass --strict-version to rebuild at 3.17.4.
```

---

## 4. Cache decision flow (core)

Effective caching is on when `store.is_some()` (opened successfully and
`config.cache_enabled`) **and** the per-call override did not pass `--no-cache`.

```
resolve_reference()                      # pre-clone; yields ResolvedRef (+commit_sha)

if caching && resolved.commit_sha.is_some():
    # ---- PRE-CLONE FAST PATH (cases A/B/C) ----
    predicted = predicted_version(builder, version)   # None for embedded builders
    hit = store.lookup_build(commit, require=variant_keys,
                             version = predicted, match = strict?Strict:Lenient)
    if hit:
        if predicted.is_some() && !hit.version_matched: warn_version_mismatch()
        copy_requested_variants(hit -> output_dir)
        return BuildOutput{ from_cache: true, ... }     # NO clone / build

# ---- clone → checkout → resolve authoritative version ----
workspace.clone(); checkout(resolved); commit = HEAD
actual_version = resolve_version(...)   # source of truth

if caching:
    # ---- POST-CHECKOUT PARTIAL REUSE (cases D/E/F) ----
    existing = store.get_existing_variants(project, actual_version, commit)
    reuse   = variant_keys ∩ existing
    build   = variant_keys - existing
    if build.is_empty():                # everything already cached at actual_version
        copy_reused(reuse -> output_dir); return from_cache
else:
    build = variant_keys

built = run_build(builder, actual_version, variants=build)   # subset build
copy_reused(reuse -> output_dir); collect(built -> output_dir)

if caching:
    store.store(metadata@actual_version, all_requested_artifacts_in_output_dir)  # best-effort

return BuildOutput{ from_cache: (build.is_empty()), ... }
```

`predicted_version(builder, version)`: `None` for `Embedded` builders;
otherwise `version`, else `builder.default_version()`, else `None`. When
`None`, the fast-path lookup is version-unpinned (case B). `strict_version` is
inert when there is no predicted version (nothing to be strict about) and is
reported ignored for embedded builders.

`store.store(...)` is idempotent and self-healing, so storing the full
requested set (reused files are recognized and reused, not recopied) keeps the
row complete and refreshes `last_used_at`. Any pre-existing variants not in this
request are left untouched.

### Resilience (non-negotiable)

Opening the store, every lookup, every copy-from-cache, and every store call
are **best-effort**. On any storage error: log a warning, treat as a cache miss
(or, for the store-after-build step, simply skip), and let the build proceed.
A cache problem must never fail a build whose artifacts are otherwise fine.
Storage calls run on `tokio::task::spawn_blocking` (the store is blocking
SQLite + file I/O; builds are async and may overlap).

---

## 5. Refactor of the build command (abstraction)

`BuildCommand::execute` is decomposed so no single method owns the whole
pipeline. Proposed private structure in `crates/core/src/commands/build.rs`
(names indicative):

- `resolve_reference(git_ref, project_info, reporter) -> ResolvedRef`
  — folds early GitHub resolution + remote `RefResolver` into one step.
- `cache_fast_path(ctx, resolved, reporter) -> Option<BuildOutput>`
  — cases A/B/C; returns `Some` on a full hit.
- `run_clone_build(ctx, resolved, reporter) -> BuildOutput`
  — clone/checkout/version-resolve, then:
  - `plan_reuse(actual_version, commit) -> ReusePlan{ reuse, build }`
  - `materialize(reuse, built, output_dir)` — copy cached + collect built.
  - `store_build(metadata, artifacts)` — best-effort warm.
- `download_release(...)` — gains `find_release` / `store_release` (§6).
- Small helpers: `variant_keys(builder, requested) -> Vec<Option<String>>`,
  `predicted_version(builder, version)`, `copy_cached_to_output(...)`,
  `warn_version_mismatch(reporter, requested, got)`.

A `CacheContext` (borrows `&ArtifactStore`, project, commit, variant keys,
version, `strict_version`) carries the inputs so the fast-path and reuse
deciders are independently unit-testable without git/GitHub.

### Public API surface

Introduce a request struct so per-call options don't sprawl into positional
args:

```rust
pub struct BuildRequest<'a> {
    pub project: &'a str,
    pub version: Option<&'a str>,
    pub git_ref: &'a str,
    pub variants: &'a [&'a str],       // empty = builder defaults
    pub output_dir: &'a Path,
    pub no_cache: bool,                // per-call override
    pub strict_version: bool,
}
```

`Apvm::build(&self, req: BuildRequest<'_>, reporter)` becomes the primary
entry; existing convenience methods (`build_from_pr`, `_branch`, `_tag`,
`_commit`) construct a `BuildRequest` with `no_cache: false, strict_version:
false`. `download_release` keeps its own signature (releases have no
variant/version knobs) and honors the cache toggle + `no_cache`.

`BuildOutput` gains:
- `pub from_cache: bool` — derived: true when every artifact origin is `Cache`.
- `pub cache_version_mismatch: bool` — true only in case C.
- `pub requested_version: Option<String>` — what the user pinned, for the
  case-C message (actual version is already on `result.version`).

`ProducedArtifact` gains `pub origin: ArtifactOrigin` (§3) — the per-artifact
source of truth for provenance reporting.

---

## 6. Release caching (in scope)

`download_release` uses the storage release API (keyed by tag; assets have no
variant). Keyword releases are already resolved to a concrete tag before the
cache is consulted (required — keywords are moving targets).

Flow when caching is active:

1. Resolve the release (GitHub API) → concrete tag + asset list; filter assets
   to the requested variants (existing logic).
2. `store.find_release(project, tag)`:
   - If present and **all** requested asset filenames are healthy → copy them
     to `output_dir`, return `from_cache: true`. No download.
   - Otherwise download the missing assets, copy all requested to `output_dir`.
3. `store.store_release(metadata, assets)` — best-effort warm (idempotent,
   heals, adds newly downloaded assets to the existing row).

Same resilience and `spawn_blocking` rules as builds. `--no-cache` and the
config toggle apply identically.

---

## 7. Cache maintenance CLI (in scope)

New `apvm cache` subcommand group wrapping the storage maintenance API
(`usage`, `clean`, `gc`, `verify`, `repair`, `clear_all`):

- `apvm cache info` — `ArtifactStore::usage()`: totals, per-project breakdown,
  cache dir path.
- `apvm cache clean [--older-than <dur>] [--project <p>] [--dry-run] [--releases|--builds]`
  — `clean(CleanOptions...)`; dry-run prints what would be freed.
- `apvm cache gc` — `gc()` reconcile db↔disk, sweep temp files.
- `apvm cache verify [--checksum]` — `verify(VerifyMode)`.
- `apvm cache repair` — `repair()` (quarantine + rebuild) for a corrupt db.
- `apvm cache clear [--yes]` — `clear_all()` with confirmation.

These operate on `config.cache_dir` and are independent of `cache_enabled`
(you can still inspect/clean a cache you've turned off). Errors surface
normally here (unlike the build path, the user explicitly asked for the
operation).

---

## 8. Changes by crate (reference)

### `apvm-config` (`crates/config/src/config.rs`)
- `Config`: rename `builds_dir` → `cache_dir` (required `PathBuf`, no default);
  add `cache_enabled: bool` (default `true` via `Config::new`); add
  `set_cache_enabled`.
- `ConfigFile`: `cache_dir: Option<PathBuf>`, `cache_enabled: Option<bool>`
  (sparse). Update `merge`/`is_empty`/`set`/`unset`/`get`.
- `ConfigKey`: replace `BuildsDir` with `CacheDir` (`"cache-dir"`); add `Cache`
  (`"cache"`). Update `all`, `Display`, `FromStr`, `is_sensitive` (both
  non-sensitive). `Cache` get/set marshals `"true"`/`"false"` (validation in
  CLI sanitizer).
- Update this crate's tests to the renamed/added keys.

### `apvm-core`
- `error.rs`: add `Storage(#[from] apvm_storage::Error)`.
- `github/models.rs` + `github/client.rs`: `PullRequest.head_sha` from
  `pr.head.sha`.
- `git/resolver.rs`: `resolve_pr` sets `commit_sha: pr.head_sha`.
- `build/result.rs` (produced artifact): add `ArtifactOrigin { Cache, Built,
  Downloaded }` and `ProducedArtifact.origin`; set it at every construction
  site (runner → `Built`, release download → `Downloaded`, cache copy →
  `Cache`).
- `commands/build.rs`: PR early-resolve sets `commit_sha`; decompose per §5;
  add cache read/write and release caching (§4, §6); `BuildOutput.from_cache`
  (derived), `cache_version_mismatch`, `requested_version`.
- `build/progress.rs`: `BuildPhase::Cache` ("Checking cache") for lookup
  progress; reuse `BuildEvent::Warning` for the case-C mismatch signal.
- `lib.rs`: `Apvm.store: Option<Arc<ArtifactStore>>`, opened best-effort in all
  constructors when `cache_enabled`; `BuildRequest`-based `build`; helpers
  `cache_active()`, `cache_dir()`.

### CLI (`crates/cli`)
- `defaults.rs`: replace `default_builds_dir` with `default_cache_dir`
  (`{apvm_dir}/cache`); `paths.rs`: derive `cache_dir = apvm_dir/cache`
  (drop the separate builds-dir param); `main.rs`: update `Paths::new` call.
- `sanitize.rs`: `sanitize_bool` for `ConfigKey::Cache`; path sanitizer now
  serves `CacheDir`.
- `commands/config.rs`: `default_for_key` for `CacheDir` and `Cache`.
- `commands/build.rs`: `--no-cache` and `--strict-version` args → `BuildRequest`;
  render per-artifact provenance + the `Source:` summary (§3), and the case-C
  version-mismatch as a separate yellow block. A small color helper honors
  `NO_COLOR` / non-TTY.
- `commands/cache.rs` (new) + `main.rs` dispatch: the `apvm cache` group (§7).

### NAPI (`crates/napi`)
- `config.rs`: `ApvmConfig.cacheDir` + `cacheEnabled`; default cache dir
  `~/.apvm/cache` (add `directories` dep); map into `Config`.
- `types.rs`: `BuildOptions.noCache` + `strictVersion`; `JsBuildOutput.fromCache`
  + `cacheVersionMismatch` + `requestedVersion`; `JsProducedArtifact.origin`
  (`"cache" | "built" | "downloaded"`) so JS consumers get the same per-artifact
  provenance the CLI shows.
- `error.rs`: handle `Error::Storage` (reuse `storage_error_to_napi`, drop its
  `#[allow(dead_code)]`).
- `apvm.rs`: thread the new `BuildOptions` fields into `BuildRequest`; update
  TSDoc; regenerate `.d.ts` and confirm the new fields appear.

---

## 9. Phases

Each phase is independently reviewable, compiles, and keeps existing builds
working. Verify at each boundary before moving on.

**Phase 1 — Config & path plumbing.**
Rename `builds_dir`→`cache_dir` (no default in config/core); add `cache_enabled`
+ `Cache`/`CacheDir` keys; CLI default `~/.apvm/cache`; NAPI `cacheDir`/
`cacheEnabled`; bool sanitizer; `config` command coverage. No caching behavior
yet.
*Verify:* `apvm config` shows `cache`/`cache-dir`; `set/get/unset` work; a build
still runs unchanged.

**Phase 2 — Commit SHA for PRs.**
`PullRequest.head_sha`; populate `ResolvedRef.commit_sha` in resolver and
early-resolve.
*Verify:* resolving a PR yields a `commit_sha` (unit test against the client
model; confirm no mock harness gap first).

**Phase 3 — Core scaffolding & refactor.**
`Error::Storage`; open `ArtifactStore` in `Apvm` (best-effort, `None` on
failure); `BuildRequest`; decompose `execute` into the §5 helpers; add
`BuildOutput.from_cache`/`cache_version_mismatch` (always false); `BuildPhase::Cache`.
Pure refactor + wiring, no cache reads/writes.
*Verify:* full build suite passes; opening a bad cache dir leaves `store: None`
and does not fail `Apvm::new`.

**Phase 4 — Build cache read/write.**
Fast-path full hit (A/B/C), post-checkout partial reuse (D/E/F), best-effort
store-after-build, version-mismatch warning, `--no-cache`, `--strict-version`.
*Verify:* build same PR/commit twice → second is a hit; request one extra
variant → partial reuse builds only the new one and the CLI shows each
artifact's origin (`cache`/`built`) + `Source:` summary; `--ver` mismatch shows
the yellow case-C block and `--strict-version` forces a rebuild; `--no-cache`
and `cache=false` always rebuild.

**Phase 5 — Release caching.**
`find_release`/`store_release` in `download_release`, honoring toggle/`--no-cache`.
*Verify:* download same release twice → second from cache; requesting a new
variant/asset downloads only the missing one.

**Phase 6 — Cache maintenance CLI.**
`apvm cache info|clean|gc|verify|repair|clear`.
*Verify:* against a populated cache — `info` totals match, `clean --dry-run`
reports without deleting, `gc`/`verify` run clean, `clear --yes` empties it.

---

## 10. Testing checklist

- **config**: key parse/display/serialize/merge for `Cache` + `CacheDir`;
  `cache_enabled` defaulting and sparse round-trip.
- **core**:
  - `PullRequest.head_sha` populated; PR `ResolvedRef.commit_sha` set.
  - `CacheContext` fast-path decisions (A/B/C) and `plan_reuse` (D/E/F) as
    isolated unit tests using a temp `ArtifactStore` pre-seeded via `store()` —
    no network/git.
  - store-open failure ⇒ `store: None`, build proceeds.
  - store/lookup error mid-build ⇒ warning, build still succeeds.
  - all-same-version invariant asserted for partial-reuse outputs.
  - per-artifact `origin` correct across full hit (all `Cache`), full build
    (all `Built`), partial (mixed), and release (`Downloaded`/`Cache`).
- **cli**: `sanitize_bool`; `config get/set/unset cache`; `--no-cache` /
  `--strict-version` parsing; `apvm cache` subcommands against a temp cache.
- **napi**: regenerate types; smoke test `cacheEnabled:false` (always builds)
  and `true` (second build `fromCache:true`); `strictVersion`/`noCache` honored.
- **manual e2e**: build the same PR twice with the real CLI (second near-instant,
  cache line shown); `apvm config set cache false` reverts to always-rebuild.
