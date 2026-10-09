# Site deployment — install & manage apvm-built plugins on WordPress sites

Status: **planned** · Target version: **3.3.0** (from 3.2.0) · Author: Sandy Figueroa

---

## TL;DR

| | |
|---|---|
| **Goal** | Put an apvm-built artifact onto a real WordPress site over WP-CLI, optionally activate it, and manage its lifecycle there. Makes the "Automation" in APVM real. |
| **Registry** | `~/.apvm/sites.json` — sibling of `config.json`, atomic write, `0600`, secrets masked everywhere. |
| **Transports** | **SSH first** (system `ssh`/`scp`), then **local**, then **docker** — the last two are the final two phases. |
| **New commands** | `apvm site add\|list\|show\|test\|remove` and `apvm plugin list\|status\|install\|activate\|deactivate\|uninstall\|delete`. Grammar matches the existing `cache`/`config`/`skill` noun groups. |
| **Core primitive** | `apvm_core::wp::SiteClient` — site-only, needs no GitHub client and no cache. |
| **Composed** | `Apvm::install_plugin()` = existing cache-aware `BuildCommand` (cache hit / build / release download) → upload → install → verify → optional activate. |
| **"Is it installed?"** | Both: `plugin_state()` is public so callers can check *before* any work, **and** `install_artifact()` takes an `ExistingPluginPolicy` (`fail` default \| `reinstall` \| `overwrite` \| `skip`) that it re-checks and enforces itself. No destructive default; no confirmation callback crosses FFI. |
| **Removal** | Two passes: deactivate **every** target, verify, *then* uninstall each. Required for correctness — see §3. Never `uninstall --deactivate`, never `deactivate --uninstall`. |
| **Variants** | `apvm plugin uninstall backwpup` removes **all** its variants (`backwpup` + `backwpup-pro`); `--variants` narrows. Install stays single-variant. |
| **Slug** | Authoritative from the artifact zip's root directory; declared per variant on `Builder` for the pre-flight check; mismatch → warning, zip wins. |
| **Reliability** | Exit codes are a secondary signal only. Reads parse `--format=json`; every mutation is confirmed by re-reading state. |
| **New deps** | `async-ssh2-tokio` only (Phase 5), declared in the root `Cargo.toml`. |
| **Phases** | 1 registry → 2 SSH + probe → 3 read/lifecycle → 4 install → 5 SSH passwords → **6 local** → **7 docker**. |

---

## 1. Scope and decisions

**In scope:** the three plugins apvm builds — BackWPup (`free`, `pro-de`, `pro-en`), WP Rocket, Imagify. The plugin argument is always a registry name, so **no free-text slug enters the current plan** and the plugin argument carries no injection surface at all.

**Out of scope, planned as enhancements (§10):** managing arbitrary non-apvm plugins on a site, and installing an arbitrary local zip. Both carry a security assessment there.

| Decision | Choice | Rationale |
|---|---|---|
| SSH transport | Hybrid: system `ssh`/`scp` for key/agent/`ssh_config`; native library for passwords | Inherits `Host` aliases, ProxyJump, agent, `known_hosts` for free |
| Secrets | Env-var reference **and** plaintext value | Plaintext file is `0600` + masked in every output path |
| Grammar | `apvm <noun> <verb>` | Matches `cache clean`, `config set`, `skill install` |
| Already installed, user declines | Abort; `--force` = `wp plugin install --force` (files replaced, data kept, no hooks) | "No" must never mutate the site |
| Registry | Single `~/.apvm/sites.json` | One atomic write, one file to protect, sibling of `config.json` |
| `site add` | Probes by default; `--no-test` skips; "save anyway? [y/N]" on failure | Catches a wrong path/host at registration, not mid-deploy |

**Alternative noted, not chosen:** password auth could instead reuse the system-`ssh` path via `SSH_ASKPASS` + `SSH_ASKPASS_REQUIRE=force` (present in OpenSSH ≥ 8.4; local `ssh` is 10.3) with `apvm` as the askpass helper — no second SSH stack, no new crate. Phase 5 keeps the password path behind one seam, so switching is a local change.

**WP-CLI's own `--ssh=` is deliberately unused:** it requires WP-CLI installed *locally* purely to proxy, and still cannot transfer a local zip. apvm requires `wp` only on the target.

---

## 2. Verified ground truth

Everything below was checked, not assumed.

