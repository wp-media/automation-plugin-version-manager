# APVM CLI

The command-line interface for the **Automation Plugin Version Manager**. Built with [Clap 4](https://docs.rs/clap/4) (derive API), it provides an ergonomic terminal experience with colored output, progress spinners via [indicatif](https://docs.rs/indicatif/0.18), and structured debug logging via [tracing](https://docs.rs/tracing/0.1).

This crate produces the `apvm` binary. All heavy logic is delegated to [`apvm-core`](../core/README.md) — the CLI owns only user interaction, default paths, input sanitization, and display.

## Commands

| Command     | Description                                                                 |
|-------------|-----------------------------------------------------------------------------|
| `build`     | Build a plugin from a git reference (PR, branch, tag, commit, release)      |
| `list`      | List all registered plugins                                                 |
| `info`      | Show detailed information about a plugin (repo, variants, version, tools)   |
| `cache`     | Inspect and maintain the artifact cache                                     |
| `config`    | View or change configuration settings                                       |
| `update`    | Download and install the latest APVM release                                |
| `uninstall` | Remove the APVM binary from this system                                     |

## Build Command

The `build` command is the primary command. It accepts a plugin name, a git reference, and optional flags.

```sh
apvm build <PLUGIN> <GIT_REF> [OUTPUT] [OPTIONS]
```

### Arguments

| Argument    | Required | Default | Description                              |
|-------------|----------|---------|------------------------------------------|
| `<PLUGIN>`  | Yes      | —       | Plugin name (e.g., `backwpup`)           |
| `<GIT_REF>` | Yes      | —       | Git reference (see below)                |
| `[OUTPUT]`  | No       | `.`     | Output directory for build artifacts     |

### Options

| Option              | Short | Description                                                                |
|---------------------|-------|----------------------------------------------------------------------------|
| `--ver <VERSION>`   | `-v`  | Package version (required for BackWPup)                                    |
| `--variants <LIST>` |       | Comma-separated variants (e.g., `free,pro-en`)                             |
| `--no-cache`        |       | Bypass the artifact cache for this build (still warms it)                  |
| `--verbose`         |       | Show full command output instead of spinner                                |

Each artifact's provenance is shown after a build (`built`, `cache`, or
`downloaded`), with a one-line `Source:` summary. Builds are served from the
cache when the resolved commit is already built; a partial request reuses the
cached variants and builds only the rest. `release:` downloads are cached too
(by tag): a repeated download is served from the cache, and requesting a new
asset downloads only the missing one. See [Config Command](#config-command)
for the `cache` / `cache-dir` settings.

### Git Reference Formats

The `<GIT_REF>` argument supports automatic detection or explicit prefixes:

**Automatic detection (no prefix):**

| Input     | Resolved as                                                |
|-----------|------------------------------------------------------------|
| `123`     | PR #123 (or branch if PR doesn't exist)                    |
| `#123`    | PR #123 (`#` stripped)                                     |
| `develop` | Branch name                                                |
| `v1.0.0`  | Tag (if exists) or branch                                  |
| `abc1234` | Commit SHA (7–40 hex characters)                           |
| `5.6.8`   | GitHub Release (if plugin supports releases), else tag/branch |

**Explicit prefixes:**

| Prefix             | Example              | Description                        |
|--------------------|----------------------|------------------------------------|
| `pr:`              | `pr:123`             | Force PR interpretation            |
| `branch:`          | `branch:main`        | Force branch interpretation        |
| `tag:`             | `tag:v1.0.0`         | Force tag interpretation           |
| `commit:`          | `commit:abc1234`     | Force commit interpretation        |
| `release:`         | `release:v5.6.8`     | Download from GitHub Release       |

### Special Keyword Refs

Instead of specifying an exact tag or release version, you can use keyword shortcuts that dynamically resolve to the latest or previous reference.

**Tag keywords** — tags are sorted by creation date ([`git tag --sort=-creatordate`](https://git-scm.com/docs/git-for-each-ref#_field_names)):

| Keyword                | Resolves to                                             |
|------------------------|---------------------------------------------------------|
| `tag:latest-stable`    | Latest tag excluding `-alpha`, `-beta`, `-rc` suffixes  |
| `tag:previous-stable`  | Previous stable tag                                     |
| `tag:latest`           | Very latest tag (including prereleases)                  |
| `tag:previous-latest`  | Tag right before the latest                             |

**Release keywords** — fetched from the [GitHub Releases API](https://docs.github.com/en/rest/releases/releases). Drafts are always excluded:

| Keyword                    | Resolves to                                              |
|----------------------------|----------------------------------------------------------|
| `release:latest-stable`    | Latest stable release (non-prerelease, non-draft)        |
| `release:previous-stable`  | Previous stable release                                  |
| `release:latest`           | Very latest non-draft release (including prereleases)     |
| `release:previous-latest`  | Previous non-draft release                               |

### Examples

```sh
# Build from a branch
apvm build backwpup develop -v 5.1.0

# Build from a PR
apvm build backwpup pr:123 -v 5.1.0

# Download from a specific GitHub Release
apvm build backwpup release:5.6.8

# Build from the latest stable tag
apvm build backwpup tag:latest-stable -v 5.1.0

# Download the latest stable release
apvm build backwpup release:latest-stable

# Build only specific variants
apvm build backwpup 123 -v 5.1.0 --variants free,pro-en

# Output to a specific directory
apvm build backwpup 123 -v 5.1.0 ./dist

# Verbose output
apvm --verbose build backwpup 123 -v 5.1.0

# Embedded-version plugins (WP Rocket, Imagify): no -v needed, version is
# auto-detected from source and the artifact is named imagify-<version>.zip
apvm build imagify develop
apvm build imagify tag:v2.3.0
```

## List Command

```sh
apvm list
```

Displays all registered plugins with their repository and visibility (public/private).

## Info Command

```sh
apvm info <PLUGIN>
```

Shows detailed information about a plugin:

- Repository URL and GitHub owner/repo
- Default branch
- Whether it requires authentication (private)
- Version requirement (`Required`, `Embedded`, or `Optional`)
- Default version (if any)
- Available variants with descriptions
- Required build tools (e.g., npm, composer, gulp, rsync, zip)

Example:

```sh
apvm info backwpup
apvm info wp-rocket
apvm info imagify
```

## Cache Command

Inspect and maintain the artifact cache (the `cache-dir`, default `~/.apvm/cache`).
These commands operate on the cache directly and work even when caching is
turned off (`cache false`).

```sh
# Usage totals and per-project breakdown
apvm cache info

# Remove cached entries; scope by kind, project, or age
apvm cache clean --dry-run                 # preview what would be removed
apvm cache clean --older-than 30d          # not used in the last 30 days
apvm cache clean --project backwpup        # one project
apvm cache clean --builds                  # builds only (keep release downloads)
apvm cache clean --releases                # release downloads only

# Reconcile the database with disk and sweep leftover temp files
apvm cache gc

# Verify cached files are intact (add --checksum to re-hash them).
# Exits non-zero when issues are found, so CI can gate on cache health.
apvm cache verify
apvm cache verify --checksum

# Recover a corrupt cache database (quarantine + rebuild the index)
apvm cache repair

# Remove everything (prompts unless -y)
apvm cache clear
apvm cache clear -y
```

**`--older-than`** accepts `m` (minutes), `h` (hours), `d` (days), `w` (weeks),
e.g. `12h`, `30d`, `2w`. An entry's "last used" time is refreshed on every cache
hit, so entries you keep building never age out. `--builds` and `--releases`
are mutually exclusive (omit both to clean everything).

## Config Command

Manages the APVM configuration file (`~/.apvm/config.json`).

```sh
# Show all current settings
apvm config

# Get a specific value
apvm config get token
apvm config get cache-dir
apvm config get cache

# Set a value
apvm config set token ghp_xxxxxxxxxxxx
apvm config set cache-dir /path/to/cache
apvm config set cache false

# Remove a value (revert to default)
apvm config unset token

# Show config file path
apvm config path
```

**Available keys:**

| Key         | Description                                                                             |
|-------------|-----------------------------------------------------------------------------------------|
| `token`     | GitHub Personal Access Token                                                            |
| `cache-dir` | Base directory of the artifact cache                                                    |
| `cache`     | Whether the artifact cache is active — `true`/`false` (default `true`)                  |

Input values are sanitized before storage:

- **Token**: trimmed, validated against known GitHub token prefixes (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`). Unrecognized prefixes produce a warning but are still saved.
- **Cache**: accepts `true`/`false` (also `1`/`0`, `yes`/`no`, `on`/`off`, case-insensitive), normalized to `true`/`false`. Any other value is rejected.
- **Path**: expanded (`~` resolved), validated as absolute.

### Environment Variables

- **`APVM_CACHE_DIR`** — Overrides the artifact cache directory for the current invocation. Takes precedence over the config `cache-dir` and the default `~/.apvm/cache`, and is honored by both builds and the `cache` command. Useful for CI or local testing: point it at a throwaway directory so a run never reads from or warms your real cache.
- **`GITHUB_TOKEN`** / **`GH_TOKEN`** — Fallback GitHub token when none is in config.
- **`RUST_LOG`** — Enables logging (`debug`, `trace`, …); implies verbose diagnostics.

```sh
# Build against an ephemeral cache; your real cache is left untouched
APVM_CACHE_DIR="$(mktemp -d)" apvm build imagify develop ./out
```

## Update Command

```sh
apvm update
```

Self-updates the `apvm` binary:

1. Fetches the [latest GitHub Release](https://docs.github.com/en/rest/releases/releases#get-the-latest-release) for this repository
2. Compares the release version against the current binary version using [semver](https://docs.rs/semver/1)
3. Downloads the platform-appropriate binary
4. Verifies its SHA-256 checksum against `checksums.txt`
5. Atomically replaces the running binary via [self-replace](https://docs.rs/self-replace/1)

**Supported platforms:**

| OS      | Architecture | Artifact name              |
|---------|--------------|----------------------------|
| macOS   | arm64        | `apvm-darwin-arm64`        |
| macOS   | x64          | `apvm-darwin-x64`          |
| Linux   | x64          | `apvm-linux-x64-gnu`       |
| Linux   | arm64        | `apvm-linux-arm64-gnu`     |
| Windows | x64          | `apvm-win32-x64-msvc.exe`  |

## Uninstall Command

```sh
# Interactive (prompts for confirmation)
apvm uninstall

# Skip confirmation
apvm uninstall -y
apvm uninstall --yes
```

Removes the `apvm` binary and its containing `bin/` directory. Uses [`self_replace::self_delete_outside_path`](https://docs.rs/self-replace/1/self_replace/fn.self_delete_outside_path.html) for cross-platform binary deletion:

- **Unix**: `unlink(2)` — the kernel keeps the inode alive until the process exits.
- **Windows**: Renames the `.exe` aside, spawns a helper process with `FILE_FLAG_DELETE_ON_CLOSE`.

Does **not** remove configuration files (`~/.apvm/config.json`) or the artifact cache (`~/.apvm/cache/`).

## GitHub Token Resolution

The CLI resolves a GitHub token from multiple sources, checked in order:

1. **Config file** — `apvm config set token ghp_xxx`
2. **`GITHUB_TOKEN`** environment variable
3. **`GH_TOKEN`** environment variable
4. **`gh auth token`** command ([gh CLI](https://cli.github.com/) ≥ 2.17.0)
5. **gh CLI config file** — `~/.config/gh/hosts.yml`

A token is **required** for private repositories and PR builds. It is optional for public repositories but recommended for higher API rate limits (5,000 vs 60 requests/hour).

## Default Paths

| Path                    | Purpose                                       |
|-------------------------|-----------------------------------------------|
| `~/.apvm/`              | APVM home directory                           |
| `~/.apvm/config.json`   | Configuration file                            |
| `~/.apvm/cache/`        | Default artifact cache store                  |

These defaults are determined using the [`directories`](https://docs.rs/directories/6) crate for cross-platform home directory resolution.

## Debug Logging

```sh
RUST_LOG=debug apvm build backwpup 123 -v 5.1.0
RUST_LOG=trace apvm build backwpup 123 -v 5.1.0
```

Tracing is only initialized when the `RUST_LOG` environment variable is set. The CLI uses [`tracing-subscriber`](https://docs.rs/tracing-subscriber/0.3) with the [`EnvFilter`](https://docs.rs/tracing-subscriber/0.3/tracing_subscriber/filter/struct.EnvFilter.html) for filter syntax.

## Installation

### Quick Install (recommended)

**macOS / Linux:**

```sh
curl -fsSL https://raw.githubusercontent.com/wp-media/automation-plugin-version-manager/develop/install.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/wp-media/automation-plugin-version-manager/develop/install.ps1 | iex
```

### From Source

```sh
cargo install --path crates/cli
```

### Development Build

```sh
cargo build
# Binary at target/debug/apvm
```

## Dependencies

| Crate                                              | Version | Purpose                           |
|----------------------------------------------------|---------|-----------------------------------|
| [clap](https://docs.rs/clap/4)                    | 4       | CLI argument parsing (derive API) |
| [tokio](https://docs.rs/tokio/1)                  | 1       | Async runtime (`current_thread`)  |
| [indicatif](https://docs.rs/indicatif/0.18)       | 0.18    | Progress bars and spinners        |
| [tracing](https://docs.rs/tracing/0.1)            | 0.1     | Structured logging                |
| [tracing-subscriber](https://docs.rs/tracing-subscriber/0.3) | 0.3 | Log formatting and filtering |
| [semver](https://docs.rs/semver/1)                | 1       | Version comparison for updates    |
| [self-replace](https://docs.rs/self-replace/1)    | 1       | Atomic binary self-replacement    |
| [reqwest](https://docs.rs/reqwest/0.13)           | 0.13    | HTTP downloads (native-tls)       |
| [sha2](https://docs.rs/sha2/0.11)                 | 0.11    | SHA-256 checksum verification     |
| [directories](https://docs.rs/directories/6)      | 6       | Platform home directory           |
| [tempfile](https://docs.rs/tempfile/3)             | 3       | Staging downloaded binaries       |
| [apvm-core](../core/README.md)                    | workspace | Core build and git logic        |
| [apvm-config](../config/)                          | workspace | Configuration types             |
| [apvm-storage](../storage/)                        | workspace | Artifact storage                |

## Running Tests

```sh
cargo test -p apvm
```
