---
name: apvm-cli
description: Build, inspect, and manage versions of the WP-Media WordPress plugins registered in apvm (BackWPup — `free`, `pro-de`, `pro-en` variants — WP Rocket, and Imagify) with the apvm CLI (Automation Plugin Version Manager). Covers `apvm build` (from a PR, branch, tag, commit, or GitHub Release), `apvm list` / `apvm info` (registered plugins), `apvm cache` (info / clean / gc / verify / repair / clear), `apvm config` (token, cache-dir, cache), `apvm skill` (install/uninstall this Claude Code skill), `apvm update`, and `apvm uninstall`. Use when the user mentions apvm, asks to build BackWPup / WP Rocket / Imagify from a PR/branch/tag/commit/release, asks about or invokes any apvm subcommand, or hits an apvm-related error (token, cache, build failure).
when_to_use: Trigger on requests like "build backwpup from PR 123", "what plugins does apvm know about", "clean the apvm cache", "set my GitHub token for apvm", "update apvm", "download the latest release of imagify", "install the apvm skill", or any direct `apvm <subcommand>` invocation. Also trigger when the user references a plugin artifact path under `~/.apvm/` or asks about the WP Media plugin version manager.
allowed-tools: Bash(apvm *)
---

# APVM CLI

`apvm` is the **Automation Plugin Version Manager**: a Rust CLI that builds
the WP-Media WordPress plugins it knows about (BackWPup with `free`, `pro-de`,
`pro-en` variants; WP Rocket; Imagify) from any of its supported git
references (PR, branch, tag, commit, GitHub Release) and manages a
SQLite-indexed artifact cache. This skill is the authoritative reference for
its command surface; treat the source-of-truth as `apvm <command> --help` and
`crates/cli/src/commands/*.rs`.

Binary path (after install): `~/.apvm/bin/apvm`. Version in this repo: **3.1.1**.

---

## Global conventions

- **`--verbose`** (also `--verbose` after the subcommand) — show full command
  output instead of the spinner. Applies to every subcommand.
- **`--version` / `-V`** — print version, supported on every subcommand.
- **`NO_COLOR=<anything>`** — disable ANSI colors (`https://no-color.org`).
  Colors are also auto-disabled when stdout is not a TTY.
