# APVM — Automation Plugin Version Manager

[![CI](https://github.com/wp-media/automation-plugin-version-manager/actions/workflows/ci.yml/badge.svg)](https://github.com/wp-media/automation-plugin-version-manager/actions/workflows/ci.yml)

A Rust CLI tool and library for building and managing multiple versions of WordPress plugins from any git reference (branch, tag, commit, PR, or GitHub Release).

Built for developers and QA engineers who need to quickly build plugins from specific PRs, switch between versions, and automate version management.

# Available commands

| Command  | Description                                          |
|----------|------------------------------------------------------|
| `build`  | Build a plugin from a git reference or download from a GitHub Release |
| `list`   | List all available plugins                           |
| `info`   | Show detailed information about a plugin             |
| `config` | View or change configuration settings                |

Run `apvm --help` for full usage or `apvm <command> --help` for command-specific options.

## Features

- **Build from any git ref** — branch, tag, commit SHA, or PR number
- **Download from GitHub Releases** — skip the build entirely and download pre-built assets from a GitHub Release (e.g., `release:5.6.8`). Currently available for BackWPup
- **Automatic ref detection** — `v1.0.0` resolves to a tag, `develop` to a branch, `123` to PR #123, and version-like inputs (e.g., `5.6.8`) are checked against GitHub Releases first (when the plugin supports releases)
- **Multi-variant builds** — e.g., BackWPup produces `free`, `pro-de`, and `pro-en` variants. You can choose which ones to build (`free` and `pro-en` are the defaults for BackWPup)
- **Version handling** — required, embedded (auto-detected), or optional per plugin (BackWPup requires a version at build time; WP Rocket auto-detects it from source)
- **GitHub token auto-resolution** — automatically finds tokens from config, `GITHUB_TOKEN`, `GH_TOKEN`, `gh auth token`, or the gh CLI config file

## Supported Plugins

| Plugin    | Variants             | Version  | Releases | Repository |
|-----------|----------------------|----------|----------|------------|
| BackWPup  | free, pro-de, pro-en | Required | Yes      | Private    |
| WP Rocket | (single)             | Embedded | No       | Public     |

## Requirements

- [**Rust**](https://www.rust-lang.org/tools/install) ≥ 1.94.1 (2024 edition)
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

## Node.js / N-API bindings (`apvm-napi`)

This repository now includes a Node.js package powered by [`napi-rs`](https://napi.rs/), exposed as `apvm-napi`.

### Node.js and build requirements — short summary

- **Runtime (using the package):** `apvm-napi` supports Node.js 18 and later at runtime. For platforms where a pre-compiled `.node` binary is provided, no native build is required on the consumer machine.
- **Build-time (only when a prebuilt binary is NOT available):** building the native addon requires a Rust toolchain (`cargo`, `rustc`) and Node.js 20+ for the build scripts. Some helper scripts used during the build process rely on Node 20+ APIs (for example certain `node:util` helpers). The resulting compiled addon can still run on Node.js 18+ provided the addon is built for a compatible N-API level.

In short: if your platform/arch matches a prebuilt binary included in this repository you only need Node.js 18+ at runtime; if no prebuilt binary exists for your platform you must build from source, which requires Node.js 20+ and the Rust toolchain.

Included pre-compiled binaries in this repository (these are shipped with the package and used automatically by the loader):

- `apvm-napi.darwin-arm64.node` — macOS (Apple Silicon / arm64)
- `apvm-napi.linux-x64-gnu.node` — Linux x86_64 (GNU/glibc)
- `apvm-napi.linux-arm64-gnu.node` — Linux arm64 (GNU/glibc)
- `apvm-napi.win32-x64-msvc.node` — Windows x64 (MSVC)

If your platform/arch is not listed above, the package install will attempt to compile the native addon locally.

### Build from source (only required if your platform is not prebuilt)

If a prebuilt `.node` binary for your platform is not present, the package's install step will compile the native addon locally. Building from source requires:

- Node.js 20+ (required during the build step only; runtime can be Node.js 18+)
- Rust toolchain (`rustup`, `cargo`, `rustc`) installed and available on `PATH`
- Platform development tools and headers (C compiler, linker). On Debian/Ubuntu, install `build-essential`, `pkg-config`, and `libssl-dev`.

To build locally (release):

```bash
# ensure Node 20+ is active for the build step
node -v # should be v20.x or later

# From repository root
npm i
npm run build
```

Notes:

- Using Node 20 for the build step does not force runtime Node 20 for users — the compiled addon can be used on Node 18+ when the addon is built against a compatible N-API level.

### Quick API usage (TypeScript/Node.js)

```ts
import { Apvm } from 'apvm-napi';

const apvm = await Apvm.create();

const output = await apvm.buildFromBranch('wp-rocket', 'develop', '/tmp/apvm-output');

console.log(output.description);
console.log(output.result.artifacts.map((a) => a.filename));
```

### API overview

- `Apvm.create(config?)` → creates an instance (async)
- `Apvm.createWithTokenResolution(config?)` → resolves token from config/env/gh CLI (async)
- `apvm.hasToken()` / `apvm.tokenSource()` / `apvm.listProjects()`
- Build methods:
	- `apvm.build(options, onProgress?)`
	- `apvm.buildFromPr(...)`
	- `apvm.buildFromBranch(...)`
	- `apvm.buildFromTag(...)`
	- `apvm.buildFromCommit(...)`

You can pass any git ref (branch, commit, tag, PR, or release) to `apvm.build()` via the `gitRef` field (e.g., `"pr:123"`, `"release:5.6.8"`, `"develop"`).

`ApvmConfig` fields are optional:

- `buildsDir?: string`
- `githubToken?: string`

For full signatures and event/result types, see `index.d.ts`.

### Why is `apvm.create()` async?

This rust project uses tokio runtime (for asynchronous operations in rust), and some dependencies heavily rely on tokio, so, to keep things simple, creating an instance is asynchronous.

## Usage

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

# Build specific variants only
apvm build backwpup 123 -v 5.1.0 --variants free,pro-en

# Specify output directory (default: current directory)
apvm build backwpup 123 -v 5.1.0 ./dist

# Verbose output (shows commands and full output)
apvm --verbose build backwpup 123 -v 5.1.0
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

### List Plugins

```sh
apvm list
```

### Plugin Details

```sh
apvm info backwpup
```

```sh
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

# Set custom builds directory
apvm config set builds-dir /path/to/builds

# Remove a value (revert to default)
apvm config unset token

# Show config file path
apvm config path
```

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
└── storage/    Artifact storage, deduplication, manifests, queries
```

### Architecture

- **Config** — pure `serde` types for configuration. No file I/O, no hardcoded paths.
- **Core** — orchestrates builds (clone → fetch → resolve ref → checkout → detect version → build → collect artifacts), downloads from GitHub Releases, git operations, and GitHub API via [octocrab](https://docs.rs/octocrab/0.49).
- **Storage** — manages artifacts with commit-based deduplication, cross-platform links (symlinks on Unix, junctions on Windows), and a fluent query API.
- **NAPI** — Node.js bindings via [napi-rs](https://napi.rs/). Exposes the core library as a native addon with async support on the tokio runtime.
- **CLI** — owns default paths (`~/.apvm`, `~/apvm-builds`), provides progress display via [indicatif](https://docs.rs/indicatif/0.18), and delegates all logic to core.

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