**Installed directory (= WP-CLI plugin `name`)** — read from the zip roots of real artifacts in `~/.apvm/cache`:

| Plugin | Variant | Directory |
|---|---|---|
| backwpup | `free` | `backwpup` |
| backwpup | `pro-de` **and** `pro-en` | `backwpup-pro` — one slug, they replace each other |
| wp-rocket | — | `wp-rocket` |
| imagify | — | `imagify` |

**The uninstall guard is cross-variant.** `backwpup/uninstall.php` and `backwpup-pro/uninstall.php` (5.7.4) are identical on this point:

```php
if ( ! defined( 'WP_UNINSTALL_PLUGIN' ) ) { exit(); }
// Only uninstall if no BackWPup Version active.
if ( ! class_exists( \BackWPup::class ) ) {   // ← data cleanup lives inside this block
```

Both variants declare `BACKWPUP_PLUGIN_LOADED`, text domain `backwpup`, and the same `\BackWPup` class (headers differ only by `Plugin Name: BackWPup` vs `BackWPup Pro`). So **any** active BackWPup blocks **any** variant's cleanup. This is what forces the two-pass removal in §3.

**WP-CLI behaviour** (developer.wordpress.org/cli):

| Command | Verified fact |
|---|---|
| `plugin uninstall` | "warn and skip if the plugin is active"; flags `--deactivate`, `--skip-delete`, `--all`, `--exclude` |
| `plugin delete` | "Deletes plugin files without deactivating or uninstalling" — no hooks, data kept |
| `plugin deactivate` | flags `--network`, `--uninstall`, `--all`, `--exclude` |
| `plugin activate` | flags `--network`, `--all`, `--exclude` |
| `plugin list` | `--format=json`, `--skip-update-check`; optional fields include **`file`**, `title`, `description`; statuses `active`, `active-network`, `inactive`, `must-use`, `dropin` |
| `core is-installed` | "Doesn't produce output; uses exit codes"; `--network` tests multisite |
| Global params | `--path`, `--url`, `--skip-plugins`, `--skip-themes`, `--require`, `--quiet`, `--debug`, `--[no-]color`, `--ssh=[<scheme>:]…` (`ssh`/`docker`/`docker-compose`/`vagrant`) |

**Crates** (crates.io): `async-ssh2-tokio` 0.13.0 (`AuthMethod::with_password` / `with_key_file` / `with_key` / `with_agent`, `ServerCheckMethod`), `russh` 0.62.5, `russh-sftp` 2.4.0, `ssh2` 0.9.6.

**Codebase precedents reused, not reinvented:** `is_safe_version`/`ensure_safe_version` (allowlist + `escape_debug` on rejected input), `ConfigKey::is_sensitive` masking, `confirm()` in `cache.rs`/`uninstall.rs`, `ProgressReporter`/`ClosureReporter`/`NullReporter`, `config_io` load/save shape, `Paths`, enum dispatch as in `RefSource`, `variants_ignored`/`version_ignored` warnings in `cli/commands/build.rs`, `Result<(), String>` validation errors as in `ConfigKey::from_str` (keeps `apvm-config` dependency-free).

---

## 3. Algorithms

### 3.1 Target resolution

The positional is a registry name; slugs come from `Builder::plugin_slug(variant) -> Option<&'static str>`, deduped in declaration order.

| Invocation | Slugs |
|---|---|
| `uninstall backwpup` | `backwpup`, `backwpup-pro` — **all variants** |
| `uninstall backwpup --variants free` | `backwpup` |
| `uninstall backwpup --variants pro-de,pro-en` | `backwpup-pro` (deduped) |
| `uninstall wp-rocket` | `wp-rocket`; `--variants` warns and is ignored |

`--variants` (plural) on removal/read commands mirrors `apvm build`. `plugin install` takes `--variant` (singular): removal acts on a *set of installed directories*, install produces *exactly one artifact*.

### 3.2 Action matrix

| Command | Multi-slug | Confirm | Slugs not installed |
|---|---|---|---|
| `list` | n/a — every plugin on the site, apvm-managed ones marked | no | n/a |
| `status` | yes, one row per slug | no | reported |
| `activate` | **no** — must resolve to exactly one *installed* slug; 0 → error, >1 → error demanding `--variants` (free + pro active together fatals WP) | no | error |
| `deactivate` | yes, all installed | no | skipped |
| `uninstall` / `delete` | yes, all installed, two-pass | **yes** / `-y` | skipped |
| `install` | **no** | on conflict / `-y` / `--force` | n/a |

