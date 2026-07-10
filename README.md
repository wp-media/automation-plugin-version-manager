# APVM — Automation Plugin Version Manager

[![CI](https://github.com/wp-media/automation-plugin-version-manager/actions/workflows/ci.yml/badge.svg)](https://github.com/wp-media/automation-plugin-version-manager/actions/workflows/ci.yml)

A Rust CLI tool and library for building and managing multiple versions of WordPress plugins from any git reference (branch, tag, commit, PR, or GitHub Release).

Built for developers and QA engineers who need to quickly build plugins from specific PRs, switch between versions, and automate version management.

# Available commands

| Command     | Description                                          |
|-------------|------------------------------------------------------|
| `build`     | Build a plugin from a git reference (PR, branch, tag, commit, release) |
| `list`      | List all available plugins                           |
| `info`      | Show detailed information about a plugin             |
| `config`    | View or change configuration settings                |
| `update`    | Update apvm to the latest version                    |
| `uninstall` | Uninstall apvm from this system                      |

Run `apvm --help` for full usage or `apvm <command> --help` for command-specific options.

For extended CLI documentation, see the [CLI crate README](crates/cli/README.md).

## Features

- **Build from any git ref** — branch, tag, commit SHA, or PR number
- **Download from GitHub Releases** — skip the build entirely and download pre-built assets from a GitHub Release (e.g., `release:5.6.8`). Currently available for BackWPup
- **Special keyword refs** — use `tag:latest-stable`, `tag:latest`, `release:latest-stable`, `release:latest` (and their `previous-*` variants) to dynamically resolve the most recent tags or releases without knowing the exact version
- **Automatic ref detection** — `v1.0.0` resolves to a tag, `develop` to a branch, `123` to PR #123, and version-like inputs (e.g., `5.6.8`) are checked against GitHub Releases first (when the plugin supports releases)
- **Multi-variant builds** — e.g., BackWPup produces `free`, `pro-de`, and `pro-en` variants. You can choose which ones to build (`free` and `pro-en` are the defaults for BackWPup)
- **Persistent artifact cache** — builds and release downloads are cached (SQLite-indexed, on by default): rebuilding the same commit is served from the cache, and a partial request reuses cached variants and builds only the rest. Per-artifact provenance (`built`/`cache`/`downloaded`), `--no-cache` / `--strict-version` overrides, a `--warm-cache` mode that primes the cache without producing output, and an `apvm cache` maintenance command
- **Version handling** — required, embedded (auto-detected), or optional per plugin (BackWPup requires a version at build time; WP Rocket auto-detects it from source)
- **Self-update** — `apvm update` downloads and replaces the binary with the latest release, verifying SHA-256 checksums
- **Self-uninstall** — `apvm uninstall` cleanly removes the binary and its directory (with `-y`/`--yes` to skip confirmation)
- **GitHub token auto-resolution** — automatically finds tokens from config, `GITHUB_TOKEN`, `GH_TOKEN`, `gh auth token`, or the gh CLI config file

## Supported Plugins

| Plugin    | Variants             | Version  | Releases | Repository |
|-----------|----------------------|----------|----------|------------|
| BackWPup  | free, pro-de, pro-en | Required | Yes      | Private    |
| WP Rocket | (single)             | Embedded | No       | Public     |
| Imagify   | (single)             | Embedded | No¹      | Public     |

¹ Imagify publishes GitHub Releases (git tags), but they carry no downloadable
build assets — distribution goes to WordPress.org. Build a specific released
version from its tag instead, e.g. `apvm build imagify tag:v2.3.0`.

## Requirements

- [**Rust**](https://www.rust-lang.org/tools/install) ≥ 1.96.1 (2024 edition)
- **Git** installed and in `PATH`
- **GitHub authentication** for private repositories (optional for public). The tool auto-detects tokens from multiple sources (config file, environment variables, `gh` CLI)
- Plugin-specific build tools (npm, composer, gulp, rsync, zip, etc.) as needed (try `apvm info [plugin-name]` to learn more about a specific plugin)

## Installation

### Quick Install (recommended)

Install the latest pre-built binary with a single command — no Rust toolchain required.

If the tool was already installed, executing this again will attempt to update to latest stable version available.

**macOS / Linux:**

```sh
curl -fsSL https://raw.githubusercontent.com/wp-media/automation-plugin-version-manager/develop/install.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/wp-media/automation-plugin-version-manager/develop/install.ps1 | iex
```

The installer will:
- Detect your OS and architecture
- Download the correct binary from the [latest GitHub release](https://github.com/wp-media/automation-plugin-version-manager/releases/latest)
- Verify its SHA-256 checksum
- Install it to `~/.apvm/bin/` and add it to your `PATH`

To override the install directory, set `APVM_INSTALL` before running:

```sh
APVM_INSTALL=/opt/apvm curl -fsSL https://raw.githubusercontent.com/wp-media/automation-plugin-version-manager/develop/install.sh | sh
```

### Install from Source

If you have the Rust toolchain installed, you can build and install from source via [`cargo install`](https://doc.rust-lang.org/cargo/commands/cargo-install.html):

```sh
cargo install --path crates/cli
```

This compiles in release mode and places the `apvm` binary in `~/.cargo/bin/`, which should already be in your `PATH`.

#### Updating to new version (Easy way)

Just execute the same script for quick installation.

#### Updating from source (after code changes)

After pulling changes or switching branches, reinstall with `--force` to overwrite the existing binary:

```sh
cargo install --path crates/cli --force
```

The [`--force`](https://doc.rust-lang.org/cargo/commands/cargo-install.html#install-options) flag is required when reinstalling a package at the same version number.

### Development Build

For local development (unoptimized, faster compile):

```sh
cargo build
```

Binary at `target/debug/apvm`.

## Node.js / N-API Bindings (`apvm-napi`)

This repository includes a Node.js package powered by [`napi-rs`](https://napi.rs/), exposed as `apvm-napi`.

```ts
import { Apvm } from 'apvm-napi';

const apvm = await Apvm.create();
const output = await apvm.buildFromBranch('wp-rocket', 'develop', '/tmp/apvm-output');
console.log(output.result.artifacts.map((a) => a.filename));
```

For extended documentation about the Node.js bindings, see the [NAPI crate README](crates/napi/README.md).

## CLI Usage

### Build a Plugin

```sh
# Build from a branch
apvm build backwpup develop -v 5.1.0

# Build from a tag
apvm build backwpup v5.1.0 -v 5.1.0

# Build from a PR number
apvm build backwpup 123 -v 5.1.0

# Build from a commit SHA
apvm build backwpup abc1234 -v 5.1.0

# Explicit ref prefixes
apvm build backwpup tag:v5.1.0 -v 5.1.0
apvm build backwpup branch:develop -v 5.1.0
apvm build backwpup commit:abc1234 -v 5.1.0
apvm build backwpup pr:123 -v 5.1.0

# Download pre-built assets from a GitHub Release (no build required)
apvm build backwpup release:5.6.8

# Special keyword refs — latest/previous tags and releases
apvm build backwpup tag:latest-stable -v 5.1.0
apvm build backwpup tag:latest -v 5.1.0
apvm build backwpup release:latest-stable
apvm build backwpup release:latest

# Build specific variants only
apvm build backwpup 123 -v 5.1.0 --variants free,pro-en

# Specify output directory (default: current directory)
apvm build backwpup 123 -v 5.1.0 ./dist

# Verbose output (shows commands and full output)
apvm --verbose build backwpup 123 -v 5.1.0
```

For embedded-version plugins (WP Rocket, Imagify) the version is auto-detected
from source, so `-v` is unnecessary (any value passed is ignored) and the
artifact is named automatically (e.g. `imagify-<version>.zip`):

```sh
# Imagify — public repo, embedded version, single artifact
apvm build imagify develop            # build from the develop branch
apvm build imagify tag:v2.3.0         # build a specific released version from its tag
apvm build imagify pr:123             # build from a pull request
apvm build imagify develop ./dist     # choose an output directory
```

**Ref auto-detection rules:**

| Input       | Resolved as             |
|-------------|-------------------------|
| `develop`   | Branch                  |
| `v1.0.0`    | Tag (if exists), else branch  |
| `abc1234`   | Commit SHA (7–40 hex)   |
| `123`       | PR #123                 |
| `#123`      | PR #123 (`#` stripped)  |
| `5.6.8`     | GitHub Release (if the plugin has releases), else tag/branch |
| `release:5.6.8` | GitHub Release (explicit) |

**Special keyword refs:**

| Keyword                  | Resolves to                                              |
|--------------------------|----------------------------------------------------------|
| `tag:latest-stable`      | Latest tag excluding `-alpha`, `-beta`, `-rc` suffixes   |
| `tag:previous-stable`    | Previous stable tag                                      |
| `tag:latest`             | Very latest tag (including prereleases)                   |
| `tag:previous-latest`    | Tag right before the latest                              |
| `release:latest-stable`  | Latest stable GitHub Release (non-prerelease, non-draft) |
| `release:previous-stable`| Previous stable release                                  |
| `release:latest`         | Very latest non-draft release (including prereleases)     |
| `release:previous-latest`| Previous non-draft release                               |

Tags are sorted by creation date (most recent first). Releases are fetched from the [GitHub Releases API](https://docs.github.com/en/rest/releases/releases). Drafts are always excluded from release keywords.

### Warm the Cache

`--warm-cache` runs the **same pipeline** as a build — resolve the ref, reuse whatever is already cached, and build or download only what is missing — then stores everything into the cache. The one difference is that it produces **no output**: nothing is written to an output directory. Use it to prime the cache so a later build of the same reference is an instant hit.

```sh
# Prime the cache for a branch (no output directory is written)
apvm build backwpup develop -v 5.1.0 --warm-cache

# Warm specific variants
apvm build backwpup 123 -v 5.1.0 --variants free,pro-en --warm-cache

# Warm a release's pre-built assets into the cache
apvm build backwpup release:5.6.8 --warm-cache
```

The summary distinguishes what was already cached (`reused`) from what had to be `built` or `downloaded` to warm it — all of which are cached afterwards. `--warm-cache` cannot be combined with `--no-cache` (warming _is_ a cache operation — this is a hard error). An output directory and `--strict-version` are both no-ops with `--warm-cache` rather than errors: passing either is accepted and simply ignored, with a note printed to explain why.

### List Plugins

```sh
apvm list
```

### Plugin Details

```sh
apvm info backwpup
apvm info wp-rocket
```

Shows repository, version requirement, available variants, and required build tools.

### Configuration

```sh
# Show all settings
apvm config

# Get a specific value
apvm config get token

# Set GitHub token (required for private repos and PR builds)
apvm config set token ghp_xxxxxxxxxxxx

# Set custom cache directory
apvm config set cache-dir /path/to/cache

# Toggle the artifact cache (default: true)
apvm config set cache false

# Remove a value (revert to default)
apvm config unset token

# Show config file path
apvm config path
```

### Environment Variables

- **`APVM_CACHE_DIR`** — Overrides the artifact cache directory for this run.
  Highest precedence: wins over the config file's `cache-dir` and the default
  `~/.apvm/cache`. Honored by both the build commands and `apvm cache`.
- **`GITHUB_TOKEN`** / **`GH_TOKEN`** — GitHub token used when none is set in
  config (part of token auto-resolution).
- **`RUST_LOG`** — Enables logging output (`debug`, `trace`, …).

`APVM_CACHE_DIR` is handy for pointing a run at a throwaway cache so it doesn't
read from or warm your real `~/.apvm/cache` — for example in CI or when testing:

```sh
# Build against an ephemeral cache; your real cache is left untouched
APVM_CACHE_DIR="$(mktemp -d)" apvm build imagify develop ./out
```

It is honored by both the build commands and `apvm cache` (so maintenance
targets the same directory the build used).

### Cache Maintenance

```sh
# Usage totals and per-project breakdown
apvm cache info

# Remove cached entries (preview with --dry-run; scope by kind/project/age)
apvm cache clean --dry-run
apvm cache clean --older-than 30d --project backwpup

# Reconcile with disk, verify integrity, or recover a corrupt database
apvm cache gc
apvm cache verify --checksum
apvm cache repair

# Remove everything
apvm cache clear -y
```

See the [CLI crate README](crates/cli/README.md#cache-command) for the full
cache command reference.

### Update

```sh
apvm update
```

Downloads the latest release from GitHub, verifies the SHA-256 checksum, and atomically replaces the running binary. Supports macOS (arm64/x64), Linux (x64/arm64), and Windows (x64).

### Uninstall

```sh
# Interactive (asks for confirmation)
apvm uninstall

# Skip confirmation
apvm uninstall -y
```

Removes the `apvm` binary and its `bin/` directory. Does not remove configuration or build artifacts.

### GitHub Token Resolution

The CLI and NAPI bindings resolve a GitHub token from multiple sources, checked in order:

1. **Config file** — `apvm config set token ghp_xxx`
2. **`GITHUB_TOKEN`** environment variable
3. **`GH_TOKEN`** environment variable
4. **`gh auth token`** command ([gh CLI](https://cli.github.com/) ≥ 2.17.0)
5. **gh CLI config file** — `~/.config/gh/hosts.yml`

A token is **required** for private repositories (e.g., BackWPup) and for building from PR numbers. It is optional for public repositories but recommended for higher API rate limits (5,000 vs 60 requests/hour).

### Debug Logging

```sh
RUST_LOG=debug apvm build backwpup 123 -v 5.1.0
RUST_LOG=trace apvm build backwpup 123 -v 5.1.0
```

Tracing is only enabled when `RUST_LOG` is set. See the [`tracing-subscriber` EnvFilter docs](https://docs.rs/tracing-subscriber/0.3/tracing_subscriber/filter/struct.EnvFilter.html) for filter syntax.

## Project Structure

```
crates/
├── cli/        CLI binary (clap-based), owns defaults and user interaction
├── config/     Pure configuration types (no I/O, no defaults)
├── core/       Core library: building, git, GitHub API, version detection
├── napi/       Node.js N-API bindings (napi-rs cdylib)
└── storage/    SQLite-backed artifact cache (builds + release assets)
```

### Architecture

- **Config** — pure `serde` types for configuration. No file I/O, no hardcoded paths.
- **Core** — orchestrates builds (resolve ref → cache fast path → clone → checkout → detect version → build → collect artifacts), downloads from GitHub Releases, integrates the artifact cache (serve on hit, reuse partial variants, warm after builds), git operations, and GitHub API via [octocrab](https://docs.rs/octocrab/0.49). For extended documentation, see the [Core crate README](crates/core/README.md).
- **Storage** — the SQLite-backed artifact cache: one embedded database indexes builds (keyed by project/version/commit, per-variant artifacts) and release assets (keyed by tag/filename), with health-checked lookups, LRU-style aging, and maintenance operations (clean, gc, verify, repair). For extended documentation, see the [Storage crate README](crates/storage/README.md).
- **NAPI** — Node.js bindings via [napi-rs](https://napi.rs/). Exposes the core library as a native addon with async support on the tokio runtime. For extended documentation, see the [NAPI crate README](crates/napi/README.md).
- **CLI** — owns default paths (`~/.apvm`, `~/.apvm/cache`), provides progress display via [indicatif](https://docs.rs/indicatif/0.18), and delegates all logic to core. For extended documentation, see the [CLI crate README](crates/cli/README.md).

## Running Tests

```sh
# All tests
cargo test

# Specific crate
cargo test -p apvm-core
cargo test -p apvm-storage
cargo test -p apvm-config
cargo test -p apvm

# Node.js tests (Vitest)
npm test
```

## Key Dependencies

| Crate                | Version | Purpose                        |
|----------------------|---------|--------------------------------|
| [clap](https://docs.rs/clap/4)             | 4       | CLI argument parsing           |
| [tokio](https://docs.rs/tokio/1)           | 1       | Async runtime                  |
| [octocrab](https://docs.rs/octocrab/0.49)  | 0.49    | GitHub API client              |
| [reqwest](https://docs.rs/reqwest/0.13)    | 0.13    | HTTP client (release asset downloads) |
| [serde](https://docs.rs/serde/1)           | 1       | Serialization/deserialization  |
| [napi](https://docs.rs/napi/3)             | 3       | Node.js N-API bindings         |
| [indicatif](https://docs.rs/indicatif/0.18)| 0.18    | Progress bars and spinners     |
| [thiserror](https://docs.rs/thiserror/2)   | 2       | Error type derivation          |
| [tracing](https://docs.rs/tracing/0.1)     | 0.1     | Structured logging             |
| [chrono](https://docs.rs/chrono/0.4)       | 0.4     | Date/time for manifests        |
| [sha2](https://docs.rs/sha2/0.11)          | 0.11    | Artifact hashing               |
| [zip](https://docs.rs/zip/8)               | 8       | ZIP archive creation           |
| [which](https://docs.rs/which/8)           | 8       | Tool dependency checking       |
| [walkdir](https://docs.rs/walkdir/2)       | 2       | Recursive directory traversal  |
| [self-replace](https://docs.rs/self-replace/1) | 1   | Atomic binary self-replacement |
| [semver](https://docs.rs/semver/1)         | 1       | Semantic version parsing       |

## CI

The project uses GitHub Actions for continuous integration ([ci.yml](.github/workflows/ci.yml)).

| Job | Description |
|-----|-------------|
| **Check** | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo doc -D warnings` |
| **Build NAPI** | Builds native `.node` binaries for 5 targets (macOS ARM64/x64, Linux x64/ARM64, Windows x64) |
| **Test** | Runs `cargo test --workspace` + `npm test` on Linux, macOS, and Windows |
| **Commit Artifacts** | Auto-commits updated `.node` binaries on push to `develop` |
| **CI (gate)** | Single required status check for branch protection |

## License

MIT — see [package.json](package.json) and crate manifests.
