# APVM NAPI — Node.js Bindings

Node.js bindings for the **Automation Plugin Version Manager**, powered by [`napi-rs`](https://napi.rs/) 3. Exposes the [`apvm-core`](../core/README.md) library as a native addon with full async/Promise support on the [Tokio](https://docs.rs/tokio/1) runtime.

All build operations return JavaScript Promises and run on the Tokio runtime without blocking the Node.js event loop. Progress events are delivered via a thread-safe callback using [`ThreadsafeFunction`](https://docs.rs/napi/3/napi/threadsafe_function/struct.ThreadsafeFunction.html). The callback is best-effort: an exception it throws, or a rejection of a promise it returns (an `async` callback), is discarded, so it never fails the call or crashes the process. Discarded errors do not reach `process.on('uncaughtException')` or `'unhandledRejection'`, so handle errors inside the callback if you need to see them. Methods are safe to call from worker threads: a worker may exit while its calls are still pending without affecting the rest of the process.

When the thread that made a call goes away first, the processes the call started are killed. A worker that exits or is terminated cancels its pending `Apvm` calls (every `Apvm` method that returns a promise): every process they started — git and its helpers, build steps and the tools these run — is killed with its whole process tree, and their async work is dropped (a file or cache write already under way on a blocking thread finishes first). The worker's teardown waits for that, at most 2 s. A worker exiting stops only its own calls, never another thread's. When the main thread ends the process — `process.exit()`, an uncaught exception or unhandled rejection, or its event loop emptying while a worker still runs — the processes of every thread's calls are killed too; for this, the first call on each thread adds one `exit` listener to its `process` (it ignores a `process.emit('exit')` that is no real exit). On Windows, only each call's own child process is killed, not the processes that child started. `ApvmCache` methods start no processes and run to completion. Not covered: a process that leaves its tree by outliving its own parent (e.g. a daemon a tool starts), and a Node.js process killed by a signal it does not handle (e.g. `SIGTERM`, `SIGKILL`), which runs no cleanup, so processes it started may outlive it.

## Requirements

- **Runtime**: Node.js ≥ 18 ([N-API 8](https://nodejs.org/docs/latest-v18.x/api/n-api.html))
- **Build from source** (only if no prebuilt binary for your platform): Node.js ≥ 20 + Rust toolchain

## Prebuilt Binaries

Prebuilt `.node` binaries are shipped for these platforms and loaded automatically:

| Binary                           | Platform                   |
|----------------------------------|----------------------------|
| `apvm-napi.darwin-arm64.node`    | macOS (Apple Silicon)      |
| `apvm-napi.darwin-x64.node`     | macOS (Intel x64)          |
| `apvm-napi.linux-x64-gnu.node`  | Linux x86_64 (GNU/glibc)  |
| `apvm-napi.linux-arm64-gnu.node`| Linux arm64 (GNU/glibc)   |
| `apvm-napi.win32-x64-msvc.node` | Windows x64 (MSVC)        |

If your platform is not listed, the package install attempts to compile the addon locally. Building from source requires:

- Node.js 20+ (build step only; the compiled addon runs on Node.js 18+)
- Rust toolchain (`rustup`, `cargo`, `rustc`) on `PATH`
- C compiler and linker. On Debian/Ubuntu: `build-essential`, `pkg-config`, `libssl-dev`

```sh
# Build locally (release)
npm i
npm run build
```

## Quick Start

```ts
import { Apvm } from 'apvm-napi';

// Create an instance with automatic GitHub token resolution.
// Tries: config → GITHUB_TOKEN env → GH_TOKEN env → gh CLI → gh config file.
// A token is required for private repos (e.g., BackWPup) and recommended
// for public repos to get higher API rate limits (5,000 vs 60 req/hour).
const apvm = await Apvm.createWithTokenResolution();

// Build WP Rocket from the 'develop' branch.
// The gitRef field accepts any format: PR number, branch, tag, commit SHA,
// release tag, or special keywords like 'tag:latest-stable'.
// outputDir is where the built .zip artifacts will be placed.
const output = await apvm.build({
  project: 'wp-rocket',
  gitRef: 'develop',
  outputDir: '/tmp/output',
});

// Human-readable summary of what was built (e.g., "branch 'develop' @ a1b2c3d")
console.log(output.description);

// The detected or provided version string (e.g., "3.17.4")
console.log(output.result.version);

// Short commit SHA (7 chars) of the exact commit that was built
console.log(output.commitShort);

// List the filename of each produced artifact (e.g., ["wp-rocket-3.17.4.zip"])
console.log(output.result.artifacts.map((a) => a.filename));
```

## API Reference

### `Apvm` Class

The main entry point. Created via async factory methods.

#### `Apvm.create(config?): Promise<Apvm>`

Creates an instance using the provided config. If `githubToken` is set, it is used directly.

```ts
// Minimal — default cache dir (~/.apvm/cache), no token
const apvm = await Apvm.create();

// With explicit config
const apvm = await Apvm.create({
  githubToken: 'ghp_xxxxxxxxxxxx',
});
```

> **Why is `create()` async?** The underlying HTTP client ([octocrab](https://docs.rs/octocrab/0.54)) requires a [Tokio](https://docs.rs/tokio/1) runtime during initialization.

#### `Apvm.createWithTokenResolution(config?): Promise<Apvm>`

Creates an instance with automatic GitHub token resolution. Tries these sources in order:

1. `githubToken` field in the config (if provided)
2. `GITHUB_TOKEN` environment variable
3. `GH_TOKEN` environment variable
4. `gh auth token` command ([gh CLI](https://cli.github.com/) ≥ 2.17.0)
5. gh CLI config file (`~/.config/gh/hosts.yml`)

```ts
const apvm = await Apvm.createWithTokenResolution();
console.log('Has token:', apvm.hasToken());       // true/false
console.log('Source:', apvm.tokenSource());        // e.g., "GITHUB_TOKEN"
```

### Instance Methods

#### `apvm.hasToken(): boolean`

Returns `true` if a GitHub token is available. Required for private repos (e.g., BackWPup).

#### `apvm.tokenSource(): string | null`

Returns where the token came from: `"config file"`, `"GITHUB_TOKEN"`, `"GH_TOKEN"`, `"gh auth token"`, or `"gh config"`. Returns `null` if no token.

#### `apvm.listProjects(): string[]`

Returns registered project names (e.g., `["backwpup", "wp-rocket"]`).

### Build Methods

#### `apvm.build(options, onProgress?): Promise<JsBuildOutput>`

The primary build method. Accepts a flexible `gitRef` string.

```ts
const output = await apvm.build(
  {
    project: 'backwpup',
    gitRef: 'pr:123',
    version: '5.1.0',
    variants: ['free', 'pro-en'],
    outputDir: '/tmp/output',
  },
  (err, event) => {
    if (err || !event) return;
    if (event.type === 'phase_started') {
      console.log(`[${event.phase}] ${event.message}`);
    }
  },
);
```

#### Convenience Methods

| Method | Equivalent `gitRef` |
|--------|---------------------|
| `buildFromPr(project, prNumber, outputDir, version?, variants?, onProgress?)` | `pr:{prNumber}` |
| `buildFromBranch(project, branch, outputDir, version?, variants?, onProgress?)` | `branch:{branch}` |
| `buildFromTag(project, tag, outputDir, version?, variants?, onProgress?)` | `tag:{tag}` |
| `buildFromCommit(project, commit, outputDir, version?, variants?, onProgress?)` | `commit:{commit}` |

All return `Promise<JsBuildOutput>`.

### Cache Warming

#### `apvm.warmCache(options, onProgress?): Promise<JsBuildOutput>`

Runs the **same pipeline** as `build()` — resolve the ref, reuse whatever is already cached, and build or download only what is missing — then stores everything into the cache. The one difference is that it delivers **nothing** to an output directory; its purpose is to prime the cache so a later `build()` of the same reference is an instant hit.

`WarmOptions` is deliberately smaller than `BuildOptions`: there is **no `outputDir`** (nothing is delivered) and **no `noCache`** (warming _is_ a cache operation).

```ts
// Prime the cache for WP Rocket's develop branch (no output directory).
await apvm.warmCache({ project: 'wp-rocket', gitRef: 'branch:develop' });

// A later build of the same ref is then served entirely from the cache.
const output = await apvm.build({
  project: 'wp-rocket',
  gitRef: 'branch:develop',
  outputDir: '/tmp/output',
});
console.log(output.fromCache); // true
```

The returned `JsBuildOutput` describes what was cached. Each artifact's `origin` distinguishes what was already cached (`"cache"`) from what had to be `"built"` or `"downloaded"` to warm it; artifact `path`s point at their canonical locations inside the cache, not an output directory.

### Cache Maintenance

`ApvmCache` maintains the artifact cache from Node, at parity with `apvm cache` (one shared implementation in `apvm-core`, so both behave the same).

```ts
import { Apvm, ApvmCache, JsCleanTarget } from 'apvm-napi';

const apvm = await Apvm.create({});
const cache = apvm.cache();                                // the cache this instance builds into
const other = ApvmCache.open({ cacheDir: '/var/cache/apvm' }); // standalone: no GitHub client needed

await cache.clean({ olderThan: '30d', project: 'backwpup', target: JsCleanTarget.Builds });
const issues = await cache.verify({ checksum: true });     // [] = healthy
if (issues.length > 0) await cache.gc({ checksum: true }); // removes what it can (see gc below)
```

| Method | CLI | Resolves with |
|---|---|---|
| `ApvmCache.open(config?)` | — | a handle (synchronous) for `cacheDir` — default `~/.apvm/cache`, `APVM_CACHE_DIR` wins — made absolute |
| `apvm.cache()` | — | a handle (synchronous) for the directory the instance builds into, fixed at `create()` |
| `cache.dir()` | — | the directory (synchronous) |
| `cache.info()` | `cache info` | `JsCacheUsage` |
| `cache.clean(options?)` | `cache clean` | `JsCleanReport` — no options removes everything |
| `cache.clear()` | `cache clear` | `JsCleanReport` — no prompt: the call is the consent |
| `cache.gc(options?)` | `cache gc [--checksum]` | `JsGcReport` — removes missing/damaged entries; keeps files it cannot read (listed in `failures`) |
| `cache.verify(options?)` | `cache verify [--checksum]` | `JsVerifyIssue[]` — resolves with the issues, never rejects for them |
| `cache.repair()` | `cache repair` | `JsRepairReport` — a no-op on a healthy cache |
| `apvm.cacheStatus()` | — | `JsCacheStatus` |

**Behavior**

- **Independent of `cacheEnabled`:** a disabled cache can still be inspected and cleaned. Creating a handle touches nothing on disk.
- **No cache yet:** a missing or empty directory resolves every method with an empty report and creates nothing. `info().exists` is `false` then — often a sign of the wrong `cacheDir`.
- **Only its own directories:** a cache is created only in a missing or vacant directory (nothing but `.DS_Store` / `Thumbs.db` / `desktop.ini`) — `Apvm.create()` on a directory holding other files reports `cacheStatus()` `unavailable` and writes nothing there — and store-shaped data without the cache's `.apvm.lock` file is someone else's, whatever its `apvm.db` holds: every method rejects with `GenericFailure` ("not an apvm cache"), changing nothing.
- **Strict options**, because `clean()`'s default removes everything: a non-object or an object that is not plain (a class instance, a `Map`, `Object.create(defaults)` — options are read from own properties only, so inherited ones would be ignored), an unknown key (`dryrun`), a wrong type, or — for `clean()` — a key set to `undefined` / `null` rejects with `InvalidArg` instead of falling back to a default. Omit a key to leave it unset. `ApvmCache.open()` reads its config the same way (it throws), so `{ cacheDir: process.env.UNSET }` cannot silently open the default cache.
- **Validation first:** bad options reject with `InvalidArg` whatever the disk state, even on a corrupt cache.
- **Relative `cacheDir`:** made absolute when the handle (or the `Apvm`) is created, so a later `process.chdir()` changes nothing. `~` is not expanded.
- **Threading:** each call runs on the blocking pool with its own store, closed before the promise settles. At most one call per CPU runs at a time (the rest wait), so maintenance cannot starve builds. Calls are safe alongside builds on the same cache.
- **Snapshots:** `verify()` may report as `missing` an entry that a concurrent `clean()` or `clear()` removed meanwhile; `gc()` re-checks before deleting.
- **Interrupted repair** (the process exited mid-way): the cache is left needing repair — `CacheCorrupted`, and `gc()` refuses — never half-indexed. Run `repair()` again.
- **Repair never swaps the database file:** it keeps a copy and resets the file in place, so other processes using the cache cannot be crashed by it, and see the repaired index. A database file SQLite can no longer read is only reset once nobody has it open: while a build in this or another process holds it, `repair()` rejects with a `GenericFailure` ("in use…"), nothing changed; idle `Apvm` instances hold no handle.
- **One copy of the addon per process:** two copies (a duplicated dependency, a bundler copying the `.node` file) link two SQLite libraries whose file locks cannot see each other, which corrupts or crashes on a shared cache. The second copy's `Apvm.create()`, `Apvm.createWithTokenResolution()` and `ApvmCache.open()` throw `GenericFailure`; deduplicate the dependency. (A copy loaded only inside a worker thread is not detected, and where `globalThis` is frozen or sealed the guard cannot record anything and is skipped.)
- **Self-healing instances:** each build (and `cacheStatus()`) opens the cache afresh and closes it when done, so after a `repair()` from anywhere (this process, the CLI, another process), `Apvm` instances using that cache resume caching on their next build, and damage made meanwhile is reported. Nothing needs re-creating. `cacheStatus()` reflects whether the database *opens*: a database that opens but has unreadable rows reports `active` while the maintenance methods reject with `CacheCorrupted`.

**Error codes.** Every method reports errors by rejecting with an `Error` that has a `message` and a `code` (its `stack` has no JavaScript frames: it is created on the event loop when the work finishes). Only `ApvmCache.open()` is synchronous; it throws `InvalidArg`. An exception thrown by the options object itself while it is read (a getter, a `Proxy` trap) propagates synchronously, unchanged.

| `err.code` | When | Remedy |
|---|---|---|
| `CacheCorrupted` | The database is corrupt, has unreadable rows, or is missing / blank while cached builds remain (every method but `repair()`) | `await cache.repair()`, then retry |
| `InvalidArg` | Bad options (see *Strict options*), a bad `olderThan` (e.g. `30y`) or `project` (e.g. `Bad/Name`), or an empty cache path | Fix the input |
| `GenericFailure` | Anything else: I/O, someone else's data in the directory, a newer apvm's database. `repair()` never rejects with `CacheCorrupted`, so the recipe below cannot loop | Report it |

```ts
let usage;
try {
  usage = await cache.info();
} catch (e) {
  if ((e as { code?: string }).code !== 'CacheCorrupted') throw e;
  await cache.repair(); // keeps a copy, resets the database in place, re-indexes builds on disk
  usage = await cache.info();
}

const status = await apvm.cacheStatus(); // { state: 'active' | 'disabled' | 'corrupted' | 'unavailable', reason? }
```

While caching is on but the cache is `corrupted` or `unavailable`, every build still succeeds, uncached, and emits one `warning` progress event with the reason.

### Release Download Methods

These methods download pre-built assets directly from GitHub Releases, bypassing the clone → build pipeline entirely. The version is always derived from the release tag — there is no `version` parameter.

#### `apvm.downloadRelease(project, tag, outputDir, variants?, onProgress?): Promise<JsBuildOutput>`

Downloads assets from a specific release tag.

```ts
// Minimal
const output = await apvm.downloadRelease('backwpup', '5.6.8', '/tmp/output');

// With variant filter and progress
const output = await apvm.downloadRelease(
  'backwpup',
  '5.6.8',
  '/tmp/output',
  ['free', 'pro-en'],
  (err, event) => {
    if (err || !event) return;
    console.log(event.type, event.message);
  },
);
```

#### `apvm.downloadReleaseBySelector(project, selector, outputDir, variants?, onProgress?): Promise<JsBuildOutput>`

Downloads assets using a [`JsReleaseSelector`](#jsreleaseselector) keyword instead of a literal tag. Useful when you always want the latest or previous release without knowing the exact tag in advance.

```ts
import { Apvm, JsReleaseSelector } from 'apvm-napi';

// Latest stable release (non-prerelease, non-draft)
const output = await apvm.downloadReleaseBySelector(
  'backwpup',
  JsReleaseSelector.LatestStable,
  '/tmp/output',
);

// Previous stable release with progress
const output = await apvm.downloadReleaseBySelector(
  'backwpup',
  JsReleaseSelector.PreviousStable,
  '/tmp/output',
  undefined,
  (err, event) => {
    if (err || !event) return;
    if (event.type === 'phase_started') {
      console.log(`[${event.phase}] ${event.message}`);
    }
  },
);
```

### Supported `gitRef` Values

The `gitRef` field in `BuildOptions` supports all the same formats as the CLI:

**Automatic detection:**

| Input     | Resolved as                                                |
|-----------|------------------------------------------------------------|
| `"123"`   | PR #123 (or branch if PR doesn't exist)                    |
| `"develop"` | Branch name                                              |
| `"v1.0.0"` | Tag (if exists) or branch                                |
| `"abc1234"` | Commit SHA (7–40 hex characters)                         |
| `"5.6.8"` | GitHub Release (if plugin supports it), else tag/branch    |

**Explicit prefixes:** `"pr:123"`, `"branch:main"`, `"tag:v1.0.0"`, `"commit:abc1234"`, `"release:5.6.8"`

**Special keyword refs:**

| Keyword                      | Resolves to                                              |
|------------------------------|----------------------------------------------------------|
| `"tag:latest-stable"`        | Latest tag excluding `-alpha`, `-beta`, `-rc`            |
| `"tag:previous-stable"`      | Previous stable tag                                      |
| `"tag:latest"`               | Very latest tag (including prereleases)                   |
| `"tag:previous-latest"`      | Tag right before the latest                              |
| `"release:latest-stable"`    | Latest stable release (non-prerelease, non-draft)        |
| `"release:previous-stable"`  | Previous stable release                                  |
| `"release:latest"`           | Very latest non-draft release                             |
| `"release:previous-latest"`  | Previous non-draft release                               |

### Types

#### `ApvmConfig`

```ts
interface ApvmConfig {
  cacheDir?: string;      // Artifact cache base dir (default: ~/.apvm/cache)
  cacheEnabled?: boolean; // Whether the artifact cache is active (default: true)
  githubToken?: string;   // GitHub PAT (required for private repos)
}
```

> The `APVM_CACHE_DIR` environment variable, when set, overrides `cacheDir`
> (and the default) for every `Apvm` instance in the process. It is read
> from `process.env` when `create()` / `ApvmCache.open()` is called — in a
> worker thread, from that worker's own `process.env` — and a relative
> directory is made absolute then, so neither a later env change nor
> `process.chdir()` moves an existing instance's cache. Handy in tests to
> isolate the cache from the real `~/.apvm/cache` — this package's own test
> suite uses it (see `__tests__/setup.ts`).
>
> Unlike `ApvmCache.open()`, `Apvm.create()` reads its config leniently (as
> napi does): an unknown key such as `cachedir` is ignored and
> `cacheDir: undefined` means the default — so `apvm.cache()` then maintains
> `~/.apvm/cache`. Check `apvm.cache().dir()` before a destructive call when
> the config comes from untrusted or hand-written input.

#### `BuildOptions`

```ts
interface BuildOptions {
  project: string;        // "backwpup", "wp-rocket", or "imagify" currently
  gitRef: string;         // Any git ref (see above)
  version?: string;       // Required for BackWPup, optional and No-Op for WP Rocket / Imagify (Embedded version)
  variants?: string[];    // e.g., ["free", "pro-en"]. No-Op for WP Rocket / Imagify (No variants)
  outputDir: string;      // Absolute path for artifacts to be stored after build
  noCache?: boolean;      // Bypass the artifact cache for this build (default false)
}
```

#### `WarmOptions`

Passed to [`warmCache()`](#apvmwarmcacheoptions-onprogress-promisejsbuildoutput). Deliberately smaller than `BuildOptions`: no `outputDir` (nothing is delivered) and no `noCache` (warming _is_ a cache operation).

```ts
interface WarmOptions {
  project: string;        // "backwpup", "wp-rocket", or "imagify" currently
  gitRef: string;         // Any git ref (see above)
  version?: string;       // Required for BackWPup, optional for WP Rocket / Imagify (Embedded version)
  variants?: string[];    // e.g., ["free", "pro-en"]. No-Op for WP Rocket / Imagify (No variants)
}
```

#### `JsBuildOutput`

```ts
interface JsBuildOutput {
  result: JsBuildResult;        // Artifacts and version info
  resolvedRef: JsResolvedRef;   // Resolved git reference metadata
  commit: string;               // Full commit SHA
  commitShort: string;          // Short SHA (7 chars)
  branch: string;               // Checked-out branch name
  description: string;          // e.g., "PR #123 @ a1b2c3d"
  fromCache: boolean;           // true when every artifact came from the cache
  versionOverride?: JsVersionOverride; // set if the source version was rewritten
}
```

#### `JsBuildResult`

```ts
interface JsBuildResult {
  artifacts: JsProducedArtifact[];  // Built artifact files
  version: string;                  // Detected or provided version
}
```

#### `JsProducedArtifact`

```ts
interface JsProducedArtifact {
  variantId?: string; // Variant (e.g., "free", "pro-en"); undefined for single-output plugins
  path: string;       // Full path to the artifact file
  filename: string;   // File name only (e.g., "wp-rocket-3.17.4.zip")
  size: number;       // File size in bytes
  origin: string;     // "cache" | "built" | "downloaded"
}
```

#### `JsReleaseSelector`

String enum for selecting which GitHub Release to download. Used with [`downloadReleaseBySelector()`](#apvmdownloadreleaseselectorproject-selector-outputdir-variants-onprogress-promisejsbuildoutput).

> All string enums (`JsBuildPhase`, `JsCleanTarget`, `JsOutputStream`, `JsReleaseSelector`) are runtime enums, so members such as `JsReleaseSelector.Latest` work under TypeScript's `isolatedModules`. Their members are plain enumerable properties, like a compiled TypeScript enum, so `Object.values(JsReleaseSelector)` lists every value in declaration order.

```ts
export declare enum JsReleaseSelector {
  /** Latest non-prerelease, non-draft release (`releases/latest` endpoint). */
  LatestStable    = 'LatestStable',
  /** Second non-prerelease, non-draft release. */
  PreviousStable  = 'PreviousStable',
  /** Very latest non-draft release, including prereleases. */
  Latest          = 'Latest',
  /** Second non-draft release. */
  PreviousLatest  = 'PreviousLatest',
}
```

| Value              | GitHub API                                                                 |
|--------------------|----------------------------------------------------------------------------|
| `LatestStable`     | [`GET /releases/latest`](https://docs.github.com/en/rest/releases/releases#get-the-latest-release) |
| `PreviousStable`   | [`GET /releases`](https://docs.github.com/en/rest/releases/releases#list-releases) — second non-prerelease, non-draft |
| `Latest`           | [`GET /releases`](https://docs.github.com/en/rest/releases/releases#list-releases) — first non-draft |
| `PreviousLatest`   | [`GET /releases`](https://docs.github.com/en/rest/releases/releases#list-releases) — second non-draft |

#### Cache maintenance types

```ts
interface CleanOptions  { olderThan?: string; project?: string; dryRun?: boolean; target?: JsCleanTarget }
interface GcOptions     { checksum?: boolean } // also re-hash files (slowest)
interface VerifyOptions { checksum?: boolean }
declare enum JsCleanTarget { All = 'All', Builds = 'Builds', Releases = 'Releases' }
```

`olderThan` is an amount plus a unit: `m` minutes, `h` hours, `d` days, `w` weeks (`30d`, `12h`) — the CLI's `--older-than` grammar. Pass a member, e.g. `JsCleanTarget.Builds`.

Byte sizes and counts are `number`s; timestamps are ISO-8601 strings; fields that do not apply are absent.

| Type | Fields |
|---|---|
| `JsCacheUsage` | `cacheDir`, `exists`, `totalBytes`, `buildsBytes`, `releasesBytes`, `buildCount`, `releaseCount`, `fileCount`, `databaseBytes`, `oldestBuild?` / `newestBuild?` (when the oldest / newest build was *built* — `olderThan` goes by last use), `projects[]` (`project`, `buildCount`, `releaseCount`, `buildsBytes`, `releasesBytes`) |
| `JsCleanReport` | `buildsDeleted`, `releasesDeleted`, `bytesFreed`, `dryRun`, `failures: string[]` (directories that could not be removed; each line says whether `gc()` retries it or it must be removed by hand) |
| `JsGcReport` | `staleBuildRows`, `staleReleaseRows`, `orphanDirsRemoved`, `orphanBytesRemoved`, `staleTempFilesRemoved`, `damagedArtifacts`, `damagedBytesRemoved`, `failures: string[]` |
| `JsVerifyIssue` | `project`, `kind` (`'build'` → `version`, `commit`; `'release'` → `tag`), `filename`, `path`, `problem` (`'missing'`; `'size_mismatch'` → `expectedSize`, `actualSize`; `'checksum_mismatch'` → `expectedSha256`, `actualSha256`; `'unreadable'` → `details`, kept by `gc()`; `'invalid_record'` → `details`, removed by `gc()`) |
| `JsRepairReport` | `quarantinedDatabase?` (where the damaged database was kept), `rebuiltMissingDatabase` (it was missing or blank), `buildsAdopted`, `artifactsAdopted`, `entriesSkipped`, `orphanReleaseDirs` |
| `JsCacheStatus` | `state` (`'active'` \| `'disabled'` \| `'corrupted'` \| `'unavailable'`), `reason?` |

#### `JsBuildEvent` (Progress)

Events use a [discriminated union](https://www.typescriptlang.org/docs/handbook/2/narrowing.html#discriminated-unions) pattern:

```ts
interface JsBuildEvent {
  type: string;               // Event discriminator
  phase?: JsBuildPhase;       // For phase_started / phase_completed
  message?: string;           // For phase_started / warning
  step?: JsBuildStep;         // For step_started / step_completed
  stream?: JsOutputStream;    // For command_output ("stdout" | "stderr")
  line?: string;              // For command_output
  artifacts?: string[];       // For build_succeeded
  reason?: string;            // For build_failed
  resolvedRef?: JsResolvedRef; // For reference_resolved
}
```

**Event types:**

| `type`               | Description                             | Key fields         |
|----------------------|-----------------------------------------|--------------------|
| `reference_resolved` | Input ref resolved to a concrete source | `resolvedRef`      |
| `phase_started`      | Build phase began                       | `phase`, `message` |
| `phase_completed`    | Build phase finished                    | `phase`            |
| `step_started`       | Individual step started                 | `step`             |
| `step_completed`     | Individual step finished                | `step`             |
| `command_output`     | stdout/stderr line                      | `stream`, `line`   |
| `warning`            | Non-fatal warning                       | `message`          |
| `build_succeeded`    | Build completed successfully            | `artifacts`        |
| `build_failed`       | Build failed                            | `reason`           |

The `reference_resolved` event fires once, early — after the input ref is
resolved but before the clone/download — so consumers can display *what* is
being built (e.g. `event.resolvedRef.source.description` → `"branch 'develop'"`).

**Build phases** (`JsBuildPhase`):

`Preflight` → `ReleaseDownload` (if release) → `Clone` → `Checkout` → `DependencyCheck` → `PreBuild` → `Setup` → `Build` → `BuildHook` → `PostBuild` → `CollectArtifacts`

### Error Handling

All errors from the Rust core are converted to JavaScript `Error` objects via N-API status codes:

| Rust Error                        | N-API Status      | When                              |
|-----------------------------------|-------------------|-----------------------------------|
| `ProjectNotFound`                 | `InvalidArg`      | Unknown project name              |
| `PrivateRepoNoToken`             | `InvalidArg`      | Private repo without token        |
| `ReleaseNotFound`                | `InvalidArg`      | No release for given tag          |
| `NoMatchingReleaseAssets`        | `InvalidArg`      | No matching variant assets        |
| `ReleasesNotAvailable`           | `InvalidArg`      | Plugin doesn't support releases   |
| `Config`                          | `InvalidArg`      | Invalid configuration             |
| `GitHub`, `Git`, `Build`, `Io`   | `GenericFailure`  | API, git, build, or I/O errors    |

Errors are thrown as Promise rejections — they never crash the Node.js process. For the build methods, napi's own argument conversion is the exception: an argument of the wrong type, or an unknown enum value, throws synchronously. The cache maintenance methods read their arguments themselves and always reject, with their own codes (see [Cache Maintenance](#cache-maintenance)).

```ts
try {
  const output = await apvm.build({
    project: 'backwpup',
    gitRef: 'pr:999999',
    outputDir: '/tmp/output',
  });
} catch (err) {
  // err is a standard JavaScript Error
  console.error(err.message);
}
```

## Supported Projects

| Project     | Variants             | Version    | Releases |
|-------------|----------------------|------------|----------|
| `backwpup`  | free, pro-de, pro-en | Required   | Yes      |
| `wp-rocket` | (single)             | Optional   | No       |
| `imagify`   | (single)             | Optional   | No       |

## Full Example

```ts
import { Apvm } from 'apvm-napi';

async function main() {
  // Create with automatic token resolution
  const apvm = await Apvm.createWithTokenResolution();

  if (!apvm.hasToken()) {
    console.warn('No GitHub token found — private repos will fail');
  }

  console.log('Available projects:', apvm.listProjects());

  // Build with progress reporting
  const output = await apvm.build(
    {
      project: 'wp-rocket',
      gitRef: 'develop',
      outputDir: '/tmp/apvm-output',
    },
    (err, event) => {
      if (err || !event) return;
      switch (event.type) {
        case 'phase_started':
          console.log(`▶ [${event.phase}] ${event.message}`);
          break;
        case 'step_started':
          console.log(`  ▷ ${event.step?.label}`);
          break;
        case 'build_succeeded':
          console.log(`✔ Build succeeded: ${event.artifacts?.length} artifacts`);
          break;
        case 'build_failed':
          console.error(`✘ Build failed: ${event.reason}`);
          break;
      }
    },
  );

  console.log(`Built: ${output.description}`);
  console.log(`Version: ${output.result.version}`);
  console.log(`Commit: ${output.commitShort}`);

  for (const artifact of output.result.artifacts) {
    console.log(`  ${artifact.filename} (${artifact.size} bytes, sha256: ${artifact.sha256})`);
  }
}

main().catch(console.error);
```

## Architecture

```
┌────────────────┐     ┌──────────────┐     ┌──────────────┐
│  JS Consumer   │────▶│   JsApvm     │────▶│  apvm_core   │
│  (TypeScript)  │     │  (N-API)     │     │   ::Apvm     │
└────────────────┘     └──────────────┘     └──────────────┘
      Promises            bridges              Rust async
```

- **`apvm.rs`** — Main `Apvm` class exposed to JS. Wraps `apvm_core::Apvm` in `Arc` for safe concurrent access.
- **`config.rs`** — `ApvmConfig` JS object mapped to `apvm_config::Config`.
- **`types.rs`** — All JS-compatible types: `JsBuildOutput`, `JsBuildEvent`, `JsBuildPhase`, etc.
- **`progress.rs`** — Bridges `ProgressReporter` trait to a JS callback via `ThreadsafeFunction`.
- **`error.rs`** — Converts `apvm_core::Error` to `napi::Error` with appropriate status codes.

## Dependencies

| Crate                                        | Version   | Purpose                           |
|----------------------------------------------|-----------|-----------------------------------|
| [napi](https://docs.rs/napi/3)              | 3         | N-API bindings (async, napi8)     |
| [napi-derive](https://docs.rs/napi-derive/3)| 3         | Proc macros for `#[napi]`         |
| [napi-build](https://docs.rs/napi-build/2)  | 2         | Build script for `.node` output   |
| [tokio](https://docs.rs/tokio/1)            | 1         | Async runtime (multi-thread)      |
| [apvm-core](../core/README.md)              | workspace | Core build and git logic          |
| [apvm-config](../config/)                    | workspace | Configuration types               |
| [apvm-storage](../storage/)                  | workspace | Artifact storage                  |

## Building

```sh
# Release build (from repo root)
npm run build

# Debug build
npm run build:debug
```

## Running Tests

```sh
npm test
```

Tests use [Vitest](https://vitest.dev/) and cover instance creation, token methods, project listing, build error handling, and a full end-to-end build of WP Rocket.

For full TypeScript type definitions, see [`index.d.ts`](../../index.d.ts) in the repository root.