- Exit codes: `0` success, `1` any error (verify exits 1 when issues found).
- `RUST_LOG=debug|trace` — enable tracing (`tracing-subscriber` `EnvFilter`).
- **`APVM_CACHE_DIR=<path>`** — override the cache directory for one
  invocation. Takes precedence over the config `cache-dir` and the default
  `~/.apvm/cache`. Honored by both `apvm build` and `apvm cache`.
  **Prefer the user's real cache — do not set this by default.** The user's
  configured cache (or `~/.apvm/cache`) is the intended target for normal use,
  so builds are reused across invocations. Only override `APVM_CACHE_DIR` when
  the user, a system prompt, or a `CLAUDE.md` explicitly says to use an isolated
  cache (e.g. developing/testing *apvm itself*, where a throwaway dir avoids
  warming the developer's real cache: `APVM_CACHE_DIR="$(mktemp -d)" apvm ...`).
- **Automatic update check** — every command except `update`/`uninstall`
  performs a **non-blocking, background** check for a newer release, at most
  **once per hour** (throttled via `~/.apvm/update-check.json`). It never
  updates anything; when a newer version is known it prints a boxed notice to
  **stderr** as the last output, suggesting `apvm update`. The check and notice
  are skipped when stderr is not a TTY (pipes/CI) and never change the exit
  code — only `apvm update` itself can fail.
- **`APVM_NO_UPDATE_CHECK=<non-empty>`** — disable the automatic update check
  and its notice entirely (e.g. `APVM_NO_UPDATE_CHECK=1`). `NO_COLOR` also
  applies to the notice.

### Default paths

| Path                       | Purpose                                     |
|----------------------------|---------------------------------------------|
| `~/.apvm/`                 | APVM home                                   |
| `~/.apvm/config.json`      | Configuration file                          |
| `~/.apvm/update-check.json`| Background update-notifier state (throttle) |
| `~/.apvm/cache/`           | Default artifact cache store (SQLite)       |

---

## Commands

### `apvm build <PLUGIN> <GIT_REF> [OUTPUT]`

Build a plugin from a git reference. This is the primary command.

| Argument    | Required | Default | Notes                                                 |
|-------------|----------|---------|-------------------------------------------------------|
| `<PLUGIN>`  | yes      | —       | Registered name (e.g., `backwpup`, `wp-rocket`, `imagify`) |
| `<GIT_REF>` | yes      | —       | PR, branch, tag, commit, or `release:` ref            |
| `[OUTPUT]`  | no       | `.`     | Output directory for build artifacts (ignored with `--warm-cache`, warned) |

| Option              | Description                                                                                          |
|---------------------|------------------------------------------------------------------------------------------------------|
| `-v`, `--ver <VER>` | Package version. Required for BackWPup; optional for WP Rocket / Imagify (auto-detected from source). Ignored with a warning when the plugin embeds its version. |
| `--variants <LIST>` | Comma-separated variants to build (BackWPup defaults: `free,pro-en`). Ignored when the plugin has no variants. |
| `--no-cache`        | Bypass the artifact cache for this build (still warms it unless `cache` is off in config).          |
| `--warm-cache`      | Prime the cache **without producing output** — same build, but no artifacts are written to `[OUTPUT]`. **Conflicts with `--no-cache`** (warming *is* caching). |

> **A normal build already caches.** You do **not** need `--warm-cache` to
> persist what you built — every successful `apvm build` writes its artifacts to
> the cache (unless `cache` is off in config). `--warm-cache` is *only* for the
> case where you want to populate the cache but do **not** want the built files
> delivered to an output directory (e.g. pre-warming CI, or seeding the cache
> for a later build). If you want the artifacts on disk, run a normal build.

After a build you get: `Build complete: ...`, `Commit: <short>`, `Version: ...`,
a per-artifact provenance line (`built` / `cache` / `downloaded`), and a
`Source:` summary (`all built` / `all from cache` / `1 built, 2 from cache` / ...).

**Git reference formats are extensive** — see
[references/git-refs.md](references/git-refs.md) for the full guide
(auto-detection, explicit prefixes, `tag:latest-stable`, `release:previous-latest`,
etc).

Examples:

```sh
# Branch build (BackWPup requires -v)
apvm build backwpup develop -v 5.1.0

# PR build
apvm build backwpup 123 -v 5.1.0
apvm build backwpup "#123" -v 5.1.0        # '#' is stripped

# Tag / commit
apvm build backwpup tag:v5.0.0 -v 5.1.0
apvm build backwpup abc1234 -v 5.1.0

# GitHub Release (downloads pre-built assets, no build tools required)
apvm build backwpup release:5.6.8
apvm build backwpup release:latest-stable

# Special keyword refs
apvm build backwpup tag:latest-stable -v 5.1.0
apvm build backwpup tag:previous-stable -v 5.1.0
apvm build imagify tag:latest-stable

# Embedded-version plugins (WP Rocket, Imagify): no -v needed
apvm build imagify develop
apvm build imagify tag:v2.3.0

# Specific variants / output dir / warm-cache
apvm build backwpup 123 -v 5.1.0 --variants free,pro-en
apvm build backwpup 123 -v 5.1.0 ./dist
apvm build backwpup 123 --warm-cache -v 5.1.0

# Verbose
apvm --verbose build backwpup 123 -v 5.1.0
```

---

### `apvm list`

Print registered plugins sorted by name:

```
Available plugins:

  backwpup       wp-media/backwpup-pro (private)
  imagify        wp-media/imagify-plugin (public)
  wp-rocket      wp-media/wp-rocket (public)
```

No arguments, no flags beyond `--verbose` / `-h` / `-V`.

---

### `apvm info <PLUGIN>`

Show repo, branch, visibility, version requirement, variants, and tool deps:

```
Plugin: backwpup
  Repository:  https://github.com/wp-media/backwpup-pro.git
  GitHub:      wp-media/backwpup-pro
  Branch:      develop
  Private:     yes (requires GITHUB_TOKEN)

Version:
  Requirement: Required (must be provided via --ver)
  Default:     9.99.99

Variants:
  free         Free           BackWPup Free version
  pro-de       Pro (German)   BackWPup Pro German version
  pro-en       Pro (English)  BackWPup Pro English version
  Default:     free, pro-en

Tool dependencies:
  npm          (required)
  composer     (required)
  gulp         (required, auto-install: npm install --global gulp-cli)
```

`Version` is one of `Required` / `Embedded` / `Optional`:

- **Required** — `--ver` is mandatory (BackWPup). Omitting it errors out.
- **Embedded** — version lives only in source; `--ver` is rejected with a warning.
- **Optional** — version is auto-detected from source; passing `--ver` rewrites
  it into the plugin's source so the built artifact actually carries it (WP Rocket, Imagify).

---

### `apvm cache <SUBCMD>`

Inspect and maintain the artifact cache. Works **even when caching is turned
off** (`apvm config set cache false`) — operates directly on the cache dir.

| Subcommand | Purpose                                                                                |
|------------|----------------------------------------------------------------------------------------|
| `info`     | Usage totals + per-project breakdown                                                   |
| `clean`    | Remove entries by age / project / kind (supports `--dry-run`)                          |
| `gc`       | Reconcile the SQLite DB with disk, sweep stale temp files                              |
| `verify`   | Check integrity (presence + size; `--checksum` re-hashes). **Exits non-zero on issues** so CI can gate on cache health |
| `repair`   | Recover a corrupt cache DB (quarantine it, rebuild the index from disk)                |
| `clear`    | Remove everything (prompts unless `-y`)                                                |

#### `apvm cache clean` flags

| Flag                       | Notes                                                                  |
|----------------------------|------------------------------------------------------------------------|
| `--older-than <DURATION>`  | `30d`, `12h`, `2w`, `45m` (units: `m`, `h`, `d`, `w`). Recently-used entries are kept — last-use is refreshed on every cache hit |
| `--project <NAME>`         | Restrict to one project                                                |
| `--dry-run`                | Report what would be removed, do not delete                            |
| `--builds` / `--releases`  | Scope to builds only / release downloads only (**mutually exclusive**) |

```sh
apvm cache info
apvm cache clean --dry-run
apvm cache clean --older-than 30d
apvm cache clean --project backwpup --builds
apvm cache verify
apvm cache verify --checksum
apvm cache gc
apvm cache repair
apvm cache clear -y
```

---

### `apvm config [SUBCMD]`

Manages `~/.apvm/config.json`. **No subcommand** prints all settings with
their source (`(default)` / `(from config file)`).

| Subcommand        | Description                                              |
|-------------------|----------------------------------------------------------|
| `get <KEY>`       | Print one value (or `<default> (default)` when unset)    |
| `set <KEY> <VAL>` | Set a value (sanitized before storage)                   |
| `unset <KEY>`     | Remove the override, revert to default                   |
| `path`            | Print the config file path (and whether it exists)        |

#### Config keys

| Key         | Type    | Default          | Notes                                                                                       |
|-------------|---------|------------------|---------------------------------------------------------------------------------------------|
| `token`     | string  | _(not set)_      | GitHub PAT. Sanitized; known prefixes accepted (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`). Unrecognized prefixes warn but are saved. |
| `cache-dir` | path    | `~/.apvm/cache`  | Absolute path, `~` expanded. Overridden by `APVM_CACHE_DIR`.                                |
| `cache`     | bool    | `true`           | Master switch. `true`/`false` (also `1`/`0`, `yes`/`no`, `on`/`off`, case-insensitive). Anything else rejected. |

#### Token sanitization on display

The CLI masks the token in `apvm config` output:

- `<=8 chars` → `***`
- Known prefix + `<8` secret → `ghp_***` (no suffix)
- Known prefix + `>=8` secret → `ghp_***...last4`
- No prefix + `>=8` chars → `***...last4`

```sh
apvm config                              # show all settings
apvm config path
apvm config get token
apvm config set token ghp_xxxxxxxxxxxx
apvm config set cache-dir /path/to/cache
apvm config set cache false
apvm config unset token
```

---

### `apvm skill <SUBCMD> [-g|--global]`

Installs or removes **this very skill** (`apvm-cli`) as a
[Claude Code skill](https://code.claude.com/docs/en/skills). Mostly for
humans setting up a machine or project — but run it if the user asks.

| Subcommand  | Effect on `./.claude/skills/apvm-cli/` (default) or `~/.claude/skills/apvm-cli/` (`-g`/`--global`) |
|-------------|------------------------------------------------------------------------------|
| `install`   | Write the skill there (directories created, existing install replaced)       |
| `uninstall` | Remove exactly that directory — parents and other skills are never touched   |

Behavior notes:

- The skill files are **embedded in the binary at compile time** — the
  command is fully self-contained (no network, works offline) and the
  installed skill always matches the binary version exactly.
- A project `install` outside a git repository prints a **warning** but still
  installs (informational only).
- `uninstall` succeeds (exit 0) when the skill is not installed — idempotent.
  It **refuses** (exit 1) to remove a path that is a symlink, a file, or a
  directory without a `SKILL.md`, telling you to delete it manually.

```sh
apvm skill install              # this project (./.claude/skills/apvm-cli)
apvm skill install -g           # all projects (~/.claude/skills/apvm-cli)
apvm skill uninstall            # remove from this project
apvm skill uninstall --global   # remove the all-projects installation
apvm --verbose skill install    # also list the embedded files
```

---

### `apvm update`

Self-update. Fetches the **latest GitHub Release** from
`wp-media/automation-plugin-version-manager`, compares versions with semver,
downloads the platform-appropriate binary, verifies its **SHA-256** against
`checksums.txt`, and atomically replaces the running binary via
`self-replace` (`rename(2)` on Unix, helper process on Windows).

Supported artifact names:

| OS      | Arch  | Artifact                          |
|---------|-------|-----------------------------------|
| macOS   | arm64 | `apvm-darwin-arm64`               |
| macOS   | x64   | `apvm-darwin-x64`                 |
| Linux   | x64   | `apvm-linux-x64-gnu`              |
| Linux   | arm64 | `apvm-linux-arm64-gnu`            |
| Windows | x64   | `apvm-win32-x64-msvc.exe`         |

For an unsupported `(os, arch)` pair, the command tells you to install from
source: `cargo install --path crates/cli`.

When the **global** Claude Code skill (`~/.claude/skills/apvm-cli/`) is
installed, a successful update also refreshes it (via the new binary's
`skill install --global`; non-fatal — a failure only warns). Project-local
copies are not touched: re-run `apvm skill install` inside each project.

A successful run also records the checked version and timestamp in
`~/.apvm/update-check.json`, keeping the background update notice (see
[Global conventions](#global-conventions)) in sync so it does not immediately
re-report. Every **other** command performs that same version check
automatically in the background (throttled to once per hour) and only reports —
it never self-updates.

---

### `apvm uninstall [-y]`

Removes the `apvm` binary and its containing `bin/` directory
(`~/.apvm/bin/`), plus the **global** Claude Code skill
(`~/.claude/skills/apvm-cli/`) when installed (non-fatal: a refusal —
symlink, stray file, no `SKILL.md` — only warns). Does **not** remove
`config.json`, the cache, or **project-local** skills
(`./.claude/skills/apvm-cli` in individual repos) — remove those per project
with `apvm skill uninstall` *before* uninstalling the binary.

- **Unix:** `unlink(2)` — kernel keeps the inode alive until the process exits.
- **Windows:** renames the `.exe` aside and spawns a helper with
  `FILE_FLAG_DELETE_ON_CLOSE`.

```sh
apvm uninstall          # prompts for confirmation
apvm uninstall -y       # skip confirmation
```

---

## GitHub token resolution order

Resolved in order on every build (cached after first hit):

1. `apvm config set token <PAT>`
2. `GITHUB_TOKEN` env var
3. `GH_TOKEN` env var
4. `gh auth token` (requires `gh` CLI ≥ 2.17.0)
5. `~/.config/gh/hosts.yml`

Required for private repos and PR builds. Recommended for public repos
(5,000 vs 60 req/hr).

---

## Plugin-specific quick reference

| Plugin     | `--ver`        | Variants             | Default variants | Notes                                                   |
|------------|---------------|----------------------|------------------|---------------------------------------------------------|
| `backwpup` | **Required**  | `free`, `pro-de`, `pro-en` | `free, pro-en`   | Private. Supports GitHub Release downloads (`release:`). |
| `wp-rocket`| Optional      | _(none)_             | _(all)_          | Public. `--ver` rewrites source version into the artifact. |
| `imagify`  | Optional      | _(none)_             | _(all)_          | Public. Imagify publishes git tags but no release assets — build from the tag instead of `release:`. |

---

## Common error patterns

| Symptom                                                          | Likely cause / fix                                                                |
|------------------------------------------------------------------|-----------------------------------------------------------------------------------|
| `Error: The cache database is corrupt — run 'apvm cache repair'` | Follow the hint: `apvm cache repair` quarantines the DB and rebuilds the index.   |
| `Error: failed to determine home directory`                      | `HOME` unset on Unix / `USERPROFILE` unset on Windows. Rare; set the env var.     |
| `Error: --ver required` (BackWPup)                               | Pass `-v 5.1.0` (or whatever version). See `apvm info backwpup` for the default. |
| `Warning: --ver ignored for '<plugin>'`                          | Plugin has `Embedded`/`Optional` version handling but you passed `--ver`. Harmless for `Optional` (it rewrites source); for `Embedded` it's rejected. |
| `Note: output directory '...' is ignored with --warm-cache`      | `--warm-cache` delivers nothing. Drop the `[OUTPUT]` arg, or use a normal build. |
| Build succeeds but artifact missing `Version: ...` line          | The variant was served from cache with a stale version; `apvm cache clean --project <name> --builds`. |

---

## Verification commands

```sh
apvm --version              # 3.1.1 (or current)
apvm list                   # registered plugins
apvm info <plugin>          # version requirement, variants, tool deps
apvm config path            # config file location
apvm cache info             # cache size + per-project breakdown
apvm cache verify           # integrity check (exits 1 on issues)
```

---

## See also

- [references/git-refs.md](references/git-refs.md) — full git reference format
  guide (auto-detection, explicit prefixes, `tag:` / `release:` keyword refs).
- Source of truth: `crates/cli/src/main.rs`, `crates/cli/src/commands/*.rs`,
  and `crates/cli/README.md` in this repo.
- Always trust `apvm <command> --help` over this file when they disagree —
  the binary ships the live version.