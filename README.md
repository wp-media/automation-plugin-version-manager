# APVM — Automation Plugin Version Manager

A Rust CLI tool and library for building and managing multiple versions of WordPress plugins from any git reference (branch, tag, commit, or PR).

Built for developers and QA engineers who need to quickly build plugins from specific PRs, switch between versions, and automate version management.

# Available commands

| Command  | Description                                          |
|----------|------------------------------------------------------|
| `build`  | Build a plugin from a git reference (branch, tag, commit, PR) |
| `list`   | List all available plugins                           |
| `info`   | Show detailed information about a plugin             |
| `config` | View or change configuration settings                |

Run `apvm --help` for full usage or `apvm <command> --help` for command-specific options.

## Features

- **Build from any git ref** — branch, tag, or commit SHA, PR number (PR number is WIP)
- **Automatic ref detection** — `v1.0.0` to a tag, `develop` to a branch, `123` resolves to PR #123 (when a GitHub token is provided or when this tool auto-detects one)
- **Multi-variant builds** — e.g., BackWPup produces `free`, `pro-de`, and `pro-en` variants. You can choose which ones you want to build (`free` and `pro-en` are defaults for BackWPup).
- **Version handling** — required, embedded (auto-detected), or optional per plugin (BackWPup requires version to be specified at build time, and WP Rocket does not require this)

## Supported Plugins

| Plugin    | Variants             | Version  | Repository |
|-----------|----------------------|----------|------------|
| BackWPup  | free, pro-de, pro-en | Required | Private    |
| WP Rocket | (single)             | Embedded | Public     |

## Requirements

- [**Rust**](https://www.rust-lang.org/tools/install) ≥ 1.85 (2024 edition)
- **Git** installed and in `PATH`
- **GitHub authentication** for private repositories (optional for public). This tool attempts to auto-detect.
- Plugin-specific build tools (npm, composer, gulp, rsync, zip, etc.) as needed (try `apvm info [plugin-name]` to learn more about a specific plugin)

## Installation

Install the `apvm` binary globally via [`cargo install`](https://doc.rust-lang.org/cargo/commands/cargo-install.html):

```sh
cargo install --path crates/cli
```

This compiles in release mode and places the `apvm` binary in `~/.cargo/bin/`, which should already be in your `PATH`.

### Updating After Changes

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

### Node.js requirements

- **Node.js >= 18** (enforced via `package.json` `engines.node`)
- **Rust toolchain** (`cargo`, `rustc`) available in `PATH` for native build
- Platform build prerequisites for Rust native modules

### Install and build (from source)

When installed from this repository, the package builds the native addon during install via the `prepare` script.

```sh
# From this repository
npm install

# Build native addon explicitly (release)
npm run build

# Run Node.js tests
npm test
```

Native outputs are generated at the project root:

- `index.js` (loader)
- `index.d.ts` (TypeScript declarations)
- `apvm-napi.<platform>.node` (native module)

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

You can pass any gitref (branch, commit or tag) to `apvm.build`

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
| `abc1234`   | Commit SHA (7-40 hex)   |
| `123`       | PR #123                 |
| `#123`      | PR #123 (`#` stripped)  |

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

# Set GitHub token (For building from PR numbers)
apvm config set token ghp_xxxxxxxxxxxx

# Set custom builds directory
apvm config set builds-dir /path/to/builds

# Remove a value (revert to default)
apvm config unset token

# Show config file path
apvm config path
```

NOTE: `builds-dir` is not used/implemented at the moment

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
└── storage/    Artifact storage, deduplication, manifests, queries (CURRENTLY NOT USED BY CLI)
```

### Architecture

- **Config** — pure `serde` types for configuration. No file I/O, no hardcoded paths.
- **Core** — orchestrates builds (clone → fetch → resolve ref → checkout → detect version → build → collect artifacts), git operations, GitHub API via [octocrab](https://docs.rs/octocrab/0.49), and project registry.
- **Storage** — manages artifacts with commit-based deduplication, cross-platform links (symlinks on Unix, junctions on Windows), and a fluent query API.
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
| [clap](https://docs.rs/clap/4.5)         | 4.5     | CLI argument parsing           |
| [tokio](https://docs.rs/tokio/1.49)       | 1.49    | Async runtime                  |
| [octocrab](https://docs.rs/octocrab/0.49) | 0.49    | GitHub API client              |
| [serde](https://docs.rs/serde/1.0)        | 1.0     | Serialization/deserialization  |
| [indicatif](https://docs.rs/indicatif/0.18)| 0.18   | Progress bars and spinners     |
| [thiserror](https://docs.rs/thiserror/2.0) | 2.0    | Error type derivation          |
| [tracing](https://docs.rs/tracing/0.1)     | 0.1     | Structured logging             |

## License

Private — WP Media.
