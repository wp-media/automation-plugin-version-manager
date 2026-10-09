# Site deployment — manage WordPress plugins on remote sites

Status: **planned** — rewritten 2026-10-09, revisions 2 and 3 the same day (review feedback) · Target version: **3.4.0** (from 3.3.0) · Author: Sandy Figueroa

---

## TL;DR

- **What:** a new **library crate `apvm-sites`**, used by the CLI, the Node bindings (NAPI) and `apvm-core`.
  - **Inventory:** servers → sites → groups, with one default SSH key and per-server overrides.
  - **Plugin operations:** install (apvm builds, cached or built on demand, or **any zip**), list, **status checks** (is it installed/active, which version/variant), activate, deactivate, uninstall, reset (uninstall but keep files), delete, and ownership check/fix. They work by slug (or by apvm project, or `--all`), on one site or a batch of sites.
  - **`apvm wp run`:** runs any WP-CLI command on many sites, safely.
- **How it stays safe:** every command runs **plan → confirm → apply → verify → report**. Each step is one WP-CLI process, confirmed by **re-reading WordPress state**; exit codes are never trusted alone. Removal is always **two-phase** (deactivate, verify, then uninstall in a separate process).
- **Variants are managed:** apvm knows which slug each variant installs to and tells pro-de from pro-en on a site.
  - **Free and pro are independent plugins:** each can be installed, activated, uninstalled or reset on its own, and both can be installed or active at the same time (coexistence tests). Nothing is blocked because of a sibling; the plan only notes what BackWPup's own guards will do.
  - **Activating next to an active sibling** switches by default (deactivate the sibling, then activate, as separate processes); `--keep-siblings` keeps both active.
  - **pro-de and pro-en share one folder:** asking for both is an error; `--force` replaces one with the other.
- **Ownership respects owner *and* group:** WP-CLI runs as the plugins directory's owner **and group** (`sudo -n -u <owner> -g <group>`), so `ftpuser:www-data` sites stay `ftpuser:www-data` with the group bits the web server needs. Mismatches are repaired least-privilege-first: `chgrp` as the owner before root `chown`.
- **Uploads:** each zip is inspected locally, streamed over SSH into a private **staging dir** (`/tmp/apvm.XXXXXXXXXX`, owned by the identity, `0700`), checksum-verified, installed from there, and always cleaned up.
- **Transport:** system OpenSSH (your `~/.ssh/config`, agent, ProxyJump), multiplexed on Unix. Scripts travel over SSH's stdin, so the remote login shell (bash, zsh, fish, …) never parses them.
- **Phases:** 43 small ones; libraries first, then the CLI and NAPI front ends in parallel tracks. No new crates.io dependencies.

---

## 0. Change log

### Revision 3 (review feedback, 2026-10-09)

| Topic | Change |
|---|---|
| Free + pro independence | The `family_active` block is gone: free and pro install, activate, uninstall, reset and delete independently, and may coexist installed or active. The plan notes guard effects instead of blocking |
| Both active | `--keep-siblings` on `activate` and `install --activate` keeps an active sibling active; the default stays the verified switch |
| Status queries | New `apvm wp status` (`--expect`, `--version`, `--variant`, `--ownership`) plus `is-installed` / `is-active`; exit codes 0 / 1 / 3; NAPI `status`, `isInstalled`, `isActive` |
| Lock | New `apvm site unlock` for a stuck site lock |
| Command reference | §5 rewritten as a complete, organized reference |

### Revision 2 (review feedback, 2026-10-09)

| Topic | Change |
|---|---|
| Decisions §16 | Finalized: `apvm wp`, `--if-installed fail`, family rule blocks, owner identity, `inventory.json` |
| Variants | New §6.9: families, variant markers (pro-de vs pro-en verified), variant switch, `--variants`, `--project`, shared-data rule |
| Group | Identity is **owner + group** (`sudo -u U -g G`); group bits and setgid mirrored from the plugins dir; repair ladder `chgrp` (no root) → root `chown`; FTP-owned sites in the E2E harness |
| WP-CLI passthrough | New `apvm wp run` (§6.12) with a command classifier, refused-command list, confirmation for writes, NAPI `runWp` |
| Staging | Promoted to its own section (§6.6): location, upload, checksum, space check, cleanup, stale sweep |
| Script delivery | Scripts now travel over **stdin** (`sh -s`); only uploads use `sh -c` with allowlisted values |
| Concurrency | New per-site remote lock (§6.13) so two people never deploy to one site at once |
| New command | `apvm wp ownership [--fix]` (check/repair existing installs) |
| Library-first | §8.1 library contract; NAPI phases interleaved per capability; core uses `apvm-sites` now (hints + build→install) |

### Revision 1 (vs the first draft)

Arbitrary slugs and zips are in scope; inventory with defaults/servers/sites/groups; owner-aware execution; `--all` expanded by apvm; `reset`; one plan + one confirmation; `delete` deactivates first; key-only SSH (passwords → E1); local/docker transports → E2; hermetic and Docker E2E tests.

---

## 1. Goals and non-goals

| # | Goal |
|---|---|
| G1 | Persist servers, sites (a WP path per server), groups, one default SSH identity and per-server overrides. |
| G2 | Target one site, several sites, groups, all sites of a server, or every site. |
| G3 | Install an apvm plugin from a ref: cache hit → no build; miss → build (or release download) first. |
| G4 | Install any WordPress plugin zip, or several, on any target set. |
| G5 | List, activate, deactivate, uninstall, reset and delete, by slug, by apvm project, or `--all [--exclude]`. |
| G6 | Activation is its own command and an optional install step. |
| G7 | Removal is always two-phase. |
| G8 | Files end up with the plugins dir's **owner, group and group access**; `sudo` only in strict, documented shapes. |
| G9 | Variants are first-class: known slugs, detection on sites, free/pro fully independent (both installed or active allowed), switching by default with an opt-out, same-slug requests refused. |
| G10 | Run arbitrary WP-CLI commands on targets, with the same identity, safety and reporting. |
| G11 | Clear success/failure per site × plugin, `--json`, a diagnosable run log. |
| G12 | Library-first: every capability is an `apvm-sites` API; the CLI and NAPI are thin; core can use it. |
| G13 | Ask whether plugins are installed/active (and which version/variant), human-readable or as a script assertion. |

**Non-goals (3.4.0):** themes; managing WP-CLI itself (`wp cli update`, `wp package`); SSH passwords; FTP/SFTP-only hosts; Windows *remote* hosts (a Windows *client* works); hosts without POSIX `sh`; interactive `sudo` passwords.

---

## 2. Decisions

| # | Decision | Choice | Rationale | Rejected |
|---|---|---|---|---|
| D1 | Placement | New crate `apvm-sites` (no `apvm-*` deps). `apvm-core` depends on it for plugin hints and build→install. | Zip installs and `wp run` need no GitHub/build stack; core stays the glue library (CLAUDE.md); one dependency direction | All in core; CLI-only |
| D2 | Transport | System OpenSSH, argv only (no local shell) | Inherits `~/.ssh/config`, agent, known_hosts; no new crypto deps | `russh`/`async-ssh2-tokio` |
| D3 | Script delivery | **stdin**: remote command `sh -s`, script on stdin. Uploads: `sh -c '<script>'` with allowlisted values only | sshd runs commands via the login shell (§3.4). With stdin delivery that shell parses only `sh -s`, so `wp run` arguments may contain any byte but NUL | Quoted command strings for everything (fish mangles `\\`; csh rejects newlines) |
| D4 | Identity | `run_as = auto`: run as the plugins dir's **owner U with primary group G** (`sudo -n -u U -g G`); root only by explicit opt-in | Files *and* activation side effects get owner + group right from creation | SSH user + chown afterwards (kept as `run_as = login`) |
| D5 | Ownership | `ownership = mirror`: verify owner, group and group bits after file-creating steps and owner/group before removals; repair least-privilege-first | Your FTP-user/`www-data` layout; root only when unavoidable | Always chown; never chown |
| D6 | Modes | Mirror only the **group** bits (r/w/x, setgid) the plugins dir grants; only add, never remove; never touch "other" (o+w reported) | Web server keeps group read/write; nothing is ever loosened beyond the dir's own policy | Copying full modes; leaving it to `FS_CHMOD_*` alone |
| D7 | Safety model | plan → confirm → apply → verify → report | One review per batch; dry-run = plan | Per-conflict prompts |
| D8 | Concurrency | Sites in parallel (`--jobs` 4, ≤ 4 per server); steps per site strictly sequential; a per-site remote lock | `active_plugins` is one option; WP empties `wp-content/upgrade/` on every extraction (§3.3); two operators | Parallel steps per site |
| D9 | Removal | Pass 1 deactivate (own process) and verify; pass 2 uninstall/delete | BackWPup guard (§3.1) | `--deactivate` / `--uninstall` shortcuts |
| D10 | Verbs | `uninstall` = data + files · `reset` = data (`--skip-delete`) · `delete` = files; all deactivate first | Three outcomes, three verbs | Flags |
| D11 | `--all` | apvm expands to explicit slugs (no must-use/dropin) | Per-plugin verification still applies | WP-CLI `--all` |
| D12 | Already installed | `--if-installed fail` (default) \| `skip` \| `replace` (`--force`) \| `reinstall` | No silent destruction | Prompting per site |
| D13 | Health | After activation, a full WP boot (`wp option get siteurl`) must succeed | Catches "activated but the site fatals" | — |
| D14 | Multisite | Reads from day one; mutations blocked until P10 | Never delete files a subsite uses | Shipping a hole |
| D15 | Inventory | `~/.apvm/inventory.json`, `APVM_INVENTORY`, `0600`, atomic, locked, versioned, unknown fields rejected | Editable, shareable (no secrets), test-isolatable | SQLite; `config.json` |
| D16 | Credentials | Identity *paths* + agent; no stored secrets | Nothing to leak | Plaintext passwords |
| D17 | Grammar | `apvm server\|site\|group <verb>`; `apvm wp <verb>` (incl. `run`, `ownership`) | Existing `<noun> <verb>` style; no clash with `apvm list/info` | `apvm plugin …` |
| D18 | Build source | `Apvm::build` into a private temp dir; plan with the declared slug; the zip's slug must match | No pipeline changes; a confirmed plan can't drift | Separate cache-only path |
| D19 | Variants | Builders declare slug per variant, markers, "conflicts when active together" and "shared data" → `PluginHints` → `apvm-sites` (no plugin-specific code). Free and pro stay independent: apvm refuses only two variants of one slug, switches by default when activating next to an active sibling (`--keep-siblings` keeps both), and annotates plans with the guards' effects | QA needs both installed or active; BackWPup's guards are the plugin's intended behavior, so apvm reports them instead of overriding them | Blocking removal while a sibling is active (revision 2); hard-coding BackWPup in `apvm-sites` |
| D20 | WP-CLI passthrough | `apvm wp run`: argv-only, identity-run, classified (read-only allowlist; else confirmation / `allowWrites`), refuses target-changing params and interactive/unsafe commands | Power without shell injection or silent writes | A shell passthrough; no passthrough |
| D21 | Site lock | `mkdir`-based lock in the remote tmp dir, holder info, 30 min TTL; `apvm site unlock` | Atomic on POSIX, no DB writes, no WP boot | `wp option add` lock (writes the site DB) |
| D22 | Status queries | `apvm wp status` (assertions via `--expect`/`--version`/`--variant`) + `is-installed`/`is-active`; exit 0 met · 1 not met · 3 undetermined | Scripts and CI can assert deployment state; an unreachable site never reads as "not active" | Using `list` for checks; exit 0/1 only |

---

## 3. Verified ground truth

### 3.1 This repository and the BackWPup artifacts

Zip roots (`zipinfo -1`, real artifacts in `~/.apvm/cache`):

| Plugin | Variant | Slug (zip root) | Main file | Header `Plugin Name` |
|---|---|---|---|---|
| backwpup | `free` | `backwpup` | `backwpup.php` | `BackWPup` |
| backwpup | `pro-de` | `backwpup-pro` | `backwpup.php` | `BackWPup Pro` |
| backwpup | `pro-en` | `backwpup-pro` | `backwpup.php` | `BackWPup Pro` |
| wp-rocket | — | `wp-rocket` | `wp-rocket.php` | — |
| imagify | — | `imagify` | `imagify.php` | — |