**Idempotency principle:** mutations *toward absence or inactivity* (`uninstall`, `delete`, `deactivate`) exit 0 when the end state already holds. Mutations *toward presence or activity* (`install`, `activate`) fail when impossible.

### 3.3 Removal — two passes

```
Pass 0   read state for every target slug
         must-use / dropin  → error (not managed by the plugin API)
         none installed     → "nothing to remove", exit 0
         confirm: table of slug | title | version | status   (or -y)

Pass 1   deactivate EVERY installed target      --network derived from observed status
         verify each is inactive
         any failure → abort; nothing destructive has run yet

Pass 2   uninstall (or delete) each, in order
         verify each is absent
```

Processing one slug end-to-end at a time would leave free active while pro uninstalls, and pro's cleanup would silently skip (§2). Splitting the passes is what makes multi-variant removal actually remove data. Requiring every deactivation to succeed before Pass 2 begins also gives a clean abort point.

**Sibling left active:** removing only some variants while another is active defeats the guard. apvm warns with the reason and the exact fix command; `--deactivate-siblings` opts in (siblings are left deactivated, never silently reactivated). The default — removing all variants — has no siblings and is therefore always correct.

### 3.4 Install

1. Resolve site → build `SiteClient` (validate config, resolve secret; no I/O).
2. **Pre-flight** `plugin_state(declared_slug)` — *before* any build, so an abort costs zero build time.
3. CLI prompts if installed → policy `fail` (default) \| `reinstall` (`-y`) \| `overwrite` (`--force`) \| `skip`.
4. Produce the artifact: `BuildCommand` into a temp dir (auto-cleaned; `--output` also keeps it). Cache hit → no clone or build; `release:` → download.
5. Derive the authoritative slug from the zip root; warn on mismatch with the declared slug.
6. **Re-check** state and enforce the policy again — a caller's earlier check is never trusted.
7. Sibling-conflict check: if another variant is *active*, warn; with `--activate`, refuse and name the `apvm plugin deactivate` command to run.
8. `reinstall` → run §3.3.
9. `make_temp_dir` → `push_file` → `wp plugin install <remote-zip> [--force]`.
10. Verify presence; compare version when known.
11. `--activate` → **separate** `wp plugin activate` → verify. Separate so an activation fatal is never mistaken for an install failure.
12. `remove_temp_dir` always; failure is a warning, not an error.

### 3.5 Slug from the artifact

`plugin_slug_from_zip()` reads only the zip **central directory** (`ZipArchive::file_names()` — no decompression, so compression method is irrelevant), collects distinct first path components, ignores `__MACOSX`/`.DS_Store`, rejects entries that are absolute or contain `..` (zip-slip signal → refuse the archive), and errors clearly on zero or multiple roots.

---

## 4. Command surface

```
apvm site add --url <URL> --path <WP_ROOT> [--id <ID>] [--name <NAME>]
              [--transport ssh|local|docker]                    # default ssh
              --host <HOST> [--user <USER>] [--port <N>]
              [--key <PATH> | --agent | --password-env <VAR> | --password-stdin]
              [--connect-timeout <SECS>] [--wp-cli <CMD>] [--allow-root]
              [--wp-url <URL>] [--notes <TEXT>] [--no-test] [--force] [-y]
apvm site list                        # id, name, url, transport, target
apvm site show   <ID>                 # full config, secrets masked
apvm site test   <ID>                 # transport → wp → WP core → multisite → plugins dir
apvm site remove <ID> [-y]            # forgets the site; never touches the server

apvm plugin list       -s <ID>
apvm plugin status     <PLUGIN> -s <ID> [--variants <LIST>]
apvm plugin install    <PLUGIN> <GIT_REF> -s <ID> [--variant <V>] [-v <VER>]
                       [--activate] [--network] [--no-cache] [--force] [-y]
                       [--output <DIR>] [--dry-run]
apvm plugin activate   <PLUGIN> -s <ID> [--variants <LIST>] [--network]
apvm plugin deactivate <PLUGIN> -s <ID> [--variants <LIST>] [--network]
apvm plugin uninstall  <PLUGIN> -s <ID> [--variants <LIST>] [--deactivate-siblings] [-y]
apvm plugin delete     <PLUGIN> -s <ID> [--variants <LIST>] [-y]
```

