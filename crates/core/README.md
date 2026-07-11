# APVM Core

The core library for the **Automation Plugin Version Manager**. This crate contains all the heavy logic: git operations, GitHub API integration, build orchestration, version detection, and release downloads. It is consumed by both the [CLI](../cli/README.md) and the [NAPI bindings](../napi/README.md).

Design principles:

- **Core builds artifacts** — handles git, builds, version detection
- **Caching is built in** — with `Config::cache_enabled` (the default), builds and release downloads are served from and stored to the [`apvm-storage`](../storage/) cache at `Config::cache_dir`; cache failures always degrade to a normal build, never an error
- **No default paths** — consumers supply the cache directory (the CLI and Node bindings default to `~/.apvm/cache`)

## Quick Start

```rust,ignore
use apvm_core::{Apvm, BuildRequest, NullReporter};
use apvm_config::Config;
use std::path::PathBuf;

// Create config with an explicit cache directory (caching on by default)
let config = Config::new(PathBuf::from("/var/lib/myapp/cache"));
let apvm = Apvm::new(config)?;

// BackWPup: version required, artifacts delivered to the output directory
let request = BuildRequest::new("backwpup", "pr:123", "/output")
    .version(Some("5.1.0".to_string()));
let output = apvm.build(request, &NullReporter).await?;

// WP Rocket: version auto-detected from source
let output = apvm
    .build(BuildRequest::new("wp-rocket", "pr:456", "/output"), &NullReporter)
    .await?;

// Provenance: was this served from the cache?
println!("from cache: {}", output.from_cache());
for artifact in &output.result.artifacts {
    println!("  {} ({})", artifact.filename, artifact.origin);
}
```

## Architecture

```
apvm-core/src/
├── lib.rs          Main Apvm struct (entry point)
├── error.rs        Error types (thiserror-based)
├── config_io.rs    Configuration file load/save helpers
├── build/          Build system
│   ├── context.rs      BuildContext (shared build state)
│   ├── runner.rs       BuildRunner (command execution)
│   ├── result.rs       BuildResult, ProducedArtifact
│   ├── progress.rs     ProgressReporter trait + implementations
│   ├── fs.rs           Filesystem helpers (zip, collect artifacts)
│   └── plugins/        Per-plugin builder implementations
│       ├── mod.rs          Builder trait, VersionRequirement
│       ├── bwu.rs          BackWPup builder
│       └── wpr.rs          WP Rocket builder
├── commands/       High-level command implementations
│   ├── build.rs        BuildCommand + BuildRequest (full build orchestration)
│   ├── cache.rs        Artifact-cache decisions (lookups, reuse, warming)
│   └── mod.rs          Re-exports
├── git/            Git operations
│   ├── repository.rs   Low-level git commands
│   ├── resolver.rs     RefResolver (ref → ResolvedRef), tag keywords, is_prerelease_tag
│   ├── workspace.rs    BuildWorkspace (temp dir management)
│   ├── token.rs        GitHub token resolution from multiple sources
│   └── mod.rs          Re-exports
├── github/         GitHub API integration
│   ├── client.rs       GitHubClient (octocrab wrapper)
│   ├── models.rs       PullRequest, Release, ReleaseAsset
│   └── mod.rs          Re-exports
└── projects/       Project management
    ├── registry.rs     ProjectRegistry, Project definitions
    └── mod.rs          Re-exports
```

## Main Entry Point: `Apvm`

The `Apvm` struct is the primary interface. It holds the config, GitHub client, project registry, and token source.

```rust,ignore
use apvm_core::Apvm;

// Synchronous constructor (uses only config token)
let apvm = Apvm::new(config)?;

// Async constructor (resolves token from env/gh CLI)
let apvm = Apvm::new_with_token_resolution(config).await?;

// Check token state
apvm.has_token();            // bool
apvm.token_source();         // Option<&str>
```

### `Apvm::download_release()`

Downloads pre-built assets from a GitHub Release, bypassing the clone → build pipeline entirely. The version is always derived from the release tag — there is no version parameter.

Accepts a [`ReleaseSelector`] value to specify either an exact tag or a dynamic keyword:

```rust,ignore
use apvm_core::{Apvm, ReleaseSelector, NullReporter};

// Specific release tag
let output = apvm
    .download_release("backwpup", ReleaseSelector::Tag("5.6.8"), None, "/tmp/output", &NullReporter)
    .await?;

// Latest stable release (non-prerelease, non-draft)
let output = apvm
    .download_release("backwpup", ReleaseSelector::LatestStable, None, "/tmp/output", &NullReporter)
    .await?;

// Specific variants only
let output = apvm
    .download_release("backwpup", ReleaseSelector::LatestStable, Some(&["free", "pro-en"]), "/tmp/output", &NullReporter)
    .await?;
```