- **pro-de vs pro-en:** a full-tree diff of both builds at commit `1dceea8` (9.99.99) differs only in `inc/Pro/License/License.php`, `WC_API_URL = 'https://backwpup.de/'` vs `'https://backwpup.com/'` (plus a POT timestamp). Headers are identical, so the variant can only be told apart by that file. The build uses `gulp pro --language=de|en`.
- **Both variants are network-only:** `Network: true` in free and pro.
- **Shared main-file guard:** line 25 of both main files is `if ( defined( 'BACKWPUP_PLUGIN_LOADED' ) || class_exists( \BackWPup::class, false ) ) { return; }`. With two variants active, only the first one loaded runs; the other returns before registering anything, including its `register_deactivation_hook` (line 109). Activating a variant in the same process that loaded a sibling never runs its activation.
- **Shared uninstall guard and data:** both `uninstall.php` files clean up only `if ( ! class_exists( \BackWPup::class ) )`, and they drop the same tables/options. Uninstalling a variant while a sibling is **active** removes its files but keeps the shared data (BackWPup's intended behavior); with no sibling active, it wipes data the sibling also uses.
- **Other facts:** `default_variants` = `free` + `pro-en` (two different slugs). Sizes run from 2.9 MB (Imagify) to 11 MB (BackWPup Pro). `apvm build` uses `-v` for `--ver`.

### 3.2 WP-CLI (latest stable 2.12.0, published 2025-05-07)

| Fact | Source |
|---|---|
| `plugin uninstall [--deactivate] [--skip-delete] [--all] [--exclude]`; an active plugin gets `The '<x>' plugin is active.` and is skipped; `--skip-delete` runs only the uninstall procedure | handbook + `Plugin_Command` |
| `plugin deactivate [--uninstall] [--all] [--exclude] [--network]`; a network-active plugin without `--network` is refused with a warning | handbook + source |
| `plugin delete` removes files, with no active check | handbook + source |
| `plugin install <plugin\|zip\|url>… [--force] [--activate] [--activate-network]`; `--force` → `DestructivePluginUpgrader` | source |
| `--slug`, `--with-dependencies` arrived in extension-command 2.3.0 (2026-03-09), after 2.12.0, so they are not used | releases |
| `plugin list`: fields include `name,status,version,file,title`; statuses `active, active-network, dropin, inactive, must-use`; `--skip-update-check` | source docblock |
| Batch results: any failure → `WP_CLI::error("Only … X of Y")` / `"No plugins …"` | `utils.php` |
| WP-CLI forces the filesystem method `direct`, so files are created as the OS user/group running `wp` | `Runner.php` |

### 3.3 WordPress core

| Fact | Consequence |
|---|---|
| `unpack_package`: extracts into `wp-content/upgrade/<zip basename>` and **empties `upgrade/` first** | One install per site at a time (D8, §6.13) |
| `install_package`: a single top-level folder becomes the plugin dir; otherwise the zip basename is used | Require one root; fixed upload names are harmless |
| Non-empty destination without `clear_destination` → `folder_exists` | `--if-installed` |
| Extraction: dirs `FS_CHMOD_DIR = fileperms(ABSPATH) & 0777 \| 0755`, files `FS_CHMOD_FILE = fileperms(ABSPATH.'index.php') & 0777 \| 0644`; zip symlinks become regular files | Modes follow ABSPATH, not the plugins dir. On FTP-style sites the group bits may need mirroring (D6) |
| `validate_plugin_requirements`: WP/PHP requirements, then `plugin_missing_dependencies` (WP ≥ 6.5) | Activation order = dependency order |
| `uninstall_plugin`: defines `WP_UNINSTALL_PLUGIN` and includes `uninstall.php`, or `include_once`s the main file for `register_uninstall_hook` | D9 |
| `deactivate_plugins` does not check dependents | — |

### 3.4 OpenSSH, sudo, coreutils

| Fact | Source |
|---|---|
| `ssh` exits with the remote status, or 255 on error | ssh(1) |
| "All commands are run under the user's login shell" | sshd(8) |
| First ssh_config value wins; `-o` beats config files; `ControlMaster`/`ControlPath` (`%C`)/`ControlPersist`; `accept-new`; `BatchMode`; `ServerAlive*`; `IdentitiesOnly` | ssh_config(5) |
| `MaxSessions` default 10 per connection | sshd_config(5) |
| Win32-OpenSSH lacks ControlMaster (community reports) | → no multiplexing on Windows |
| `sudo -n` never prompts; `--` ends options | sudo(8) |
| `sudo -g G`: primary group G; "The sudoers policy permits any of the target user's groups to be specified via the -g option"; a user-only Runas_Spec allows "any group the target user belongs to" | sudo(8), sudoers(5) |
| `env_reset` sets `HOME`, `SHELL`, `USER`… for the target user | sudoers(5) |
| GNU `chown`: `-P` is the recursive default; `-h` acts on links; apvm passes `-R -P -h` | coreutils |
| Local: OpenSSH_10.3p1, Docker 29.8.2, Compose v5.5.1, rustc 1.99.0 | `--version` |

### 3.5 To verify in P0 (on the harness, before code depends on it)

- **WP-CLI 2.12.0:**
  - `--skip-delete` and `plugin list --skip-update-check` are present.
  - `install --force` over an **active** plugin keeps it active.
  - `WP_CLI::error` exit status (expected 1).
  - Network-only activation without `--network`.
  - `--skip-plugins=<slug>` skips that plugin's deactivation hook.
  - `WP_CLI_VERSION` is visible in `wp eval`.
  - `plugin install` behavior when `DISALLOW_FILE_MODS` is set.
- **sudo:**
  - Exact messages for password required, not in sudoers, and requiretty.
  - `sudo -n -u ftpuser -g www-data` works when `ftpuser` ∈ `www-data`.
- **Groups and permissions:**
  - Whether WP-created plugin dirs keep the parent's setgid bit.
  - A plain `wp plugin install` run by `ftpuser` (primary group `ftpuser`) produces the expected group mismatch.
- **Shells and SSH:**
  - `sh -s` with commands using `</dev/null` behaves the same on dash, bash and busybox `ash`.
  - bash `.bashrc` noise on ssh commands.
  - `sh -c` single-quoting under bash/dash/zsh/fish/tcsh login shells.
  - `ssh … -- host cmd` is accepted.
- **Harness basics:**
  - Official `wordpress` image files are owned by `www-data`.
  - WP-CLI run as `www-data` (HOME/cache warnings).
  - `mktemp -d /tmp/apvm.XXXXXXXXXX` output shape.
  - `chown -R -P -h` leaves symlink targets untouched.
- **BackWPup coexistence** (on fixtures and, manually, real builds):
  - With free and pro both active, which one runs (load order) and whether the site boots cleanly.
  - The second activation's own activation routine does not run.
  - Uninstalling one while the other is active keeps the shared data.
- **Stderr samples:** real OpenSSH stderr for each failure class (feeds P3.3).

---

## 4. Concepts

### 4.1 Inventory

```
defaults.ssh ──► servers[] (host + ssh overrides) ──► sites[] (server + WP path + url + execution settings)
                                                        ▲
                                                 groups[] (named lists of sites)
```

Effective SSH settings = built-in → `defaults.ssh` → `server.ssh` (field by field) → `~/.ssh/config` for anything apvm does not pass.

### 4.2 Targets

`--site a,b` · `--group qa` · `--server eu-1` · `--all-sites`. Flags are repeatable (union), kept in inventory order and de-duplicated. The same `host:port:user` + `path` under two ids → warning. An empty selection is an error, never "everything".

### 4.3 Plugin selection

- `<SLUG>…`: WP-CLI `name`, cross-checked with `file`.
- `--project NAME`: every installed member of an apvm project's family, e.g. `backwpup` → `backwpup` + `backwpup-pro`.
- `--all [--exclude …]`: every regular plugin installed on each site.
- `must-use`/`dropin` are never selected; naming one explicitly is **blocked**.

### 4.4 Run lifecycle (state machine)

```
Resolving ──invalid input──────────────────────────────────────────► Error (exit 1)
   │ targets + sources resolved (zips inspected locally)
   ▼
Preflight ── per site, parallel: connect → probe → read plugins (+ variant markers)
   │           unreachable/unsupported site ⇒ that site Failed; others continue
   ▼
Planned ── pure planner per site ──► --dry-run: print plan ──► exit 0 (1 if anything is blocked)
   ▼
Confirming ── needed & no -y: TTY → ask once; non-TTY → Error "pass -y" (exit 1)
   │ declined ⇒ "Aborted. Nothing changed." (exit 0, like `apvm cache clear`)
   ▼
Binding ── build sources only: build once (cache-aware) → inspect → slug == declared
   ▼
Applying ── per site parallel: acquire site lock → steps sequential, each verified → release lock
   ▼
Reported ── table / --json + run log ──► exit 0 if every item is done/no-op/skipped, else 1
```

Per item: `Planned → Running(i) → Verifying(i) → … → Done`; a verification mismatch → `Failed{step, observed}`; a transport loss → re-read → `Done | Failed | Unknown`; an unmet prerequisite → `Blocked`.

### 4.5 Outcomes and codes

| Outcome | Meaning | Exit |
|---|---|---|
| `done` | Changed and verified | 0 |
| `no-op` | Already in the requested state | 0 |
| `skipped` | On purpose (`--if-installed skip`; "not installed" for deactivate/uninstall/delete) | 0 |
| `blocked` | Not attempted: precondition failed | 1 |
| `failed` | Attempted; verification says it did not happen (step, exit code, stderr excerpt) | 1 |
| `unknown` | Connection lost mid-mutation, state not re-readable | 1 |

Stable codes (CLI `--json`, NAPI `code`):

| Area | Codes |
|---|---|
| SSH | `ssh_missing`, `ssh_auth_failed`, `ssh_host_key_unknown`, `ssh_host_key_changed`, `ssh_unreachable`, `ssh_timeout` |
| sudo / identity | `sudo_missing`, `sudo_password_required`, `sudo_not_allowed`, `sudo_requiretty`, `owner_is_root` |
| WordPress | `wp_cli_missing`, `not_wordpress`, `wp_not_installed`, `plugins_dir_not_writable`, `multisite_unsupported` |
| Execution | `site_locked`, `insufficient_space`, `upload_corrupted`, `wp_command_failed`, `verification_failed`, `state_drifted`, `outcome_unknown`, `health_check_failed`, `timeout` |
| Ownership / variants | `ownership_unfixable`, `variant_conflict` (two variants of one slug, or two siblings activated together without `--keep-siblings`) |
| Input | `package_invalid`, `refused_command`, `confirmation_required`, `invalid_input`, `inventory_invalid` |
| Status | `expectation_not_met`, `undetermined` |

---

## 5. Command reference

Every command below is a thin front end over an `apvm-sites` API (§8.1); §11 lists the NAPI equivalents. Global `--verbose` (existing) works everywhere.

### 5.1 Shared argument groups

**TARGETS** — which sites. At least one is required; flags are repeatable, accept comma lists, and combine as a union.

| Argument | Selects |
|---|---|
| `-s, --site <ID>[,<ID>…]` | Sites by id |
| `-g, --group <ID>[,<ID>…]` | Every site of a group |
| `--server <ID>[,<ID>…]` | Every site on a server |
| `--all-sites` | Every site in the inventory |

**PLUGINS** — which plugins on each site. Exactly one form.

| Argument | Selects |
|---|---|
| `<SLUG>…` | Plugins by WP-CLI slug (`backwpup-pro`, `wp-rocket`, `akismet`, …) |
| `--project <NAME>` | Every installed plugin of an apvm project (`backwpup` → `backwpup` and/or `backwpup-pro`) |
| `--all [--exclude <SLUG>[,<SLUG>…]]` | Every regular plugin installed on each site (never must-use/drop-ins) |

**RUN** — options shared by every command that changes a site.

| Argument | Meaning |
|---|---|
| `--dry-run` | Print the plan; change nothing |
| `-y, --yes` | Skip the confirmation (required when stdin is not a terminal) |
| `--json` | Machine-readable report (`"schema": 1`) |
| `-j, --jobs <N>` | Sites processed in parallel (default 4; at most 4 per server) |
| `--lock-wait <SECS>` | How long to wait for a busy site lock (default 60) |

### 5.2 Inventory commands (offline, except `add`'s connection test and `test`)

**`apvm server` — SSH hosts**

| Command | Arguments | Notes |
|---|---|---|
| `server defaults` | `[--user <USER>] [--port <N>] [--key <PATH>] [--connect-timeout <SECS>] [--ssh-binary <PATH>] [--multiplex \| --no-multiplex] [--unset <FIELD>]… [--json]` | No flags = show. `--key` is the **one main key** |
| `server add <ID>` | `--host <HOST> [--user <USER>] [--port <N>] [--key <PATH>] [--jump <[USER@]HOST[:PORT]>[,…]] [--known-hosts <PATH>] [--ssh-config <PATH>] [--multiplex \| --no-multiplex] [--name <TEXT>] [--notes <TEXT>] [--no-test] [--trust-new-host-key]` | Tests SSH by default; `--key` overrides the default key for this server; `<HOST>` may be an `~/.ssh/config` alias |
| `server edit <ID>` | the `add` flags (except `--no-test`) + `[--unset <FIELD>]…` | |
| `server list` | `[--json]` | |
| `server show <ID>` | `[--json]` | Shows the effective (merged) SSH settings |
| `server remove <ID>` | `[--cascade] [-y]` | Refused while sites use it, unless `--cascade` |
| `server test <ID>…` | `[--trust-new-host-key] [--json]` | SSH, login user, groups, sudo |

**`apvm site` — WordPress installs on servers**

| Command | Arguments | Notes |
|---|---|---|
| `site add <ID>` | `--server <ID> --path <WP_ROOT> [--url <URL>] [--wp-cli "<CMD>"] [--run-as auto\|login\|<USER>] [--owner auto\|<USER>[:<GROUP>]] [--ownership mirror\|check\|off] [--allow-root] [--tmp-dir <PATH>] [--timeout <SECS>] [--group <ID>]… [--name <TEXT>] [--notes <TEXT>] [--no-test] [--trust-new-host-key]` | Probes the site by default (§6.2) |
| `site edit <ID>` | the `add` flags (except `--no-test`) + `[--unset <FIELD>]…` | |
| `site list` | `[--server <ID>] [--group <ID>] [--json]` | |
| `site show <ID>` | `[--json]` | |
| `site remove <ID>` | `[-y]` | Also leaves its groups |
| `site test` | `[<ID>…] [TARGETS] [--trust-new-host-key] [--json] [-j <N>]` | Full read-only probe |
| `site unlock <ID>` | `[-y]` | Removes a stuck site lock after showing its holder |

**`apvm group` — named lists of sites**

| Command | Arguments | Notes |
|---|---|---|
| `group add <ID>` | `[<SITE>…] [--notes <TEXT>]` | |
| `group edit <ID>` | `[--add <SITE>]… [--remove <SITE>]… [--notes <TEXT>]` | |
| `group list` | `[--json]` | |
| `group show <ID>` | `[--json]` | |
| `group remove <ID>` | `[-y]` | The sites stay |

### 5.3 Plugin commands (`apvm wp`)

**Read-only** (no confirmation, no lock):

| Command | Arguments | Notes |
|---|---|---|
| `wp list` | `TARGETS [<SLUG>… \| --project <NAME>] [--status active\|active-network\|inactive\|must-use\|dropin] [--json] [-j <N>]` | Every plugin per site, variants annotated |
| `wp status` | `TARGETS PLUGINS [--expect installed\|absent\|active\|active-network\|inactive] [--version <VERSION>] [--variant <VARIANT>] [--ownership] [--json] [-j <N>]` | One row per site × requested plugin, absent ones included. With `--expect`/`--version`/`--variant` it is an assertion (§5.5, §6.15) |
| `wp is-installed` | `TARGETS <SLUG>… [--quiet] [--json]` | = `status --expect installed` |
| `wp is-active` | `TARGETS <SLUG>… [--network] [--quiet] [--json]` | = `status --expect active` (`active-network` with `--network`) |

**Mutating** (plan → confirm when needed → apply → verify → report):

| Command | Arguments | Notes |
|---|---|---|
| `wp install` | `TARGETS SOURCES [--if-installed fail\|skip\|replace\|reinstall \| --force] [--activate [--network] [--keep-siblings]] [--no-health-check] RUN` | **SOURCES** (at least one): `[<PLUGIN> <GIT_REF> [--variants <VARIANT>[,<VARIANT>]] [-v\|--ver <VERSION>] [--no-cache]]` and/or `[--zip <FILE>]…`. Default `--if-installed fail`; `--force` = `replace` |
| `wp activate` | `TARGETS PLUGINS [--network] [--keep-siblings] [--no-health-check] RUN` | An active sibling is switched off first unless `--keep-siblings` |
| `wp deactivate` | `TARGETS PLUGINS [--no-load] RUN` | `--network` is derived from the observed state; `--no-load` is for a plugin that crashes WordPress |
| `wp uninstall` | `TARGETS PLUGINS RUN` | Deactivate → uninstall: data + files |
| `wp reset` | `TARGETS PLUGINS [--activate] RUN` | Deactivate → `uninstall --skip-delete`: data only, files kept [→ activate] |
| `wp delete` | `TARGETS PLUGINS RUN` | Deactivate → delete: files only, data kept |
| `wp ownership` | `TARGETS PLUGINS [--fix] RUN` | Check owner/group/group bits; `--fix` repairs (§6.8) |
| `wp run` | `TARGETS [--timeout <SECS>] [-y] [--json] [-j <N>] [--lock-wait <SECS>] -- <WP-CLI ARGS>…` | Any WP-CLI command, safely (§6.12) |

Confirmation is asked for `uninstall`, `reset`, `delete`, `install` with `replace`/`reinstall`, any variant switch, `ownership --fix` that needs root, and `wp run` commands that are not read-only.

### 5.4 Existing commands touched

| Command | Change |
|---|---|
| `apvm info <PLUGIN>` | Also shows each variant's slug and marker |
| `apvm build` | Behavior unchanged; its source arguments become the shared `SourceArgs` used by `wp install` |
| `apvm list`, `cache`, `config`, `skill`, `update`, `uninstall` | Unchanged |

### 5.5 Exit codes

| Code | When |
|---|---|
| 0 | Every item done / no-op / skipped; `status`/`is-*` expectations all met; a declined confirmation ("Aborted. Nothing changed."); `wp run` with one target returns WP-CLI's own code |
| 1 | Any item blocked / failed / unknown; an expectation definitively not met; any other error |
| 2 | Invalid arguments (clap) |
| 3 | `status` / `is-*` only: some target could not be checked (unreachable, probe failed, unknown variant) and nothing was definitively not met |

### 5.6 What the user sees

```
$ apvm wp install backwpup 123 --variants pro-en -g qa --activate --force
Source   backwpup pro-en · PR #123 (built after confirmation; cache used when possible)
Targets  3 sites (group qa)

Plan
  SITE     PLUGIN        NOW                        ACTIONS
  shop     backwpup-pro  —  (backwpup free: active) switch: deactivate backwpup → install → activate
  blog     backwpup-pro  pro-de 5.7.6 · active      replace files pro-de → pro-en (data kept, stays active)
  ftpsite  backwpup-pro  —                          install → activate  (as ftpuser:www-data)
  note: add --keep-siblings to keep backwpup (free) active next to pro on shop

Apply 3 changes on 3 sites? [y/N] y
Building backwpup pro-en from PR #123 … cache hit (a612005)

Results
  ✓ shop     backwpup-pro  backwpup deactivated · installed pro-en 5.7.7 · activated · boots · www-data:www-data
  ✓ blog     backwpup-pro  replaced pro-de 5.7.6 → pro-en 5.7.7 · active · boots
  ✓ ftpsite  backwpup-pro  installed pro-en 5.7.7 · activated · boots · ftpuser:www-data g+rw
3 done · log: ~/.apvm/logs/sites/2026-10-09T10-22-31Z-install.log
```

```
$ apvm wp status -g qa backwpup-pro --expect active --variant pro-en --version 5.7.7
  SITE     PLUGIN        STATE     VERSION  VARIANT  CHECK
  shop     backwpup-pro  active    5.7.7    pro-en   ✓
  blog     backwpup-pro  inactive  5.7.7    pro-en   ✗ expected active
  ftpsite  backwpup-pro  ?         ?        ?        ? ssh_timeout
exit 1

$ apvm wp is-active -s shop backwpup-pro --quiet && echo "pro is active"
$ apvm wp run -s shop -- option get siteurl          # one site: raw output, WP-CLI's exit code
https://shop.example.com
$ apvm wp run -g qa -- cache flush                   # not read-only → plan + confirmation
Run `wp cache flush` on 3 sites (may write)? [y/N]
```

---

## 6. Algorithms

### 6.1 Remote execution contract

1. **Typed scripts.** A `Script` is built from compile-time literals and quoted values (POSIX single quotes, `'` → `'\''`). Every command in it gets `</dev/null` so nothing consumes the script stream.
2. **Framing.**
   `printf '%s\n' '<nonce>'; cd / && <identity> <argv…> </dev/null; rc=$?; [ "$rc" -eq 255 ] && rc=254; exit "$rc"`
   - Only stdout after the per-call random nonce is parsed, which ignores `.bashrc`/MOTD noise.
   - 255→254 makes a real 255 always mean an SSH failure; 254 = remote exit 255 (PHP fatal).
   - `cd /` keeps a project `wp-cli.yml` out of play; `--path` and `--url` are always explicit.
3. **Delivery** (D3):
   - **Stdin mode** (everything except uploads): remote command `sh -s`; script bytes on stdin, then EOF. The login shell sees only `sh -s`.
   - **Upload mode:** stdin carries the zip, so the script is the remote command `sh -c '<script>'`. A distinct `UploadScript` type accepts only allowlisted values (no NUL/CR/LF/backslash), whose single-quoting is portable across sh/bash/zsh/fish/csh.
4. **Identity prefix** (§6.2): empty, `sudo -n -u U -g G --`, `sudo -n -u U --`, or `runuser -u U -g G --` (root login without sudo). Root-level commands form a **closed enum**: `stat`, `chown -R -P -h` (§6.8).
5. **Limits:** stdout 8 MiB, stderr 1 MiB (with a truncation flag); `kill_on_drop`.
6. **Timeouts:** `ConnectTimeout` 15 s; `ServerAliveInterval=15`, `ServerAliveCountMax=4`; per command 300 s, install 900 s, `wp run --timeout`. A timed-out mutation → re-read → done/failed/unknown.
7. **Retries:** reads only, on transport failures only, twice (1 s, 3 s). Mutations never.
8. **Multiplexing (Unix):** `ControlMaster=auto`, `ControlPath=<control_dir>/%C`, `ControlPersist=60s`, `control_dir` `0700`. Off on Windows, for paths > 100 bytes, or with `multiplex: false`.
9. **Concurrency:** `--jobs` (4) globally plus a per-server semaphore (4) below sshd's `MaxSessions 10`.

### 6.2 Probe and identity resolution (read-only)

| Step | Runs as | What (one round trip each) |
|---|---|---|
| H — host | login user | `id -un`, `id -u`, `id -Gn`, `$SHELL`; `command -v sudo runuser sha256sum shasum openssl`; `sudo -n true`; `stat -c '%u %g %U %G %a'` (BSD `stat -f` fallback; root `stat` on EACCES) of `<path>/wp-content/plugins`, else `<path>`; `command -v <wp_cli>` |
| I — identity | — (pure) | the table below; then a probe of the chosen prefix with `true` |
| W — WordPress | identity | `wp eval` of a constant snippet (`--skip-plugins --skip-themes`): `WP_CLI_VERSION`, `PHP_VERSION`, WP version, `is_multisite()`, `WP_PLUGIN_DIR`, `WP_CONTENT_DIR`, `siteurl`, `home`, `DISALLOW_FILE_MODS`; failures classified |
| O — ownership | identity | `realpath` + `stat` of `WP_PLUGIN_DIR` → reference `(U, G, mode)`; a differing candidate → switch identity and warn |
| C — capabilities | identity | `test -w` on the plugins dir and `<content>/upgrade`; `df -Pk` of `remote_tmp_dir` and `WP_CONTENT_DIR`; checksum tool; multisite flag; `siteurl` ≠ `url` → warning |

Identity table (`U:G` = owner:group of `WP_PLUGIN_DIR`, or the explicit `owner`; `L` = login user, `Lg` = its primary group):

| `run_as` | Condition | Prefix | Group handling |
|---|---|---|---|
| `auto` | `U == L`, `G == Lg` | none | correct at creation |
| `auto` | `U == L`, `G ≠ Lg` | `sudo -n -u L -g G` if allowed, else none | if no sudo: `chgrp` repair as L (needs L ∈ G) |
| `auto` | `U ≠ L`, `U ≠ root` | `sudo -n -u U -g G` (allowed when U ∈ G or a Runas group permits it) | else `sudo -n -u U` + repair |
| `auto` | `U ≠ L`, `L == root`, no sudo | `runuser -u U -g G` | correct at creation |
| `auto` | sudo unusable | — | blocked: `sudo_*` code + exact fix |
| `auto` | `U == root` | — | blocked: `owner_is_root` |
| `login` | — | none (as L) | owner/group repaired afterwards (needs sudo); removal of foreign-owned files blocked |
| `<user>` | — | as `auto` with `U := <user>` | as `auto` |
| any → root | — | needs `allow_root: true`; adds `--allow-root` | — |

Recommended sudoers (documented). Root is only needed for repairs, and `ownership: check` needs none:

```
deploy ALL=(www-data) NOPASSWD: ALL                       # sites owned by www-data
deploy ALL=(ftpuser : www-data) NOPASSWD: ALL             # FTP-owned sites; plain (ftpuser) suffices if ftpuser ∈ www-data
deploy ALL=(root) NOPASSWD: /usr/bin/chown, /usr/bin/stat # repairs only — chown as root ≈ root
```

### 6.3 Planner — pure state machine

Inputs: operation, selection, observed rows (+ detected variants), facts, packages, `PluginHints`, options. Output: `SitePlan { items, warnings }`.

| Op ↓ / observed → | absent | inactive | active | active-network | must-use / dropin |
|---|---|---|---|---|---|
| `activate` | blocked | [Switch] Activate → verify → health | no-op | no-op | blocked |
| `deactivate` | skipped | no-op | Deactivate → verify | Deactivate `--network` → verify | blocked |
| `uninstall` | skipped | Own → Uninstall → verify absent | P1 Deactivate · P2 Own → Uninstall | same, `--network` | blocked |
| `reset` | blocked | Uninstall `--skip-delete` → verify inactive [→ Activate] | P1 Deactivate · P2 Uninstall `--skip-delete` [→ Activate] | same | blocked |
| `delete` | skipped | Own → Delete → verify absent | P1 Deactivate · P2 Own → Delete | same | blocked |
| `ownership` | skipped | Check [→ Repair → Check] | same | same | blocked |

`Own` = owner/group check + repair before removal (§6.8). `[Switch]` = deactivate an active sibling first (§6.9); skipped with `--keep-siblings`. Install policies: §6.7.

Ordering within a site:
1. All pass-1 deactivations (including switch deactivations), in reverse dependency order.
2. All pass-2 removals.
3. Installs.
4. Activations, in `Requires Plugins` topological order (a cycle is an error).

Rules:
- **Family rules:** §6.9.
- **Failure propagation:** a failed step stops its item. A failed pass-1 deactivation also blocks the pass-2 steps of family members **in the same batch**, because the confirmed plan assumed it inactive. Unrelated items continue, and a sibling is never blocked on its own.
- **Flags:** `--network` is derived from the observed status. Multisite mutations stay blocked until P10.
- **Typestate:** `BatchPlan<Unbound>` → `bind(packages)` → `BatchPlan<Bound>`; only `Bound` can be applied (compile-time).

### 6.4 Executor and verification

- **One step = one remote command + one predicate**, checked by re-reading state:
  - `StatusIs`/`Absent`/`PresentInactive`/`Version` via `wp plugin list --format=json --fields=name,status,version,file,title --skip-update-check --skip-plugins --skip-themes`.
  - `Owned` via `find` (§6.8).
  - `Boots` via `wp option get siteurl` with plugins loaded.
- **Drift:** the read verifying step *i* is also step *i+1*'s precondition; a mismatch with the plan → `state_drifted`.
- **State decides the outcome:** exit 0 with `^Error:`, or a non-zero exit with the expected state, is a warning.
- **Always-run:** lock release and staging cleanup run on every path.
- **Events:** each command, exit code and capped output is emitted as an event; `LogFileReporter` writes the run log.

### 6.5 Two-phase removal

```
Pass 0  read state; block must-use/dropin; family notes (§6.9)
Pass 1  each active target (and each sibling being switched off): wp plugin deactivate <slug> [--network]  → verify inactive
        failure ⇒ that item (and family members in this batch) blocked; nothing destructive ran for them
Pass 2  each target: [Own] → wp plugin uninstall <slug> [--skip-delete] | wp plugin delete <slug>  → verify
```

The uninstall process boots WordPress with only the still-active plugins, so guards like `class_exists(\BackWPup::class)` let cleanup run. `--deactivate`/`--uninstall` do both in **one** process that already loaded the plugin, so the guard skips cleanup.

### 6.6 Staging (the upload step)

Every package is uploaded before WordPress sees it. Lifecycle per site run:

```
Created ──► Uploading(package-n) ──► Verified(package-n) ──► … ──► Consumed by `wp plugin install` ──► Removed
   │ space check fails ⇒ insufficient_space        │ checksum mismatch ⇒ upload_corrupted
   └──────────────────────── any failure / success ─┴──► Removed (always; failure = warning)
```

| Aspect | Design |
|---|---|
| Where | `<remote_tmp_dir>/apvm.XXXXXXXXXX` (default `/tmp`), created by `mktemp -d` **as the identity**, so it is owned by U:G with mode `0700`, readable by WP-CLI running as the same identity and by nobody else |
| Why not `wp-content/upgrade` | WordPress empties `upgrade/` on every extraction, and an interrupted run would leave junk inside the site |
| Names | `package-1.zip`, `package-2.zip`, … Fixed names keep user filenames out of remote commands; WordPress names its working dir after the basename, while the slug comes from the zip root (validated locally) |
| Upload | The local file is streamed over the SSH channel's stdin into `tee -- <dir>/package-n.zip >/dev/null`, run as the identity (upload mode, §6.1). No scp/sftp needed; it rides the multiplexed connection |
| Integrity | Remote SHA-256 (`sha256sum` \| `shasum -a 256` \| `openssl dgst -sha256`) must equal the local one; with no tool, size equality + a warning |
| Space | `df -Pk`: free tmp ≥ zip size; free `WP_CONTENT_DIR` ≥ 2 × uncompressed size (extract + copy fallback), else `insufficient_space` before uploading |
| Data flow | local zip → staging → `wp plugin install <staging>/package-n.zip` → WordPress unzips into `wp-content/upgrade/package-n/` → moves to `wp-content/plugins/<slug>/` |
| Cleanup | `rm -rf -- <dir>` as the identity, only if `<dir>` matches `^<remote_tmp_dir>/apvm\.[A-Za-z0-9]{10}$` and was created by this run |
| Stale sweep | On creation: remove `apvm.*` dirs in `remote_tmp_dir` owned by the identity and older than 24 h (left by killed runs), by exact name pattern |
| Scope | One staging dir per site run, holding all of that site's packages; per-server reuse is enhancement E3 |

### 6.7 Install pipeline

```
sources: --zip FILE            → inspect locally (§6.10)        → Ready(PluginPackage)
         PLUGIN GIT_REF --variants → declared slug(s) (hints)   → Pending{slug, variant}
plan (preflight per site) → confirm → bind (build once into a private temp dir; inspect; slug == declared)
per site (lock held), per package, sequential:
  Stage (§6.6) → policy steps (below) → Install `wp plugin install <staged> [--force]` (never --activate)
  → Verify (row present, file == <slug>/<main.php>, version == header) → Own (§6.8)
  → [Switch (§6.9)] → Activate (if --activate, or it was active before a reinstall) → verify → Boots
  → Cleanup (§6.6)
```

| `--if-installed` | absent | installed, inactive | installed, active |
|---|---|---|---|
| `fail` (default) | install | blocked | blocked |
| `skip` | install | skipped | skipped |
| `replace` / `--force` | install | `install --force` (files replaced, data kept) | `install --force`, stays active (no activation hook, like an update) |
| `reinstall` | install | Uninstall → install | Deactivate → Uninstall → install → Activate |

Plan warnings: `Requires PHP` / `Requires at least` above the site's; the same slug from two sources; replacing one variant with another (§6.9).

### 6.8 Ownership, group and group access

- **Reference** `R = (U, G, P)`: owner, group and mode of `WP_PLUGIN_DIR` (`owner: auto`), or an explicit `user[:group]` with `P` still taken from the plugins dir.
- **Expected** for every entry under the target `<plugins_dir>/<slug>` (or `<plugins_dir>/<file>` for single-file plugins):
  - owner U and group G;
  - dirs carry the group r/w/x bits `P` grants, plus setgid when `P` has it;
  - files carry the group r/w bits `P` grants.
  - Bits are only ever added; "other" bits are never touched; `o+w` entries are reported.
- **Check** (one script, as the identity, numeric ids, first 20 offenders listed):
  - `find T \( ! -user <uid> -o ! -group <gid> \) -print | head -n 20`
  - `find T -type d ! -perm -<dir bits> -print | head -n 20`
  - `find T -type f ! -perm -<file bits> -print | head -n 20`
  - `find T -perm -0002 ! -type l -print | head -n 20`
- **When:**
  - after install/replace/reinstall: full check;
  - before uninstall/delete/reinstall: owner/group only, since a foreign-owned tree cannot be deleted by the identity;
  - `apvm wp ownership`: on demand.
- **Repair ladder** (`ownership: mirror`; each rung re-checked, stop when clean):

| Rung | When | Command | Privilege |
|---|---|---|---|
| 1 | Only the group differs and the identity owns the files and is in G (or runs with egid G) | `chgrp -R -h -- <gid> T` | none |
| 2 | Owner differs (or rung 1 impossible) | `cd -- '<plugins_dir>' && [ -d '<slug>' ] && [ ! -L '<slug>' ] && sudo -n chown -R -P -h -- <uid>:<gid> './<slug>'` | root (closed enum) |
| 3 | Group bits missing | `find T -type d -exec chmod g+<bits>[s] {} +` and `find T -type f -exec chmod g+<bits> {} +` (symlinks excluded by `-type`) | none (identity owns T after rung 2) |
| — | Still mismatched | item `failed` with `ownership_unfixable`, listing the entries; the plugin stays installed | — |

- **Guards:**
  - `<plugins_dir>` is the probe's `realpath`, and `<slug>` is allowlisted, so the target is always a direct child, never the plugins dir or `/`.
  - `-P -h` means symlinks are never followed, so a plugin running as `www-data` cannot plant `x -> /etc/sudoers` and get it chowned (regression test in P6.3).
  - Root never deletes.
- **Other policies:** `ownership: check` reports and never repairs (zero root commands); `off` skips everything.

### 6.9 Variants and plugin families

```
Family "backwpup"
  slug backwpup       ← variant free
  slug backwpup-pro   ← variant pro-de  marker inc/Pro/License/License.php contains "https://backwpup.de/"
                      ← variant pro-en  marker inc/Pro/License/License.php contains "https://backwpup.com/"
  conflicts when active together: yes  (shared main-file guard: only one runs per request)
  shared data: yes                     (each uninstall routine cleans data both use)
```

**Principle:** free and pro are **independent plugins**. apvm never blocks installing, activating, uninstalling, resetting or deleting one because of the other; QA can keep both installed or both active to check for errors. apvm only:
1. refuses what is physically impossible (two variants of one slug);
2. picks a safe default when activating next to an active sibling (switch), with an opt-out;
3. states in the plan what BackWPup's own guards will do.

**Where the knowledge lives:**
1. Builders (core) declare the slug per variant, marker files, `conflicts_when_active` and `shared_data`.
2. `ProjectRegistry::plugin_hints()` turns them into `apvm_sites::PluginHints`.
3. That is passed to `SiteManager`. `apvm-sites` holds no plugin-specific code, so a future multi-variant project only needs its builder.

**Rules:**

| Situation | Behavior |
|---|---|
| `install backwpup <ref>` without `--variants` | Input error listing `free, pro-de, pro-en` (the builder default holds two plugins; no silent choice) |
| `--variants free,pro-en` | Both built in one build and installed side by side (`backwpup`, `backwpup-pro`) |
| `--variants pro-de,pro-en` | `variant_conflict`: both install to `backwpup-pro` |
| pro-de on the site, pro-en requested | A replacement: `--if-installed` applies (`--force` is fine); plan: "replace pro-de 5.7.6 → pro-en 5.7.7" |
| Activate pro while free is active (default) | **Switch:** deactivate free (own process, verified) → activate pro (own process, verified) → boot check; confirmation needed. Separate processes are what let pro's activation routine run (§3.1) |
| Activate pro while free is active, `--keep-siblings` | Plain activation in its own process; both end up active. Plan note: only one runs per request (shared guard), and the second plugin's activation routine does not run, exactly as when done from the dashboard. The boot check still runs |
| Activate free and pro in one command | Needs `--keep-siblings` (else `variant_conflict`); activated in the given order |
| `install … --activate` with free + pro, or pro next to an active free | Same two rules: switch by default, both with `--keep-siblings` |
| Uninstall / reset pro while free is **active** | Allowed. Two-phase for pro itself. Plan note: BackWPup's uninstall guard sees free loaded and keeps the shared data (intended by BackWPup) |
| Uninstall / reset pro while free is installed but **inactive** | Allowed. Plan note: the data shared with free is removed |
| Uninstall both, one or both active | Pass 1 deactivates every active target, pass 2 uninstalls: both cleanups run |
| Failed deactivation of free while uninstalling both | Pro's pass 2 in **that batch** is blocked: the confirmed plan assumed free inactive. Running pro's uninstall alone afterwards is never blocked |
| `deactivate`, `delete` | Never touch or depend on siblings |
| `--project backwpup` | Every installed member on each site |
| `list` / `status` | Variant shown (`backwpup-pro  BackWPup Pro (pro-en)`); marker not found → `pro-?` (covered by a core test) |
| Any zip | The same markers are checked inside the zip, so a manually downloaded BackWPup zip gets the same rules |

**Detection on a site:** for each installed family slug with marked variants, one script (as the identity) runs `grep -F -q -- '<needle>' '<plugins_dir>/<slug>/<marker path>'` per variant and prints the match. Marker paths are allowlisted relative paths.

### 6.10 Package inspection (local)

| Rule | Detail |
|---|---|
| File | Regular `.zip`, ≤ 256 MiB, opens |
| Bomb limits | ≤ 100 000 entries; uncompressed total (central directory, `by_index_raw`) ≤ 1 GiB |
| Paths | `enclosed_name()` (no absolute, `..`, NUL); backslashes rejected; symlink entries warned |
| Structure | Exactly one top-level dir (ignoring `__MACOSX/`, `.DS_Store`); its name passes the slug allowlist |
| Plugin | A `<root>/*.php` with `Plugin Name:` in its first 8 KiB; main = `<root>/<root>.php` if headered, else the first headered (sorted); several → warning |
| Headers | WordPress `get_file_data` semantics on bytes: Name, Version, Requires at least, Requires PHP, Network, Requires Plugins, Text Domain |
| Variant | `PluginHints` markers checked inside the zip |
| Identity | SHA-256 + size |

### 6.11 Multisite (P10)

- **Activation:** network-only plugins (`Network: true`, from the package or a constant headers snippet) are activated with `--network`; `--network` on a single site is an input error.
- **Before destructive steps:** a constant snippet lists per-subsite activations (`get_sites()` × `active_plugins`, with URLs). Pass 1 also deactivates on each subsite (`--url=<subsite>`), verified by re-running the snippet.

### 6.12 `apvm wp run` — WP-CLI passthrough

| Aspect | Design |
|---|---|
| Execution | The arguments become `wp` argv elements, quoted, in a stdin-delivered script: no shell interpretation of `; \| & $() * ~`, any byte but NUL allowed. Runs as the site identity (owner:group), with apvm's `--path` and the site's `--url` |
| Allowed overrides | `--url=…` (subsite targeting), `--skip-plugins[=…]`, `--skip-themes`, `--user=` (WP user), `--format=…`, `--debug`, `--quiet` |
| Refused params | `--path` (apvm owns it), `--ssh`, `--http` (would hop elsewhere), `--allow-root` (identity policy), `--prompt` (interactive), a leading `@alias` |
| Refused commands | `shell`, `db cli` (interactive); `server` (long-running); `cli update`, `package …` (WP-CLI itself, not the site); `plugin uninstall … --deactivate` and `plugin deactivate … --uninstall` (break two-phase removal: the error points to `apvm wp uninstall`) |
| Classification | `read-only` = allowlisted command paths (e.g. `plugin list/get/status/is-installed/is-active/path`, `theme list/get/status`, `option get/list/pluck`, `core version/is-installed/check-update/verify-checksums`, `config get/list/has`, `user list/get`, `post list/get`, `post meta get/list`, `term list/get`, `comment list/get/count`, `site list`, `cron event list`, `cron schedule list`, `db size/tables`, `transient get`, `rewrite list`, `role list`, `language core/plugin/theme list`, `cli version/info`, `help`; `search-replace … --dry-run`). Everything else, including `eval`, is `may-write` |
| Confirmation | `may-write` → plan + confirmation (CLI) / `allowWrites: true` (NAPI); site lock held |
| Output | One target: raw stdout/stderr, apvm exits with WP-CLI's status (scripting: `apvm wp run -s shop -- plugin is-active x && …`). Several targets: a header block per site (or `--json` with `{site, exit, stdout, stderr, truncated}`); exit 0 only if all succeeded |
| Scope | One WP-CLI command per invocation; arguments are taken verbatim, so `run -- plugin delete x` really deletes (documented: use the dedicated verbs for safe lifecycle work) |
| Nature | The classifier is a guard against mistakes, not a sandbox: `eval` runs arbitrary PHP as the identity by design |

### 6.13 Site lock

- **Lock:** `mkdir <remote_tmp_dir>/apvm-lock-<first 16 hex of SHA-256(path)>` (atomic on POSIX), as the identity, holding a `holder` file: local `user@host`, pid, operation, start time (RFC 3339).
- **Held** for every mutating operation (install, activate, deactivate, uninstall, reset, delete, `ownership --fix`, `wp run` may-write). Reads never lock.
- **Busy:** wait up to `--lock-wait` (60 s) while printing the holder, then `site_locked`.
- **Stale:** a lock older than 30 min (longer than any operation) is removed with a warning; a lock owned by a different user (sticky `/tmp`) is reported instead.
- **Release** on every exit path; the path is validated before `rm -rf`.
- **Manual:** `apvm site unlock <ID>` shows the holder and removes a stuck lock (as the identity) after confirmation.

### 6.14 Recovery

- **Activation breaks the site:** `health_check_failed`, and the output prints `apvm wp deactivate <slug> -s <site> --no-load`. That command runs `--skip-plugins=<slug>`, so the deactivation hook does not run, which the output also says.
- **Reads always work:** they use `--skip-plugins --skip-themes`, so they still run on a broken site.

### 6.15 Status queries (`status`, `is-installed`, `is-active`)

- **Read-only:** never locks, never mutates; transport failures are retried like any read.
- **One read per site:** probe (identity) → `wp plugin list` → marker checks only when `--variant` is given or a family slug is shown → ownership check only with `--ownership`.
- **Rows:** one per site × requested plugin, **absent ones included** (unlike `list`); `--project`/`--all` expand per site.
- **Expectations:**

| `--expect` | Met when the observed status is |
|---|---|
| `installed` | anything but absent |
| `absent` | absent |
| `active` | `active` or `active-network` |
| `active-network` | `active-network` |
| `inactive` | `inactive` |

- `--version` compares the exact version string; `--variant` compares the detected variant (unknown → undetermined).
- **Per row:** met / not met / undetermined. **Exit:** 1 if any row is not met, else 3 if any is undetermined, else 0 (§5.5). An unreachable site is never reported as "not installed".
- **Output:** a table, `--json` rows `{site, plugin, status, version, variant, file, check, code}`; `is-*` with `--quiet` print nothing (for `&&` in scripts).

---

## 7. Data model — `~/.apvm/inventory.json`

```json
{
  "version": 1,
  "defaults": {
    "ssh": { "user": "deploy", "port": 22, "identity_file": "~/.ssh/id_ed25519",
             "connect_timeout_secs": 15, "multiplex": true }
  },
  "servers": [
    { "id": "eu-1", "name": "EU VPS", "host": "203.0.113.10",
      "ssh": { "user": "ubuntu", "identity_file": "~/.ssh/eu1_ed25519" } },
    { "id": "staging-box", "host": "staging" }
  ],
  "sites": [
    { "id": "shop", "server": "eu-1", "path": "/var/www/shop/public", "url": "https://shop.example.com" },
    { "id": "ftpsite", "server": "eu-1", "path": "/home/ftpuser/public_html", "notes": "ftpuser:www-data" },
    { "id": "blog", "server": "eu-1", "path": "/var/www/blog",
      "wp_cli": ["/usr/bin/php8.3", "/opt/wp-cli.phar"], "run_as": "login", "ownership": "check" }
  ],
  "groups": [ { "id": "qa", "sites": ["shop", "ftpsite", "blog"] } ]
}
```

`"host": "staging"` can be an `~/.ssh/config` alias.

| Type | Fields (optional unless **bold**) |
|---|---|
| `SshSettings` (defaults / server) | `user`, `port`, `identity_file`, `connect_timeout_secs`, `known_hosts_file`, `ssh_config_file`, `proxy_jump`, `multiplex`; defaults only: `binary` |
| `Server` | **`id`**, **`host`**, `name`, `ssh`, `notes` |
| `Site` | **`id`**, **`server`**, **`path`**, `url`, `wp_cli` (`["wp"]`), `run_as` (`auto`), `owner` (`auto` = owner:group of `WP_PLUGIN_DIR`), `ownership` (`mirror`), `allow_root` (`false`), `remote_tmp_dir` (`/tmp`), `command_timeout_secs`, `name`, `notes` |
| `Group` | **`id`**, **`sites`**, `notes` |

| Value | Allowlist | Goes to |
|---|---|---|
| ids | `^[a-z0-9][a-z0-9._-]{0,63}$`, unique per kind | local |
| `host` | `[A-Za-z0-9._:-]`, no leading `-`, ≤ 253 | ssh argv |
| `user`, `run_as`, `owner` parts | `[A-Za-z0-9._-]`, no leading `-`, ≤ 64 | ssh argv / remote |
| `port` | 1–65535 | ssh argv |
| `proxy_jump` | comma list of `[user@]host[:port]` | ssh argv |
| `path`, `remote_tmp_dir` | absolute POSIX, no `..`, no NUL/CR/LF/backslash, ≤ 4096 | remote |
| `url` | `http(s)://` host[/path], allowlisted chars | remote |
| `wp_cli` tokens | `[A-Za-z0-9._/+-]` | remote |
| local paths | no NUL/LF; `~` expanded and existence checked at use | ssh argv |
| slug | `^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$` | remote |

File handling:
- **Load:** a missing file is an empty inventory. Parsing uses `deny_unknown_fields`, then full validation, reporting every problem with its location (`sites[1].server: unknown server 'eu-2'`). A newer `version` is read-only.
- **Save:** lock (`inventory.json.lock`, `std::fs::File::lock`) → temp file `0600` → fsync → rename → fsync dir. Edits are read-modify-write under the lock.
- **Refused:** a symlinked path.

---

## 8. Architecture

```
apvm-config   apvm-storage        apvm-sites (NEW: inventory · remote · wp · package · ops · hints)
       \         /                  ▲      ▲
        apvm-core ──────────────────┘      │   core uses: PluginHints (from builders), build → package → install
         ▲     ▲                           │
     apvm(cli)  apvm-napi ─────────────────┘   front ends: args, prompts, rendering, default locations
```

```
crates/sites/src/
  lib.rs, error.rs (Error + stable code()), hints.rs, layout.rs (file/dir *names* only)
  inventory/{mod, model, validate, io, resolve}.rs
  package/{mod, headers, rules}.rs
  remote/{mod, quote, script, identity, transport, capture, local, ssh, classify, mux}.rs
  wp/{mod, command, parse, state, snippets, probe, classify}.rs
  ops/{model, plan, order, apply, verify, ownership, variants, staging, install, list, status, lock, run, runner, report}.rs
  progress.rs (SiteEvent, SiteReporter, Null/Closure/LogFileReporter) · session.rs · manager.rs
crates/core/src/deploy.rs  ·  crates/core/src/build/plugins/*.rs (slugs, markers)  ·  projects/registry.rs (plugin_hints)
crates/cli/src/commands/{server, site, group}.rs · commands/wp/{mod, args, render, list, status, install, activate,
  deactivate, uninstall, reset, delete, ownership, run}.rs · prompt.rs
crates/napi/src/{sites_inventory, sites, sites_types, sites_progress}.rs
tests/e2e/sites/…  ·  tests/support/fake-remote/{fake-ssh, fake-sudo, fake-wp}
```

### 8.1 Library contract

1. **Every capability is an `apvm-sites` API.** It covers `Inventory` (+ io), `SiteManager` (`probe`, `list`, `status`, `plan`/`apply`, `run_wp`, `unlock`), `PluginPackage::inspect`, `wp::classify`, `PluginHints`, and the reporters (including `LogFileReporter`).
2. **Front ends add only** argument parsing, confirmation, rendering, and default locations (`~/.apvm` + `layout` names). The env override helper `inventory::io::env_override()` (`APVM_INVENTORY`) lives in the library, mirroring `config_io::cache_dir_env_override`.
3. **Core uses it now** for two things:
   - `ProjectRegistry::plugin_hints()` (family/variant knowledge comes from builders);
   - `Apvm::build_package` + `Apvm::install_build` (plan → build → bind → apply, no prompts), used by NAPI `installBuild` and by any future core feature. The CLI uses the stepwise API so it can confirm before building.
4. **No defaults in libraries**, following the existing rule: inventory path, `control_dir` and log path are always passed in.
5. **Growth-safe API:** events, outcomes, codes and requests are `#[non_exhaustive]`; boxed-future `Transport` trait (dyn-compatible, no `async-trait`).

```rust
pub struct SiteManager { /* Inventory + PluginHints + SessionOptions { control_dir, jobs, per_server, timeouts, lock_wait } */ }
impl SiteManager {
    pub fn new(inventory: Inventory, hints: PluginHints, options: SessionOptions) -> Self;
    pub async fn probe(&self, t: &Targets, r: &dyn SiteReporter) -> ProbeReport;
    pub async fn list(&self, t: &Targets, f: &ListFilter, r: &dyn SiteReporter) -> ListReport;
    pub async fn status(&self, t: &Targets, p: &PluginSelection, e: &Expectations, r: &dyn SiteReporter) -> StatusReport;
    pub async fn unlock(&self, site: &SiteId) -> Result<Option<LockHolder>>;
    pub async fn plan(&self, req: OperationRequest, t: &Targets, r: &dyn SiteReporter) -> Result<BatchPlan<Unbound>>;
    pub async fn apply(&self, plan: BatchPlan<Bound>, r: &dyn SiteReporter) -> Report;
    pub async fn run_wp(&self, t: &Targets, run: WpRun, r: &dyn SiteReporter) -> Result<RunReport>;
}
#[non_exhaustive]
pub enum OperationRequest {
    Install { sources: Vec<PackageSource>, policy: IfInstalled, activate: Option<Activation>, health_check: bool },
    Activate { plugins: PluginSelection, activation: Activation, health_check: bool },
    Deactivate { plugins: PluginSelection, no_load: bool },
    Uninstall { plugins: PluginSelection },
    Reset { plugins: PluginSelection, reactivate: bool },
    Delete { plugins: PluginSelection },
    Ownership { plugins: PluginSelection, fix: bool },
}
pub struct Activation { pub network: bool, pub keep_siblings: bool }
pub struct Expectations { pub state: Option<ExpectedState>, pub version: Option<String>, pub variant: Option<String>, pub ownership: bool }
pub fn classify(args: &[String]) -> WpClassification; // ReadOnly | MayWrite | Refused(reason)
// core
impl Apvm {
    pub fn plugin_hints(&self) -> PluginHints;
    pub fn declared_packages(&self, project: &str, variants: &[String]) -> Result<Vec<PackageSource>>;
    pub async fn build_package(&self, req: BuildPackageRequest, r: &dyn ProgressReporter) -> Result<BuiltPackages>;
    pub async fn install_build(&self, sites: &SiteManager, req: InstallBuildRequest,
                               b: &dyn ProgressReporter, s: &dyn SiteReporter) -> Result<Report>;
}
```

---

## 9. Reliability rules

1. **State is the success criterion:** every mutation is followed by a read.
2. **Parsing:** reads parse framed `--format=json`. Unparseable output → `wp_command_failed` with an `escape_debug` excerpt; notices never read as "not installed".
3. **Matching:** rows match by `name == slug`, cross-checked with `file`.
4. **Reads vs mutations:** reads use `--skip-plugins --skip-themes --skip-update-check`; mutations never skip plugins (except the explicit `--no-load`).
5. **No retries:** mutations are never retried; a transport loss → re-read → done/failed/unknown.
6. **One process per step:** steps within a site are sequential and the site lock is held. The lifecycle verbs never use WP-CLI's `--all`, `--exclude`, `--deactivate`, `--uninstall` or `install --activate` (a golden test asserts it).
7. **Uploads:** SHA-256 verified; staging and lock always cleaned up.
8. **Partial failures:** never hide successes; the exit code reflects any failure.

---

## 10. Security model

Threats:

| # | Threat |
|---|---|
| T1 | Untrusted strings from automation (NAPI callers deriving slugs, paths or WP-CLI args from PR titles or webhooks) |
| T2 | A hostile or wrong zip |
| T3 | The sudo/root boundary |
| T4 | Hostile remote output |
| T5 | Secret leakage |
| T6 | MITM |
| T7 | Accidental mass destruction |

| Control | Threats |
|---|---|
| No local shell: argv arrays only | T1 |
| Allowlists in the library (NAPI covered); POSIX quoting; stdin delivery, so the login shell never parses data; `UploadScript` limited to safe values | T1 |
| Lifecycle slugs must be echoed back by `wp plugin list` before any mutation | T1, T7 |
| `wp eval` only with compile-time constant snippets (except `wp run`, where the operator asks for it) | T1 |
| `wp run`: argv-only, refused params/commands, read-only allowlist, confirmation / `allowWrites`, identity-run, lock | T1, T7 |
| Zip rules (§6.10) | T2 |
| `sudo -n` only (never `-s/-i`, `sudo sh`, sudo env tricks); root commands are a closed enum (`stat`, guarded `chown -R -P -h`); root never deletes; `chgrp` without root first; `ownership=check` → zero root | T3 |
| Nonce framing, output caps, strict parsers, `escape_debug` | T4 |
| No stored secrets; inventory and logs `0600`; no secrets in argv | T5 |
| `BatchMode=yes`; the user's known_hosts; `accept-new` only per run; `no` never | T6 |
| Plan + one confirmation; non-TTY needs `-y`; `--dry-run`; empty selection = error; site lock | T7 |

---

## 11. NAPI surface

```ts
class SiteInventory {                                   // sync, file-backed
  static load(path?: string): SiteInventory              // default ~/.apvm/inventory.json | APVM_INVENTORY
  static empty(): SiteInventory
  readonly path: string | null
  defaults(): JsDefaults;  setDefaults(d: JsDefaults): void
  servers(): JsServer[];   upsertServer(s: JsServer): void;  removeServer(id: string, cascade?: boolean): boolean
  sites(): JsSite[];       upsertSite(s: JsSite): void;      removeSite(id: string): boolean
  groups(): JsGroup[];     upsertGroup(g: JsGroup): void;    removeGroup(id: string): boolean
  save(path?: string): string
}
class SiteManager {
  static create(opts?: { inventory?: SiteInventory | string; controlDir?: string; jobs?: number;
                         logFile?: string }): SiteManager          // apvm family hints included
  static classifyWp(args: string[]): JsWpClassification          // 'read-only' | 'may-write' | { refused }
  probe(targets: JsTargets, onEvent?): Promise<JsProbeReport>
  listPlugins(targets: JsTargets, filter?: JsListFilter, onEvent?): Promise<JsListReport>
  status(targets: JsTargets, plugins: JsPluginSelection, opts?: { expect?: JsExpectedState; version?: string;
         variant?: string; ownership?: boolean }, onEvent?): Promise<JsStatusReport>   // report.ok / rows[].check
  isInstalled(site: string, slug: string): Promise<boolean>        // rejects `Undetermined` if it cannot tell
  isActive(site: string, slug: string, opts?: { network?: boolean }): Promise<boolean>
  install(targets, opts: { zips: string[]; ifInstalled?: JsIfInstalled; activate?: boolean; network?: boolean;
          keepSiblings?: boolean; healthCheck?: boolean; dryRun?: boolean }, onEvent?): Promise<JsReport>
  activate(targets, plugins: JsPluginSelection, opts?: { network?: boolean; keepSiblings?: boolean;
           healthCheck?: boolean; dryRun?: boolean }, onEvent?): Promise<JsReport>
  deactivate(…), uninstall(…), reset(…), deletePlugins(…), ownership(targets, plugins, { fix? }): Promise<JsReport>
  runWp(targets, args: string[], opts?: { allowWrites?: boolean; timeoutSecs?: number }, onEvent?): Promise<JsRunReport>
  unlock(site: string): Promise<JsLockHolder | null>
}
apvm.sites(opts?): SiteManager                                    // uses this Apvm's registry hints
apvm.installBuild(opts: { project; gitRef; variants; version?; noCache?; targets; ifInstalled?; activate?;
                  keepSiblings?; dryRun? }, onBuild?, onSite?): Promise<JsReport>
```

- **Resolve vs reject:** promises **resolve with a report** whenever work ran (per-item `outcome`/`code`). They **reject** only on bad input or an invalid inventory (`err.code` `InvalidArg` / `InventoryInvalid` / `ConfirmationRequired` for `runWp` may-write without `allowWrites` / `Undetermined` for `isInstalled`/`isActive` when the site can't be checked), via the `spawn_future_with_callback` route `ApvmCache` already uses.
- **No prompts:** a lifecycle method call *is* the intent, and `dryRun` returns the plan.
- **Enums:** `#[napi(string_enum)]`, runtime string enums per `package.json`.
- **Generated files:** `index.js`, `index.d.ts` and `*.node` stay CI-produced.

---

## 12. Testing strategy

| Layer | What | Runs |
|---|---|---|
| Unit | Validators, quoting, script goldens (both delivery modes), ssh argv goldens, classifiers (ssh errors, WP-CLI commands), parsers, planner tables, identity table, ownership ladder, variant rules, status expectations and exit-code precedence, zip rules, headers | always |
| Transport | Real `sh -s` / `sh -c` round trips of adversarial strings (Unix); capture limits; timeouts; 20 MB stdin streaming | always (Unix) |
| Executor | `FakeTransport`: two-pass order, variant switch order, drift, transport loss, partial failures, lock + cleanup on every path | always |
| Hermetic process E2E | `fake-ssh` (runs the remote command locally), `fake-sudo`, `fake-wp` (POSIX emulator over a state dir), wired via `defaults.ssh.binary` + `PATH`; used by `apvm-sites`, CLI (`Sandbox`), NAPI vitest | always (Unix) |
| Docker E2E | Real WordPress + MariaDB + sshd + sudo + WP-CLI 2.12.0 (see below); fixture plugins zipped at test time; gated by `APVM_E2E_SITES=1` | CI Linux job + local opt-in |
| Network build E2E | `apvm wp install <PLUGIN> <REF>` end to end; gated by `APVM_E2E_BUILD=1` (like `crates/core/tests/build_e2e.rs`) | manual / optional CI |

Docker E2E setup:
- **Users:**
  - `deploy`: NOPASSWD sudo.
  - `plain`: no sudo.
  - `pwsudo`: sudo with a password.
  - `fishy`: fish login shell.
  - `noisy`: `.bashrc` prints to stdout.
  - `ftpuser`: no sudo, member of `www-data`.
- **Sites:**
  - `single`: `www-data:www-data`.
  - `multi`: a multisite network, `www-data`.
  - `ftpsite`: `ftpuser:www-data`, dirs `2775`, files `0664`.

Fixture plugins:

| Fixture | Purpose |
|---|---|
| `basic` | Activation sets an option, uninstall removes it |
| `guarded` + `guarded-pro` | BackWPup-style family: shared main-file guard and shared data. `guarded-pro` has two variants told apart by a marker file; tests inject `PluginHints`. Used for coexistence (both installed, both active), switch, and independent removal |
| `network-only` | `Network: true` activation |
| `parent` / `child` | `Requires Plugins` ordering |
| `fatal-activate` | Activation fails |
| `fatal-load` | Site breaks after activation (health check, recovery) |
| `writes-content` | Activation writes into `wp-content/` (side-effect ownership) |

Rules: `APVM_CACHE_DIR` and `APVM_INVENTORY` point at temp locations; the full validation suite runs every phase; `npm test` + `npm run typecheck` run when napi changes; existing tests are never edited to pass.

---

## 13. Stack

| Item | Version | Notes |
|---|---|---|
| Rust | edition 2024, MSRV 1.97.0; local 1.99.0 | `std::fs::File::lock` |
| tokio | 1 (1.53.2) | `process`, `time`, `sync` |
| serde / serde_json | 1 (1.0.229 / 1.0.151) | |
| thiserror | 2 (2.0.21) | |
| tracing | 0.1 (0.1.44) | |
| zip | 8, `deflate` (8.6.0) | `by_index_raw`, `enclosed_name` |
| sha2 | 0.11 (0.11.0) | |
| tempfile | 3 (3.27.0) | |
| which | 8 (8.0.6) | |
| clap | 4 (4.6.7) | trailing `--` args for `wp run` |
| indicatif | 0.18 (0.18.6) | `MultiProgress` |
| napi / napi-derive / napi-build | 3 (3.14.2) / 3.6.12 / 2 | |
| @napi-rs/cli · vitest · typescript | 3.10.8 · 5.0.3 · 7.0.2 | |
| **New crates.io deps** | **none** | `apvm-sites` is a workspace crate in the root `Cargo.toml` |
| Remote | POSIX `sh`, coreutils/findutils (`mktemp`, `tee`, `stat`, `find`, `chown`, `chgrp`, `chmod`, `df`), WP-CLI ≥ the floor P0 confirms (E2E pins 2.12.0), `sudo` when owner/group ≠ the SSH user's, optional `runuser` | documented |
| Local | OpenSSH client (≥ 7.6 for `--trust-new-host-key`) | Windows 10+ ships one |
| E2E | Docker + Compose v2; `wordpress:*-php8.x-apache`, `mariadb:*-lts`, pinned by digest in P0 at the then-latest stable tags | |

---

## 14. Phases

**Definition of done (every phase):** its **Goal** is met, its **Tests** exist and pass, and it:
- passes the full validation suite: `cargo fmt --check`, `clippy -D warnings`, `cargo test`, `cargo doc --no-deps`, plus `npm test` + `npm run typecheck` when napi changes;
- updates `SKILL.md` + `references/sites.md` in the same change when it touches the CLI surface (CLAUDE.md);
- documents every public item.

Phase-specific extra conditions appear as **Done when**.

Tracks: **lib** = `apvm-sites`/core, **cli**, **napi**.

### P0 — E2E harness and ground truth
- **Goal:** a reproducible WordPress-over-SSH target (users, sites and fixtures of §12), with every §3.5 item verified.
- **Changes:**
  - `tests/e2e/sites/{compose.yaml, target/Dockerfile, target/entrypoint.sh, fixtures/*.php, verify.sh, README.md}`;
  - `.github/workflows/ci.yml` job `e2e-sites` (`compose up --wait` + `verify.sh`);
  - §3.5 results recorded in this plan.
- **Done when:** the harness is healthy locally and in CI; `verify.sh` passes, or each deviation is recorded and the plan adjusted.
- **Depends:** — · **Parallel:** P1.x, P2.x, P3.1

### P1 — Foundations (lib)

#### P1.1 — Crate, inventory model, validation, hint types
- **Goal:** the pure data layer.
- **Changes:** root `Cargo.toml`; `crates/sites/{Cargo.toml, README.md, src/{lib,error,hints,layout}.rs, src/inventory/{mod,model,validate}.rs}`.
- **Tests:** accept/reject tables per rule; CRUD integrity; serde round-trips; unknown field rejected; newer `version` read-only; `PluginHints` validation (a family needs ≥ 2 slugs; a slug belongs to one family; variant names unique; markers are allowlisted relative paths).
- **Done when:** the crate builds with no `apvm-*` deps and every validator is covered.
- **Depends:** — · **Parallel:** P0

#### P1.2 — Inventory persistence
- **Goal:** safe load/save/update.
- **Changes:** `inventory/io.rs` (lock, atomic `0600` write, `update`, symlink refusal, `env_override`).
- **Tests:** missing file = empty; located errors; `0600`; no partial file; two concurrent `update`s both land; symlink refused; env override.
- **Depends:** P1.1 · **Parallel:** P1.3–P1.5, P3.1

#### P1.3 — Settings merge and targets
- **Goal:** `ResolvedSite` + `Targets` resolution.
- **Changes:** `inventory/resolve.rs`.
- **Tests:** precedence table; union/order/dedupe; empty selection error; same-WordPress warning.
- **Depends:** P1.1 · **Parallel:** P1.2, P1.4, P1.5, P3.1

#### P1.4 — Core: slugs, variant markers, families → `PluginHints`
- **Goal:** core describes its plugins to `apvm-sites`.
- **Changes:**
  - `crates/core/Cargo.toml` (`apvm-sites`);
  - `Builder::plugin_slug(variant)`, `Builder::variant_marker(variant)`, `Builder::family_traits()` (all with defaults returning "none", so custom builders don't break), implemented for BackWPup/WP Rocket/Imagify per §3.1;
  - `ProjectRegistry::{plugin_hints, project_for_slug}` and `Apvm::plugin_hints`;
  - `apvm info` shows slug + marker per variant; `SKILL.md`.
- **Tests:** declared values equal §3.1; hints validate; `info` output.
- **Depends:** P1.1 · **Parallel:** P1.2, P1.3, P1.5, P2.x, P3.x

#### P1.5 — Package inspection
- **Goal:** `PluginPackage::inspect(path, &hints)` per §6.10.
- **Changes:** `package/{mod,headers,rules}.rs`.
- **Tests:**
  - generated zips covering every rule (single root ✓, files at root ✗, two roots ✗, `__MACOSX`, `..`/absolute/backslash ✗, symlink ⚠);
  - headers: missing `Plugin Name` ✗, several headered ⚠, beyond 8 KiB ignored, CRLF, multibyte at the boundary (no panic);
  - bomb caps; unsupported compression → clear error; SHA-256;
  - variant detection from markers.
- **Depends:** P1.1 · **Parallel:** P1.2–P1.4, P2.x, P3.x, P4.x

### P2 — Inventory front ends

#### P2.1 — cli: `apvm server` + `server defaults`
- **Goal:** manage servers offline.
- **Changes:**
  - `crates/cli/Cargo.toml`; `paths.rs` (`inventory_file`, `ssh_control_dir`, `sites_logs_dir`); `main.rs` (dispatch, env override);
  - `commands/{mod,server}.rs`; `prompt.rs` (shared `confirm`; `uninstall.rs` moves onto it, its tests unchanged);
  - `.claude/skills/apvm-cli/{SKILL.md, references/sites.md}`; `skill/embedded.rs` (`SKILL_FILES`); `tests/cli_inventory.rs`.
- **Tests:** add/edit/show/list/remove; defaults set/unset; invalid input → exit 1; `--json`; `--cascade`; `APVM_INVENTORY`; `0600`.
- **Depends:** P1.2, P1.3 · **Parallel:** P1.4, P1.5, P2.4, P3.x

#### P2.2 — cli: `apvm site`
- **Goal:** manage sites offline (the probe arrives in P4.3).
- **Changes:** `commands/site.rs`; `SKILL.md`.
- **Tests:** as P2.1, plus `--wp-cli` tokens, `--run-as`/`--owner`/`--ownership`, filters.
- **Depends:** P2.1 · **Parallel:** P2.4, P3.x

#### P2.3 — cli: `apvm group`
- **Goal:** manage groups.
- **Changes:** `commands/group.rs`; `SKILL.md`.
- **Tests:** add/edit/remove; unknown site rejected; site removal updates groups.
- **Depends:** P2.2 · **Parallel:** P2.4, P3.x

#### P2.4 — napi: `SiteInventory`
- **Goal:** the inventory from Node.
- **Changes:** `crates/napi/Cargo.toml`; `sites_inventory.rs`, `sites_types.rs`; `lib.rs` docs; `__tests__/sites-inventory.test.ts`; `crates/napi/README.md`.
- **Tests:** round-trip; validation errors with `err.code`; `APVM_INVENTORY` from `process.env`; `save` → `0600`.
- **Depends:** P1.2, P1.3 · **Parallel:** P2.1–P2.3, P3.x

### P3 — Remote execution (lib)

#### P3.1 — Script builder
- **Goal:** typed, framed scripts for both delivery modes (§6.1).
- **Changes:** `remote/{mod,quote,script,identity}.rs` (prefixes incl. `-g`, closed `RootCommand`, `UploadScript`).
- **Tests:** render goldens; adversarial round-trips through real `sh -s` and `sh -c` (Unix); outer layer through the locally available shells (skipped if absent); `UploadScript` rejects unsafe values at construction.
- **Depends:** P1.1 · **Parallel:** P1.2–P1.5, P2.x, P0

#### P3.2 — Transport trait, capture, local shell, fake
- **Goal:** run scripts and capture results safely.
- **Changes:** `remote/{transport,capture,local}.rs`, `#[cfg(test)] fake.rs`.
- **Tests:** caps; timeout kills the child; 20 MB stdin hash round-trip; nonce behind noise; 255 remap.
- **Depends:** P3.1 · **Parallel:** P4.1, P1.x, P2.x

#### P3.3 — SSH transport
- **Goal:** the real transport with actionable errors.
- **Changes:** `remote/{ssh,classify}.rs`; `tests/support/fake-remote/fake-ssh`.
- **Tests:** argv goldens across precedence; Windows argv; classification table from P0's stderr samples; hermetic fake-ssh; E2E (connect, unknown host key, `accept-new`, wrong key).
- **Depends:** P3.2, P1.3 · **Parallel:** P4.1

#### P3.4 — Multiplexing and per-server limits
- **Goal:** fast repeated execs within `MaxSessions`.
- **Changes:** `remote/mux.rs`, per-server semaphore.
- **Tests:** option goldens; long-path/Windows fallback; E2E `ssh -O check` live after the first exec.
- **Depends:** P3.3 · **Parallel:** P4.x

### P4 — WordPress driver and probe

#### P4.1 — lib: WP-CLI commands and parsers
- **Goal:** typed `wp` argv and robust parsing.
- **Changes:** `wp/{mod,command,parse,state,snippets}.rs`.
- **Tests:** argv goldens per command, **asserting the banned flags never appear**; parser fixtures (clean, notice-prefixed, HTML error, truncated, empty, unknown status, single-file plugin).
- **Depends:** P3.1 · **Parallel:** P3.2–P3.4

#### P4.2 — lib: probe, identity (owner + group), session
- **Goal:** §6.2 end to end.
- **Changes:** `wp/probe.rs`, `remote/identity.rs` (decision table), `session.rs`; `tests/support/fake-remote/{fake-wp, fake-sudo}`.
- **Tests:**
  - exhaustive identity table (including every group row); probe over `FakeTransport` for every §6.2 failure code;
  - E2E: `deploy` → `www-data` via `sudo -u -g`; `deploy` on `ftpsite` → `ftpuser:www-data`; `ftpuser` login (no sudo) → none + `chgrp` plan; `plain` → `sudo_*` with fix text; `pwsudo` → `sudo_password_required`; `fishy`/`noisy` work.
- **Depends:** P4.1, P3.3 · **Parallel:** P1.4, P1.5, P3.4

#### P4.3 — cli: `site test`, `server test`, probe on `site add`
- **Goal:** diagnose a site in one command.
- **Changes:** `commands/{site,server}.rs`, probe renderer, `--trust-new-host-key`, `--no-test`; `SKILL.md`.
- **Tests:** hermetic CLI (success, each failure, `--json`, save-anyway prompt); gated E2E.
- **Depends:** P4.2, P2.2 · **Parallel:** P5.1, P6.1

### P5 — Read path

#### P5.1 — lib: fan-out runner, events, log reporter (+ cli renderer)
- **Goal:** bounded parallel site work with observable progress.
- **Changes:** `ops/runner.rs`, `progress.rs` (incl. `LogFileReporter`: `0600`, keep newest 50); CLI `commands/wp/render.rs` (`MultiProgress`, plain lines without a TTY).
- **Tests:** limits and ordering; event delivery; log retention/permissions.
- **Depends:** P4.2 · **Parallel:** P4.3, P6.1

#### P5.2 — lib + cli: `apvm wp list`
- **Goal:** what is on which site, with variants.
- **Changes:** `ops/{list,variants}.rs` (marker detection); `commands/wp/{mod,args,list}.rs` (`TargetArgs`, `--project`); `SKILL.md`.
- **Tests:** hermetic multi-site list with one site unreachable (exit 1, others shown); filters; variant annotation (`pro-de`/`pro-en`/`pro-?`); `--json`; E2E on single + multisite.
- **Depends:** P5.1, P1.4, P2.3 · **Parallel:** P6.1, P8.1

#### P5.3 — lib + cli: `apvm wp status | is-installed | is-active`
- **Goal:** answer "is it installed/active, which version/variant?" for humans and scripts (§6.15).
- **Changes:** `ops/status.rs` (`Expectations`, evaluation), `SiteManager::status`; `commands/wp/status.rs` (`status` plus the `is-installed`/`is-active` shortcuts); exit code 3 in `main.rs`; `SKILL.md`.
- **Tests:** evaluation table (each `--expect` × each observed status; `--version`; `--variant` incl. unknown → undetermined); exit precedence 1 > 3 > 0; hermetic CLI (mismatch → 1, one unreachable site → 3, all met → 0); `--quiet`; `--json`; E2E `is-active` on active/inactive/absent.
- **Depends:** P5.2 · **Parallel:** P6.1, P8.1

#### P5.4 — napi: `SiteManager` (create, probe, listPlugins, status, isInstalled, isActive) + `apvm.sites()`
- **Goal:** the read path from Node.
- **Changes:** `sites.rs`, `sites_progress.rs` (ThreadsafeFunction, same as `JsProgressReporter`), `__tests__/sites.test.ts` (hermetic, skipped on Windows).
- **Tests:** report shapes, events, rejection codes (`Undetermined` for `isActive` on an unreachable site), hints included.
- **Depends:** P5.3, P2.4 · **Parallel:** P6.x

### P6 — Lifecycle engine (lib)

#### P6.1 — Planner
- **Goal:** §6.3, §6.7 (policies) and §6.9 as pure functions.
- **Changes:** `ops/{model,plan,order,variants}.rs`; `BatchPlan` typestate.
- **Tests:**
  - op × status × flags tables (incl. install policies);
  - family rules: switch by default; `--keep-siblings` keeps both active; two siblings activated together only with `--keep-siblings`; same-slug `variant_conflict`; coexistence and shared-data notes; **nothing is blocked because of a sibling** except within a batch whose own deactivation failed;
  - ordering: "all deactivations precede any removal"; topological order + cycle;
  - selection: `--all`/`--project`/`--exclude`; idempotent no-ops; must-use/dropin;
  - unbound plan can't be applied (`compile_fail` doctest).
- **Depends:** P4.1, P1.5 · **Parallel:** P4.3, P5.x

#### P6.2 — Executor, verification, report, `SiteManager::plan/apply`
- **Goal:** run plans with verification and honest outcomes.
- **Changes:** `ops/{apply,verify,report}.rs`, `manager.rs`; error `code()`s.
- **Tests:** `FakeTransport`:
  - no uninstall before every deactivation is verified;
  - a variant switch deactivates before it activates;
  - a failed deactivation blocks its family members' pass 2 in the same batch only, not unrelated items;
  - transport loss → done/failed/unknown; exit 0 + `Error:` → failed;
  - drift; health failure; hermetic fake-wp run.
- **Depends:** P6.1, P5.1 · **Parallel:** P8.1

#### P6.3 — Ownership, group, group access
- **Goal:** §6.8 check + repair ladder.
- **Changes:** `ops/ownership.rs`; `Own`/`Ownership` steps wired into plan/apply; `ownership` site setting.
- **Tests:**
  - goldens (guards adjacent, `-R -P -h`, numeric ids, `--`);
  - ladder table (group-only/member → `chgrp`; owner → root; modes → `chmod g+`; unfixable);
  - `mirror`/`check`/`off`; single-file plugin; symlinked plugin dir refused.
  - **E2E:**
    - a `x -> /etc/hostname` link inside a plugin is repaired and `/etc/hostname` stays `root:root`;
    - an `ftpsite` tree with group `ftpuser` and no `g+w` is repaired to `ftpuser:www-data` + `g+rw`(+setgid) without root (as `ftpuser` ∈ `www-data`);
    - a `deploy`-owned legacy dir is repaired before removal.
- **Depends:** P6.2 · **Parallel:** P6.4, P7.1, P8.1

#### P6.4 — Site lock
- **Goal:** §6.13.
- **Changes:** `ops/lock.rs`; acquire/release wired into `apply` (and later `run_wp`).
- **Tests:** goldens; path validation; fake: busy → wait → `site_locked`, stale → broken with warning, release on failure paths; E2E: two concurrent applies on one site serialize.
- **Depends:** P6.2 · **Parallel:** P6.3, P7.1, P8.1

### P7 — Lifecycle front ends

#### P7.1 — cli: shared plumbing
- **Goal:** consistent plan/confirm/result UX.
- **Changes:** `commands/wp/args.rs` (`PluginArgs`, `--dry-run`, `-y`, `--json`, `-j`), plan/result renderers, confirmation rules, JSON (`"schema": 1`).
- **Tests:** snapshots (`NO_COLOR`), confirmation matrix, JSON shape.
- **Depends:** P6.2, P5.2 · **Parallel:** P6.3, P6.4, P8.1

#### P7.2 — cli: `apvm wp activate | deactivate`
- **Goal:** activation as its own command, variant switch included.
- **Changes:** `commands/wp/{activate,deactivate}.rs`; health check; `--no-load`; `SKILL.md`.
- **Tests:** hermetic CLI; E2E:
  - `basic` activates (option set, boots);
  - `fatal-activate` fails, stays inactive, excerpt shown;
  - `fatal-load` → `health_check_failed` + hint; `--no-load` recovers it;
  - repeats are no-ops; `parent` before `child`;
  - activating `guarded-pro` while `guarded` is active → switch (needs `-y` when non-TTY), and `guarded-pro`'s activation hook ran;
  - the same with `--keep-siblings` → both active, the site boots, the plan note says which one runs; `activate guarded guarded-pro --keep-siblings` activates both.
- **Depends:** P7.1, P6.4 · **Parallel:** P6.3, P7.5, P8.1

#### P7.3 — cli: `apvm wp uninstall | reset | delete`
- **Goal:** two-phase removal from the CLI.
- **Changes:** `commands/wp/{uninstall,reset,delete}.rs`; per-verb consequences in the confirmation; `SKILL.md`.
- **Tests:** E2E:
  - **regression for D9:** `guarded` + `guarded-pro`, one active, both uninstalled → both cleanups ran;
  - a verify-only check shows WP-CLI's own `uninstall --deactivate` leaves the data;
  - independent removal: uninstalling `guarded-pro` while `guarded` stays active succeeds and keeps the shared data (note shown); with `guarded` inactive, the shared data is removed (note shown); `guarded` keeps working in both cases;
  - `reset` keeps files, clears data (`--activate` restores); `delete` keeps data;
  - foreign-owned dir repaired, then removed; `--all --exclude`; `--project`.
- **Depends:** P7.2, P6.3 · **Parallel:** P7.4, P7.5, P8.1, P8.5

#### P7.4 — cli: `apvm wp ownership` + `apvm site unlock`
- **Goal:** check/repair existing installs on demand; clear a stuck lock.
- **Changes:** `commands/wp/ownership.rs`; `site unlock` in `commands/site.rs`; `SKILL.md`.
- **Tests:** hermetic report; E2E `--fix` on `ftpsite` (no root) and `single` (root rung); `--dry-run`; `site unlock` shows the holder, asks, removes; nothing to unlock → exit 0.
- **Depends:** P7.1, P6.3, P6.4 · **Parallel:** P7.3, P7.5

#### P7.5 — napi: lifecycle + ownership ops
- **Goal:** `activate`, `deactivate`, `uninstall`, `reset`, `deletePlugins`, `ownership` from Node.
- **Changes:** `sites.rs`, types, tests.
- **Tests:** hermetic runs per op, `dryRun` plans, report codes.
- **Depends:** P6.3, P6.4, P5.4 · **Parallel:** P7.1–P7.4, P8.x

### P8 — Install

#### P8.1 — lib: staging
- **Goal:** §6.6.
- **Changes:** `ops/staging.rs`.
- **Tests:** goldens; path validation (only `<tmp>/apvm.XXXXXXXXXX`); space check; checksum-tool selection; fake mismatch → `upload_corrupted`; stale sweep pattern; E2E: 20 MB upload hash-matches, dir `0700` owned by the identity (incl. `ftpuser:www-data`), removed after success and after failure.
- **Depends:** P4.2 · **Parallel:** P6.x, P7.x

#### P8.2 — lib: install execution
- **Goal:** §6.7 inside `apply`.
- **Changes:** `ops/install.rs`; install steps, policies, switch, ownership wiring.
- **Tests:**
  - fake: policy matrix; version mismatch → `verification_failed`; cleanup + lock release on every failure path; `replace` keeps active.
  - E2E:
    - fresh install; `--force` over an active plugin (stays active, new version); `reinstall` wipes data; `skip`; `fail`;
    - `--activate` next to an active sibling: switch by default, both active with `--keep-siblings`; `--variants` with two members installs both;
    - files owned `www-data:www-data` on `single` and `ftpuser:www-data` + `g+rw` on `ftpsite`;
    - `writes-content` side-effect file owned correctly; requirement warnings.
- **Depends:** P8.1, P6.3, P6.4 · **Parallel:** P7.x

#### P8.3 — cli: `apvm wp install --zip`
- **Goal:** batch installs of any zips.
- **Changes:** `commands/wp/install.rs` (zip sources); `SKILL.md`.
- **Tests:** hermetic CLI; E2E: 3 zips (incl. `parent` + `child`) onto 2 sites with `--activate` (`parent` first); `--dry-run` sends no mutating command.
- **Depends:** P8.2, P7.1 · **Parallel:** P8.4, P8.5, P10.1

#### P8.4 — napi: `install` (zips)
- **Goal:** zip installs from Node.
- **Changes:** `sites.rs`, types, tests.
- **Tests:** hermetic installs (fresh, each `ifInstalled` policy, `activate`), `dryRun` plan, report codes, invalid zip → `InvalidArg`.
- **Depends:** P8.2, P5.4 · **Parallel:** P8.3, P8.5

#### P8.5 — core: `build_package`, `install_build`
- **Goal:** build → package → install as a reusable core API.
- **Changes:**
  - `crates/core/src/deploy.rs`, `lib.rs` re-exports;
  - `Error::Sites(#[from] apvm_sites::Error)` + `crates/napi/src/error.rs` mapping (the match is exhaustive).
- **Tests:** variant selection rules (multi-variant needs `--variants`; same-slug pairs rejected); slug mismatch; `install_build` over a `SiteManager` with fake transport and an injected built package (the build path is covered by existing pipeline tests + P8.6's gated E2E); napi status mapping.
- **Depends:** P8.2, P1.4 · **Parallel:** P8.3, P8.4, P7.x

#### P8.6 — cli: `apvm wp install <PLUGIN> <GIT_REF>`
- **Goal:** install apvm builds, cached or built on demand.
- **Changes:**
  - extract `SourceArgs` from `BuildArgs` (behavior and existing tests unchanged);
  - pending plan → confirm → build (build spinner) → bind → apply; `--dry-run` never builds;
  - `SKILL.md`.
- **Tests:** clap tests (sources, `--variants`, conflicts); `apvm build` tests untouched and green; gated `APVM_E2E_BUILD` release install.
- **Depends:** P8.3, P8.5 · **Parallel:** P8.7, P9.x

#### P8.7 — napi: `apvm.installBuild`
- **Goal:** build + install from Node.
- **Changes:** `apvm.rs`, types, tests, README.
- **Tests:** option validation (`variants` rules → `InvalidArg`); `dryRun` returns the plan without building; gated `APVM_E2E_BUILD` install.
- **Depends:** P8.4, P8.5 · **Parallel:** P8.6, P9.x

### P9 — WP-CLI passthrough

#### P9.1 — lib: classifier + `run_wp`
- **Goal:** §6.12 in the library.
- **Changes:** `wp/classify.rs`, `ops/run.rs`, `SiteManager::run_wp`.
- **Tests:** classification tables (read-only paths at depth 1–3, `--dry-run` cases, refused params, refused commands, `@alias`, two-phase violators); hermetic run with arguments holding quotes, backslashes, newlines and `$()` arriving verbatim; may-write takes the lock; timeouts; per-site results.
- **Depends:** P6.4 · **Parallel:** P7.x, P8.x

#### P9.2 — cli: `apvm wp run`
- **Goal:** the command.
- **Changes:** `commands/wp/run.rs` (trailing args after `--`, single-target passthrough of output and exit code, multi-target blocks/`--json`); `SKILL.md`.
- **Tests:** clap trailing args; confirmation for may-write; exit-code passthrough; E2E `option get siteurl`, `plugin is-active` exit codes, `eval` with a backslash/newline payload.
- **Depends:** P9.1, P7.1 · **Parallel:** P9.3

#### P9.3 — napi: `runWp`, `classifyWp`
- **Goal:** passthrough from Node.
- **Changes:** `sites.rs`, types, tests.
- **Tests:** `classifyWp` table parity with the library; may-write without `allowWrites` → `ConfirmationRequired`; refused → `InvalidArg`; hermetic run with verbatim tricky arguments.
- **Depends:** P9.1, P5.4 · **Parallel:** P9.2

### P10 — Multisite

#### P10.1 — lib: facts and rules
- **Goal:** §6.11 in probe + planner.
- **Changes:** snippets (headers, subsite activations), probe/plan rules.
- **Tests:** multisite planner tables; snippet parsing; `--network` on a single site → input error.
- **Depends:** P6.1, P4.2 · **Parallel:** P7.x, P8.x, P9.x

#### P10.2 — Enable multisite mutations (lib + cli + napi tests)
- **Goal:** remove `multisite_unsupported`.
- **Changes:** plan guard removal; docs; `SKILL.md`; NAPI tests.
- **Tests:** E2E on `multi`: `network-only` auto network-activates; a plugin active only on subsite 2 is deactivated there before uninstall, and its cleanup is verified.
- **Depends:** P10.1, P7.3, P8.3, P7.5 · **Parallel:** P8.6, P8.7, P9.x

### P11 — Docs and release 3.4.0
- **Goal:** ship.
- **Changes:**
  - root `README.md`; `crates/{sites,cli,core,napi}/README.md`;
  - final `SKILL.md` + `references/sites.md` (sudoers recipes incl. FTP-owned sites, requirements, codes);
  - version 3.4.0 in `Cargo.toml`/`Cargo.lock`/`package.json`/`package-lock.json`/`SKILL.md`;
  - tag `v3.4.0` after CI is green.
- **Depends:** P0–P10 · **Parallel:** —

### Dependency overview

```
P0 (E2E target) ─────────────────────────────────────────────────────────────────────────
P1.1 ┬ P1.2 ┬ P2.1 ─ P2.2 ─ P2.3 ─────────────┐
     ├ P1.3 ┴ P2.4 ───────────────────────────┼──────────────┐
     ├ P1.4 ──────────────────────────────────┤              │
     ├ P1.5 ─────────────┐                    │              │
     └ P3.1 ┬ P3.2 ─ P3.3 ─ P3.4              │              │
            └ P4.1 ─ P4.2 ┬ P4.3              │              │
                          ├ P5.1 ─ P5.2 ◄─────┘ ─ P5.3 ─ P5.4 ◄─────┘
                          ├ P8.1
P4.1+P1.5 ─ P6.1 ─ P6.2(+P5.1) ┬ P6.3 ┐
                               └ P6.4 ┴─ P7.5(+P5.4)
P6.2+P5.2 ─ P7.1 ─ P7.2(+P6.4) ─ P7.3(+P6.3) ;  P7.1+P6.3+P6.4 ─ P7.4
P8.1+P6.3+P6.4 ─ P8.2 ┬ P8.3(+P7.1) ─ P8.6(+P8.5)
                      ├ P8.4(+P5.4) ─ P8.7(+P8.5)
                      └ P8.5(+P1.4)
P6.4 ─ P9.1 ┬ P9.2(+P7.1) ;  P9.1+P5.4 ─ P9.3
P6.1+P4.2 ─ P10.1 ─ P10.2(+P7.3, P8.3, P7.5)
all ─ P11
```

**Critical path:** P1.1 → P3.1 → P3.2 → P3.3 → P4.2 → P5.1 → P6.2 → P6.3 → P8.2 → P8.3 → P10.2 → P11. NAPI phases never sit on it.

---

## 15. Enhancements (after 3.4.0)

| # | Enhancement | Notes |
|---|---|---|
| E1 | SSH passwords | `SSH_ASKPASS` + `SSH_ASKPASS_REQUIRE=force` (OpenSSH ≥ 8.4) with an env-fed helper, or an OS keychain; Windows spike first |
| E2 | `local` / `docker` transports in the inventory | Same scripts: local `sh -s`, `docker exec -i <c> sh -s` |
| E3 | Per-server staging reuse | One upload per server and identity |
| E4 | Manifests | Several apvm projects/refs in one plan |
| E5 | wordpress.org / URL sources | `wp plugin install <slug> --version=X` remotely |
| E6 | Deployment journal | `apvm wp history`: what went where, from which commit/sha256 |
| E7 | HTTP health check | Front-end request after activation |
| E8 | Rollback | Needs a backup of the previous plugin dir |
| E9 | Themes | Same engine |
| E10 | `config.json` hardening | `0600` + atomic write (it holds the GitHub token; today's save is a plain `fs::write`) |
| E11 | Core header parser reuse | `detect_wordpress_plugin_version` slices `&content[..8192]`, which panics if byte 8192 falls inside a multibyte character; delegate it to the P1.5 parser |
| E12 | `--slug` / `--with-dependencies` | Once stable WP-CLI bundles extension-command ≥ 2.3.0 |
| E13 | User-declared families | Inventory-level `PluginHints` for non-apvm free/pro pairs |
| E14 | `wp run --file` | Several WP-CLI commands under one lock and one confirmation |
| E15 | Version ranges in `status` | `--version '>=5.7'` (semver comparison) |

---

## 16. Decisions (finalized 2026-10-09)

| # | Decision | Final |
|---|---|---|
| 1 | Command noun | `apvm wp …` |
| 2 | Already-installed default | `--if-installed fail`; `--force` = replace |
| 3 | Variants (revised in rev 3) | Free and pro fully independent: nothing is blocked because of a sibling, both may be installed or active; activation next to an active sibling switches by default, `--keep-siblings` keeps both; pro-de/pro-en (same slug) conflict, `--force` replaces |
| 4 | Identity | `run_as = auto`: owner **and group** of the plugins dir; `login` kept as an option |
| 5 | Inventory file | `~/.apvm/inventory.json` |

---

## Sources

- WP-CLI handbook (Context7 `/wp-cli/handbook`); `wp-cli/extension-command` `Plugin_Command.php`, `CommandWithUpgrade.php`, releases; `wp-cli/wp-cli` `utils.php`, `Runner.php`, releases.
- WordPress `wp-admin/includes/class-wp-upgrader.php`, `file.php`, `plugin.php`.
- ssh(1), ssh_config(5), sshd(8), sshd_config(5) (OpenBSD); sudo(8), sudoers(5) (sudo.ws); GNU coreutils `chown`.
- zip crate docs (Context7 `/websites/rs_zip_zip`).
- Local artifacts: `~/.apvm/cache/backwpup/commits/9.99.99/1dceea8/*` (pro-de/pro-en full-tree diff), `releases/5.7.6/*`.