- `-s/--site` may be omitted when exactly one site is registered (a note is printed); otherwise the error lists valid ids.
- `--dry-run` prints the resolved ref, slugs, current site state and every command it *would* run; changes nothing.
- Editing a site = `apvm site add --id <same> --force`, or hand-edit `sites.json`.
- `<PLUGIN> <GIT_REF>`, `--ver` and `--no-cache` move into a shared `SourceArgs` flattened into both `build` and `plugin install` — one clap definition, one `git_ref` long-help.

### Commands issued, and flags deliberately banned

Per-site suffix on every call: `--path=<path>` `[--allow-root]` `[--url=<wp_url>]`.

| Purpose | Command |
|---|---|
| read state | `wp plugin list --format=json --fields=name,status,version,title,file --skip-update-check --skip-plugins --skip-themes` |
| probe | `wp --version`, `wp core version`, `wp core is-installed [--network]`, `wp plugin path` |
| deactivate | `wp plugin deactivate <slug> [--network]` |
| uninstall | `wp plugin uninstall <slug>` |
| delete | `wp plugin delete <slug>` |
| install | `wp plugin install <remote-zip> [--force]` |
| activate | `wp plugin activate <slug> [--network]` |

| Banned flag | Why |
|---|---|
| `plugin uninstall --deactivate` | Same process → classes loaded → the `class_exists` guard skips cleanup |
| `plugin deactivate --uninstall` | Identical problem, opposite direction |
| `plugin install --activate` | Activation is a separate, separately verified step |
| `--all`, `--exclude` | apvm names exactly one slug per invocation; a bug can never cascade |
| `--skip-plugins` on mutations | Hooks must run; reads only |

---

## 5. Data model — `~/.apvm/sites.json`

`snake_case`, matching `config.json`. `version` gives migration room.

```json
{
  "version": 1,
  "sites": [
    {
      "id": "staging",
      "name": "Staging",
      "url": "https://staging.domain.com",
      "path": "/var/www/staging/public_html",
      "transport": {
        "kind": "ssh",
        "host": "staging.domain.com",
        "user": "deploy",
        "port": 22,
        "auth": { "kind": "key", "path": "~/.ssh/id_ed25519" },
        "connect_timeout_secs": 15
      },
      "wp_cli": ["wp"],
      "allow_root": false,
      "wp_url": null,
      "notes": "QA env"
    }
  ]
}
```

New in `crates/config/src/sites.rs` — pure data and validation, no I/O, no new dependency:

| Type | Notes |
|---|---|
| `SitesFile { version, sites }` | `get` / `add` / `remove` / `ids` / `next_unique_id` |
| `Site` | `display_name()` = `name` or URL host; `validate() -> Result<(), String>`; `redacted()` |
| `Transport` | `#[serde(tag="kind", rename_all="snake_case")]` → `Ssh` \| `Local` \| `Docker`; enum dispatch, no `dyn`, no `async-trait` |
| `SshTransport` | `host`, `user?`, `port?`, `auth`, `connect_timeout_secs?`, `strict_host_key_checking?` |
| `SshAuth` | `Agent` \| `Key { path, passphrase_env? }` \| `Password(PasswordSource)` |
| `PasswordSource` | `Env { env }` \| `Value { value }`; **hand-written `Debug`** printing `****` so a secret can never reach a log or panic message |

Validation, all allowlists, all unit-tested: `id` `^[a-z0-9][a-z0-9._-]{0,31}$`; `host` `[A-Za-z0-9._:-]`, ≤255; `port` 1–65535; `path` absolute, no NUL or newline (spaces allowed — quoting handles them); `wp_cli` non-empty tokens. Auto-`id` derives from the URL host (`staging.domain.com` → `staging-domain-com`), de-duplicated with a numeric suffix.

`crates/core/src/sites_io.rs` mirrors `config_io.rs`: `load_sites` (missing file → empty, not an error), `save_sites` (atomic temp+rename, `0o600` on Unix, parent created), `resolve_site`. `Paths::sites_file()` → `~/.apvm/sites.json`.

---

## 6. Architecture