### `ReleaseSelector`

Typed enum for selecting which GitHub Release to download. Defined at the crate root so it is available as `apvm_core::ReleaseSelector`.

| Variant          | Resolves to                                                           |
|------------------|-----------------------------------------------------------------------|
| `Tag(&str)`      | A specific release tag, e.g., `"5.6.8"`, `"v5.6.8"`                  |
| `LatestStable`   | Latest non-prerelease, non-draft release (`releases/latest` endpoint) |
| `PreviousStable` | Second non-prerelease, non-draft release                              |
| `Latest`         | Very latest non-draft release (includes prereleases)                  |
| `PreviousLatest` | Second non-draft release                                              |

The enum derives `Debug`, `Clone`, `Copy`, `PartialEq`, and `Eq`, and implements `Display` for human-readable descriptions.

Internally, `to_git_ref()` converts the selector to the `release:xxx` string consumed by `BuildCommand`. For example, `ReleaseSelector::LatestStable` becomes `"release:latest-stable"`.

References:
- [`GET /repos/{owner}/{repo}/releases/latest`](https://docs.github.com/en/rest/releases/releases#get-the-latest-release)
- [`GET /repos/{owner}/{repo}/releases`](https://docs.github.com/en/rest/releases/releases#list-releases)

### Token Resolution

When using `new_with_token_resolution`, the token is resolved in this order:

1. `config.github_token` (explicit)
2. `GITHUB_TOKEN` environment variable
3. `GH_TOKEN` environment variable
4. `gh auth token` command ([gh CLI](https://cli.github.com/) ≥ 2.17.0)
5. gh CLI config file (`~/.config/gh/hosts.yml`)

Implemented in `git::token::resolve_github_token()`.

## Build System

### Build Pipeline

The build pipeline is orchestrated by `BuildCommand` and follows these phases:

```
Preflight → Cache → Clone → Checkout → [Cache] → DependencyCheck → PreBuild → Setup → Build → BuildHook → PostBuild → CollectArtifacts
```

The first `Cache` phase is the pre-clone fast path: when the resolved commit
is fully cached, the pipeline stops there and delivers the cached artifacts.
The post-checkout `[Cache]` pass reuses whichever requested variants are
already cached at the authoritative version and builds only the rest.

For release downloads (e.g., `release:5.6.8`), the pipeline short-circuits to:

```
Preflight → Cache → ReleaseDownload
```

(`ReleaseDownload` is skipped entirely when every requested asset is cached.)

### `BuildContext`

Shared state for a build operation, carrying the plugin name, version, variants, output directory, and workspace reference.

### `BuildRunner`

Executes shell commands (npm, composer, gulp, etc.) via [`tokio::process::Command`](https://docs.rs/tokio/1/tokio/process/struct.Command.html), and build hooks, streams stdout/stderr to the progress reporter, and collects results.

### `BuildResult` and `ProducedArtifact`

```rust,ignore
pub struct BuildResult {
    pub artifacts: Vec<ProducedArtifact>,
    pub build_dir: PathBuf,
    pub version: String,
    pub variants_built: Vec<String>,
}

pub struct ProducedArtifact {
    pub variant_id: Option<String>, // None for single-output plugins
    pub path: PathBuf,
    pub filename: String,
    pub size: u64,
    pub origin: ArtifactOrigin,     // Cache | Built | Downloaded
}
```

`ArtifactOrigin` records where each delivered file came from, so consumers can
report provenance precisely — including partial builds where some variants
were reused from the cache and others were built fresh.

### Progress Reporting

The `ProgressReporter` trait allows consumers to receive build events:

```rust,ignore
pub trait ProgressReporter: Send + Sync {
    fn report(&self, event: &BuildEvent);
}
```

Built-in implementations:

- **`ClosureReporter`** — wraps an `Fn(&BuildEvent)` closure (used by CLI)
- **`NullReporter`** — discards all events (used in tests)

`BuildEvent` is an enum with variants for each phase/step lifecycle event, command output, warnings, and success/failure.

### Version Handling

The `VersionRequirement` enum controls how each plugin handles versions:

| Variant    | Behavior                                                         | Example        |
|------------|------------------------------------------------------------------|----------------|
| `Required` | Version **must** be provided via parameter                       | BackWPup       |
| `Embedded` | Version is auto-detected from source code; parameter is ignored  | WP Rocket      |
| `Optional` | Version can be provided to override, or auto-detected if omitted | (future use)   |

Version detection helpers:

- `detect_wordpress_plugin_version(path)` — parses `Version:` header from PHP plugin files
- `detect_wordpress_readme_version(path)` — parses `Stable tag:` from `readme.txt`

### Plugin Builders

Each plugin implements the `Builder` trait:

```rust,ignore
pub trait Builder: Send + Sync {
    fn version_requirement(&self) -> VersionRequirement;
    fn default_version(&self) -> Option<&str>;
    fn has_variants(&self) -> bool;
    fn variants(&self) -> Vec<Variant>;
    fn default_variants(&self) -> Vec<String>;
    // ... build hooks, dependency checks, etc.
}
```

Current implementations:

- **`BackWPupBuilder`** (`bwu.rs`) — multi-variant (free, pro-de, pro-en), version required, supports GitHub Releases
- **`WpRocketBuilder`** (`wpr.rs`) — single output, version embedded (auto-detected from PHP header)

## Git Module

### `Repository`

Low-level git command wrapper. Executes `git` via [`tokio::process::Command`](https://docs.rs/tokio/1/tokio/process/struct.Command.html).

Operations: clone, fetch, checkout, get commit SHA, list tags, list branches.

### `RefResolver`

Resolves a user-provided ref string (e.g., `"pr:123"`, `"develop"`, `"tag:latest-stable"`) into a concrete `ResolvedRef`:

```rust,ignore
pub struct ResolvedRef {
    pub source: RefSource,    // PR, Branch, Tag, Commit, Release
    pub branch: String,       // The branch/ref checked out
    pub commit: String,       // Full SHA
    pub commit_short: String, // 7-char SHA
    pub description: String,  // Human-readable (e.g., "PR #123 @ a1b2c3d")
}
```

### Tag Keywords

`resolve_tag_keyword()` handles dynamic tag resolution using [`git tag --sort=-creatordate`](https://git-scm.com/docs/git-for-each-ref#_field_names):

| Keyword            | Logic                                                    |
|--------------------|----------------------------------------------------------|
| `latest-stable`    | First tag not matching `is_prerelease_tag()`             |
| `previous-stable`  | Second tag not matching `is_prerelease_tag()`            |
| `latest`           | First tag in the sorted list (any kind)                  |
| `previous-latest`  | Second tag in the sorted list                            |

`is_prerelease_tag(tag)` returns `true` if the lowercased tag contains `-alpha`, `-beta`, or `-rc`.

### `BuildWorkspace`

Manages a temporary directory for build operations using [`tempfile::TempDir`](https://docs.rs/tempfile/3/tempfile/struct.TempDir.html). The directory is automatically cleaned up when dropped.

### Token Resolution

`resolve_github_token()` implements the 5-source token resolution chain (see above).

`is_valid_token_format()` validates known GitHub token prefixes (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`).

## GitHub Module

### `GitHubClient`

Wraps [octocrab](https://docs.rs/octocrab/0.54) with methods for:

| Method                        | Description                                          | API Endpoint |
|-------------------------------|------------------------------------------------------|-------------|
| `get_pull_request()`          | Fetch PR metadata                                    | [`GET /repos/{owner}/{repo}/pulls/{pull_number}`](https://docs.github.com/en/rest/pulls/pulls#get-a-pull-request) |
| `get_release_by_tag()`        | Fetch release by tag, returns `None` on 404          | [`GET /repos/{owner}/{repo}/releases/tags/{tag}`](https://docs.github.com/en/rest/releases/releases#get-a-release-by-tag-name) |
| `get_latest_stable_release()` | Latest non-prerelease, non-draft release             | [`GET /repos/{owner}/{repo}/releases/latest`](https://docs.github.com/en/rest/releases/releases#get-the-latest-release) |
| `get_previous_stable_release()` | Second stable release                              | [`GET /repos/{owner}/{repo}/releases`](https://docs.github.com/en/rest/releases/releases#list-releases) (filtered) |
| `get_latest_release_any()`    | Latest non-draft release (includes prereleases)      | List releases, filtered |
| `get_previous_release_any()`  | Previous non-draft release                           | List releases, filtered |

The shared `get_nth_release(owner, repo, index, stable_only)` method underpins the list-based methods: it fetches up to 30 releases via `list().per_page(30).send()`, filters out drafts (and optionally prereleases), then takes the item at `index`.

### Models

- **`PullRequest`** — number, title, base/head branches, URL
- **`Release`** — tag name, assets list, prerelease/draft flags
- **`ReleaseAsset`** — name, download URL, size

## Commands Module

### `BuildCommand` and `BuildRequest`

The high-level build orchestrator, driven by a `BuildRequest` (project, git
ref, output directory, optional version/variants, and the per-call cache
override `no_cache`). It:

1. Validates the project exists in the registry
2. Checks if the ref is a release keyword and resolves it via the GitHub API
3. Checks if the ref is a plain release tag and serves/downloads its assets (cache-aware)
4. Resolves the ref to a commit **before** cloning and tries the cache fast path. A hit requires the commit **and** version to match; when the version isn't known statically (a source-versioned plugin built without `--ver`), it is detected pre-clone by fetching just the plugin's version file at the commit (no clone), so a repeat build of the same commit still hits the cache
5. Falls through to git-based builds (clone → checkout → build → collect), reusing any cached variants and warming the cache afterwards

### `BuildOutput`

The return type of a successful build:

```rust,ignore
pub struct BuildOutput {
    pub result: BuildResult,                // Artifacts + version
    pub resolved_ref: ResolvedRef,          // Git ref metadata
    pub commit: String,                     // Full SHA
    pub commit_short: String,               // 7-char SHA
    pub branch: String,                     // Checked-out branch
    pub version_override: Option<VersionOverride>, // Set if the source version was rewritten
}
```

Helper methods: `description()` renders a human-readable summary (e.g.
`PR #123 @ a1b2c3d`), and `from_cache()` is `true` when every delivered
artifact came from the cache (a partial build is `false`).

## Configuration I/O

`config_io` provides load/save helpers for `apvm_config::Config`:

- `load_config(path)` — load from file, error if missing
- `load_config_or_default(path)` — load from file, return default if missing
- `load_config_file(path, default)` — load and merge with provided defaults
- `save_config_file(file, path)` — write to disk

The `apvm-config` crate itself is pure types with no I/O — this module provides the "batteries included" option.

## Error Handling

All errors use the `Error` enum derived with [thiserror](https://docs.rs/thiserror/2):

| Variant                    | Description                                   |
|----------------------------|-----------------------------------------------|
| `GitHub(octocrab::Error)`  | GitHub API errors (rate limit, auth, network)  |
| `Git(String)`              | Git command failures                           |
| `Build(String)`            | Build process failures                         |
| `Config(String)`           | Invalid configuration                          |
| `Project(String)`          | Project-level errors                           |
| `ProjectNotFound(String)`  | Unknown project name                           |
| `PrivateRepoNoToken`       | Private repo without authentication            |
| `ReleaseNotFound`          | No release for given tag                       |
| `ReleasesNotAvailable`     | Plugin doesn't support releases                |
| `NoMatchingReleaseAssets`  | No variant-matching assets in release          |
| `Io(io::Error)`            | Filesystem errors                              |
| `Json(serde_json::Error)`  | JSON parse errors                              |
| `Update(String)`           | Self-update errors                             |
| `Uninstall(String)`        | Self-uninstall errors                          |

All library code returns `Result<T>` (alias for `Result<T, Error>`). **No panics** — errors are always propagated.

## Dependencies

| Crate                                            | Version | Purpose                           |
|--------------------------------------------------|---------|-----------------------------------|
| [octocrab](https://docs.rs/octocrab/0.54)       | 0.54    | GitHub API client                 |
| [tokio](https://docs.rs/tokio/1)                | 1       | Async runtime                     |
| [reqwest](https://docs.rs/reqwest/0.13)         | 0.13    | HTTP downloads (native-tls)       |
| [serde](https://docs.rs/serde/1)                | 1       | Serialization                     |
| [serde_json](https://docs.rs/serde_json/1)      | 1       | JSON parsing                      |
| [thiserror](https://docs.rs/thiserror/2)        | 2       | Error type derivation             |
| [tracing](https://docs.rs/tracing/0.1)          | 0.1     | Structured logging                |
| [which](https://docs.rs/which/8)                | 8       | Tool dependency checking          |
| [glob](https://docs.rs/glob/0.3)                | 0.3     | File pattern matching             |
| [walkdir](https://docs.rs/walkdir/2)            | 2       | Recursive directory traversal     |
| [zip](https://docs.rs/zip/8)                    | 8       | ZIP archive creation (deflate)    |
| [tempfile](https://docs.rs/tempfile/3)           | 3       | Temporary build directories       |
| [directories](https://docs.rs/directories/6)    | 6       | Platform directory resolution     |
| [apvm-config](../config/)                        | workspace | Configuration types             |
| [apvm-storage](../storage/)                      | workspace | Artifact storage                |

## Running Tests

```sh
cargo test -p apvm-core
```