```
crates/core/src/
  sites_io.rs                load/save/resolve sites.json
  wp/
    mod.rs                   SiteClient — public entry point
    exec.rs                  trait CommandRunner + CommandSpec / CommandOutput
    transport/ssh.rs         system ssh/scp (P2) + native password path (P5)
    transport/local.rs       P6
    transport/docker.rs      P7
    quote.rs                 POSIX single-quote escaping + allowlist guards
    cli.rs                   wp argv construction + JSON parsing (pure)
    plugin.rs                PluginInfo / PluginStatus / PluginState / policies
    artifact.rs              slug from zip root
    progress.rs              SiteEvent / SitePhase / SiteReporter (+ Null, Closure)
    install.rs               install + activate orchestration
    remove.rs                two-pass deactivate → uninstall | delete
```

The one abstraction that matters:

```rust
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub trait CommandRunner: Send + Sync {
    fn exec<'a>(&'a self, cmd: &'a CommandSpec) -> BoxFuture<'a, Result<CommandOutput>>;
    fn make_temp_dir<'a>(&'a self) -> BoxFuture<'a, Result<RemoteTempDir>>;
    fn push_file<'a>(&'a self, local: &'a Path, into: &'a RemoteTempDir) -> BoxFuture<'a, Result<String>>;
    fn remove_temp_dir<'a>(&'a self, dir: RemoteTempDir) -> BoxFuture<'a, Result<()>>;
}
```

Boxed futures rather than `async-trait`: **no new dependency**, and `dyn`-compatible. Three real implementations plus a scripted `FakeRunner`, so every orchestration path — including "exit 0 but stderr says `Error:`" — is unit-testable offline. Being public, it also lets a library consumer supply their own transport (k8s exec, CI runner).

| Transport | exec | temp dir | upload |
|---|---|---|---|
| SSH | `ssh -T [-p N] [-i key -o IdentitiesOnly=yes] [-o BatchMode=yes] [-o ConnectTimeout=N] user@host <remote-cmd>` | `mktemp -d` | `scp -P N [-i key] <local> user@host:<dir>/` |
| Local | `tokio::process::Command` argv, `current_dir(path)` — **no shell at all** | `tempfile::tempdir()` | `fs::copy` |
| Docker | `docker exec [-u U] [-w path] <container> <argv…>` — no shell | `docker exec <c> mktemp -d` | `docker cp <local> <c>:<dir>/` |

Remote command shape (SSH is the only place a shell is involved):
`cd <q(path)> && <q(wp…)> --path=<q(path)> [--allow-root] [--url=<q(wp_url)>] <q(arg)>…`

Using an `ssh_config` `Host` alias as `host` gives ProxyJump, agent and per-host keys for free.

**Progress:** a `SiteReporter` parallel to the existing `ProgressReporter`, with `SitePhase` = `Connect, Probe, Inspect, Deactivate, Uninstall, Delete, Upload, Install, Activate, Verify, Cleanup`. `apvm plugin install` drives both reporters into one spinner; `BuildEvent` is left untouched. Emitted commands are redacted.

**Errors** added to `apvm_core::Error`: `Site`, `Transport`, `Wp { command, status, stderr }`, `NotAWordPressSite`, `PluginAlreadyInstalled { slug, version, status }`, `PluginNotInstalled`.

---

## 7. Reliability — exit codes are not trusted

1. Every invocation returns `CommandOutput { status, stdout, stderr }`. Nothing branches on `status` alone.
2. Reads use `--format=json` and **must** parse. Unparseable stdout → `Error::Wp` carrying truncated, `escape_debug`-ed stdout+stderr — PHP notices printed before the JSON are common and must not be mistaken for "not installed".
3. Plugin matching is by `name == slug` **or** `file` starting with `slug/`, so nothing depends on WP-CLI's internal name derivation.
4. Reads add `--skip-plugins --skip-themes` (a fatal in a just-installed build must not blind every later command) and `--skip-update-check` (no wordpress.org round-trip). Mutations that need hooks never skip plugins.
5. Every mutation is confirmed by re-reading state. That, not the exit code, is the success criterion.
6. `stderr` is scanned for `^Error:` / `^Warning:` — WP-CLI exits 0 on some warnings.
7. `core is-installed` produces no output, so it is the one command where the exit code is the signal; it is cross-checked against `wp core version`.
8. Known failures get mapped, actionable errors: WP-CLI absent, `wp-load.php` not found (wrong `path`), "Could not create directory" / FTP credentials (file ownership), plugins dir not writable.

---

## 8. Security

1. **No local shell, ever** — argv arrays only. SSH is the only place a (remote) shell is involved.
2. Every token interpolated into a remote command passes an allowlist **and** `quote::sh_single()` (`'` → `'\''`). Quoting is tested by round-tripping adversarial strings through a real `sh -c 'printf %s …'` on Unix.
3. In the current plan, slugs are `&'static str` compile-time constants from builders. The slug allowlist (`^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`, no leading `-` or `.`) is nonetheless enforced **in core**, so NAPI callers are protected by construction and §10's enhancements are cheap.
4. Secrets never appear in argv (visible in `ps`): env-var indirection, or the child's own environment. `Debug`, `Display` and all events print `****`. `sites.json` is `0600`.
5. Host keys: the system-`ssh` path uses the user's own `known_hosts` policy, with `BatchMode=yes` so a prompt fails fast instead of hanging. The native path verifies against the known-hosts file and never uses a no-check mode.
6. Remote temp directories come from `mktemp -d`; cleanup only ever removes a path we created, validated absolute, via `rm -rf -- <quoted>`.
7. Destructive operations require confirmation or `-y`. Core's default policy is `Fail`.

---

## 9. NAPI surface

```ts
class SiteRegistry {                      // optional convenience over ~/.apvm/sites.json
  static load(path?: string): SiteRegistry;
  list(): SiteConfig[];  get(id: string): SiteConfig | null;
  add(site: SiteConfig): void;  remove(id: string): boolean;  save(): string;
}

class Site {                              // self-contained: config passed inline
  static create(config: SiteConfig): Site;
  probe(onProgress?): Promise<JsSiteProbe>;
  plugins(): Promise<JsPluginInfo[]>;
  pluginState(slug: string): Promise<JsPluginState>;   // ← isPluginInstalled, but richer
  activate(slug, opts?): Promise<JsPluginInfo>;
  deactivate(slug, opts?): Promise<JsPluginInfo>;
  uninstallPlugin(slugs: string[], onProgress?): Promise<JsRemovalOutcome>;  // two-pass
  deletePlugin(slugs: string[], onProgress?): Promise<JsRemovalOutcome>;
}

apvm.installPlugin(opts: JsInstallPluginOptions, onBuild?, onSite?): Promise<JsInstallPluginOutput>
```

`existing: 'fail' | 'reinstall' | 'overwrite' | 'skip'` as a `#[napi(string_enum)]`, default `fail`. Site progress uses a second `ThreadsafeFunction`, mirroring the build reporter. `Site.installArtifact` is held back to §10 E2 so the CLI and NAPI security surfaces never diverge. Generated `index.js` / `index.d.ts` / `*.node` remain CI-produced — never hand-edited or committed.

---

## 10. Phases

Every phase ends green on the full suite and updates `SKILL.md` in the same change (per `CLAUDE.md`). Local exercises always use `APVM_CACHE_DIR="$(mktemp -d)"`.

```sh
echo -e "\n=== RUNNING CARGO FMT ==="; cargo fmt --check
echo -e "\n=== RUNNING CARGO CLIPPY ==="; cargo clippy --all-targets --all-features -- -D warnings
echo -e "\n=== RUNNING CARGO TEST ==="; cargo test
echo -e "\n=== RUNNING CARGO DOC ==="; cargo doc --no-deps
# plus `npm test` and `npm run typecheck` whenever the napi crate changes
```

### Phase 1 — Site registry
**New:** `crates/config/src/sites.rs`; `crates/core/src/sites_io.rs`; `crates/cli/src/commands/site/{mod,add,list,show,remove}.rs`; `Paths::sites_file()`; `Error::Site`.
**Tests:** validation of id/host/path/port including rejections; auto-id derivation and collisions; secret masking in `Debug` and `redacted()`; sparse and full serde round-trips; unknown `version` rejected with an actionable message; atomic save and `0600` (Unix-gated); `resolve_site` across explicit / single-site / ambiguous / unknown.
**Exit:** `site add|list|show|remove` work; `--test` parses as a documented no-op.

### Phase 2 — WP-CLI layer + SSH transport (key / agent / ssh_config) + `site test`
**New:** `wp/{mod,exec,quote,cli,plugin,progress}.rs`, `wp/transport/ssh.rs`; `Error::{Transport, Wp, NotAWordPressSite}`; `apvm site test`; probe wired into `site add`.
**Details:** argv builders are pure functions returning `CommandSpec`; `which("ssh")`/`which("scp")` preflight with actionable errors; probe implements §7.7 plus an advisory warning when the site's real `siteurl` differs from the configured `url` (catches a wrong `path` on a multi-site server).
**Tests:** golden argv and remote-command strings; quoting round-trip through real `sh`; `FakeRunner` probe success/failure matrix; JSON fixtures (valid, empty, PHP-notice prefix, HTML error page, truncated); stderr `Error:` detection. Opt-in end-to-end test behind `APVM_TEST_SSH_SITE=user@host:/path`, skipped by default.
**Exit:** `apvm site test staging` reports transport, WP-CLI version, WP version, multisite, plugins dir, and any `siteurl` mismatch.

### Phase 3 — Read and lifecycle commands over SSH
**New:** `wp/remove.rs`; `Builder::plugin_slug` plus the three implementations (default `None`, so no public struct gains a field) and slug display in `apvm info`/`list`; `crates/cli/src/commands/plugin/{mod,list,status,activate,deactivate,uninstall,delete}.rs`; `Error::PluginNotInstalled`; slug allowlist enforced in core.
**Details:** the §3.3 two-pass remover with the BackWPup rationale documented in code; variant→slug dedupe; `must-use`/`dropin` rejected; `--network` derived from observed status; sibling detection and `--deactivate-siblings`; confirmation reusing the existing `confirm()` shape.
**Tests:** dedupe table (`pro-de,pro-en` → one slug); **two-pass ordering asserted against `FakeRunner` — no uninstall may be issued before every deactivate is verified**; abort-on-deactivate-failure destroys nothing; sibling warning fires, and is silent when the target set covers all variants; idempotent exit-0 paths; `activate` refusing 0 and >1 installed slugs; state matching by `name` and by `file` prefix; every status value plus an unknown forward-compat value; allowlist rejection of `-x`, `.`, `..`, `a/b`, `a;b`, empty, >64 chars, non-ASCII.
**Exit:** inspect, activate, deactivate, uninstall and delete apvm plugins on an SSH site, all variants handled.

### Phase 4 — `apvm plugin install`
**New:** `wp/{install,artifact}.rs`; `ExistingPluginPolicy`; `Apvm::install_plugin`; `crates/cli/src/commands/plugin/install.rs`; shared `SourceArgs` extracted from `BuildArgs` (updates `build.rs` and its tests); `Error::PluginAlreadyInstalled`; NAPI `Site` / `SiteRegistry` / `Apvm.installPlugin`.
**Details:** the full §3.4 flow — pre-flight before building, temp-dir build, zip-derived slug, policy re-check, sibling guard, separate activation, verified version, guaranteed cleanup, `--dry-run`.
**Tests:** policy decision table; pre-flight-before-build ordering; slug-from-zip fixtures (single root, multi-root, `__MACOSX`, zip-slip); mismatch warning; sibling conflict with and without `--activate`; version-mismatch warning; cleanup on the failure path; `--dry-run` issues no mutating command; napi vitest for registry round-trip, validation errors and the `existing` default.
**Exit:** `apvm plugin install backwpup 123 -s staging --variant pro-en -v 5.1.0 --activate` works end to end, cache hits included.

### Phase 5 — SSH password authentication
**Spike first, do not assume:** whether `async-ssh2-tokio` 0.13 exposes `stderr` separately on its result type, the exact `ServerCheckMethod` variants for known-hosts verification, and whether upload is reachable without depending on `russh-sftp` directly. If any is missing, take the `SSH_ASKPASS` route instead — the seam makes it local.
**New:** native path in `wp/transport/ssh.rs`; `async-ssh2-tokio` in the root `Cargo.toml`; `--password-env` / `--password-stdin` on `site add`.
**Tests:** auth-method selection table; missing-env-var error; **the secret is asserted absent from every event, error, `Debug` output and argv**; known-hosts failure surfaces as `Error::Transport`.

### Phase 6 — Local transport
`wp/transport/local.rs`: direct argv, `current_dir(site.path)`, no shell anywhere, `which(wp)` preflight, `tempfile` staging, `fs::copy`. `--transport local` on `site add`, with host/auth flags rejected clearly.
**Tests:** argv goldens, plus a fully offline end-to-end run against a scripted fake `wp` on `PATH` — exercises the whole orchestration through a real process boundary.

### Phase 7 — Docker transport
`wp/transport/docker.rs`: `docker exec [-u] [-w]`, `docker cp`, in-container `mktemp -d`; config gains `container` and optional `user`/`workdir`; `--allow-root` is commonly required. `docker inspect` preflight distinguishes no docker / daemon down / no such container / not running.
**Tests:** argv goldens; opt-in integration test behind `APVM_TEST_DOCKER=1` using the official `wordpress` and `wordpress:cli` images.

### Cross-cutting, folded into Phases 4 and 7
`references/sites.md` and `references/plugin-lifecycle.md` added to `.claude/skills/apvm-cli/` **and** to `SKILL_FILES` in `crates/cli/src/commands/skill/embedded.rs` (the `embedded_list_matches_repo_directory` test enforces this). `SKILL.md` command tables, env vars and version line updated. `crates/{cli,core,napi}/README.md` updated. Workspace version and `package.json` → **3.3.0** (the skill test requires `SKILL.md` to mention the current version). Dependencies are declared only in the root `Cargo.toml`.

---

## 11. Possible enhancements

The threat model is not "the user attacks their own site" — they could run `ssh host …` directly. It is: **(1) automation** — apvm is a NAPI library, and a harness may derive a slug or path from a PR title, branch name, webhook payload or filename, which *is* untrusted input reaching argv; **(2) accidental damage** — `*`, `.`, `..`, `-f` or an empty string reaching `wp plugin delete`; **(3) a hostile or simply wrong zip**. Mitigations are layered, never single.

### E1 — Manage arbitrary (non-apvm) plugins: `--slug <SLUG>`

Adds `--slug` to `status` / `activate` / `deactivate` / `uninstall` / `delete`.

| Control | Detail |
|---|---|
| Allowlist | `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$` — a WP slug is a directory name or a `.php` basename. Excludes whitespace, `/ \ ; & \| $ ' " * ? [ ] ( ) < > ! #`, backtick, newline, NUL, non-ASCII. |
| Leading character | Ban leading `-` (a slug must never parse as a WP-CLI option) and leading `.` (kills `.` and `..`). Defense in depth, not a claimed exploit. |
| **Echo-back verification** | Before any mutation the slug must appear in `wp plugin list --format=json` as an exact `name` or `file` prefix. We only ever pass back a string WordPress itself reported — this alone defeats the glob and misresolution class. |
| No bulk flags | apvm never constructs `--all` / `--exclude`. One slug per invocation. |
| Quoting | Allowlisted slugs still pass through `sh_single()`. Allowlist **and** quoting. |
| Confirmation | The prompt shows the site's own view — slug, title, version, status — so the user confirms reality, not their typed string. |
| Blast radius | An arbitrary slug reaches only `wp plugin {activate,deactivate,uninstall,delete}` for one verified, listed plugin. No `wp eval`, no `wp db`, no shell. |

Cost: a CLI flag, help text and docs. The core validation already ships in Phase 3.

### E2 — Install an arbitrary local zip: `--artifact <ZIP>`

| Control | Detail |
|---|---|
| Local path | An argv element for `scp` / `docker cp` / `fs::copy` — no local shell, so no injection. Canonicalize; require an existing regular file; require `.zip`; cap size (reject > 256 MB with a clear message). |
| **Rename on upload** | Always uploaded as a fixed safe name (`apvm-upload.zip`) into a fresh `mktemp -d`. Eliminates the filename vector instead of sanitizing it. |
| Structure | Must open; exactly one top-level directory ignoring `__MACOSX`/`.DS_Store`; anything else refused. |
| **Zip-slip** | Reject any entry that is absolute, contains `..`, a backslash or NUL, or is not valid UTF-8. We do not extract — *WordPress* does — so refusing a hostile archive rather than handing it over is the correct posture. |
| Derived slug | The root directory name passes the same E1 allowlist, so a zip rooted at `-rf` or `../../evil` can never become a WP-CLI argument. |
| "Is this a plugin?" | Require a `<root>/*.php` entry containing a `Plugin Name:` header, reusing the existing header parser. Catches the most likely real error: the wrong zip. |
| Verification | Presence verified; version compared only when known (optionally read from the zip header inside E2), so no false mismatch warnings. |
| Provenance | A one-line notice that an arbitrary artifact bypasses apvm's build and cache provenance and cannot be tied to a commit. |

Cost: a CLI flag, the validations above, and exposing `Site.installArtifact` in NAPI. The upload/install/verify machinery already ships in Phase 4.

### E3 — Smaller items
Multi-site fan-out (`-s a -s b`); `apvm site set <id> <key> <value>`; a default-site pointer (`apvm site use`); OS-keychain secrets; `0600` on the existing `config.json`, which holds the GitHub token.
