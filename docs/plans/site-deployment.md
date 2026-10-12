# Site deployment — manage WordPress plugins on remote sites

Status: **in progress**: P0 done (verified locally on macOS and Ubuntu clients, and in CI on 2026-10-11). Rewritten 2026-10-09; revisions 2 and 3 (review feedback), 4 (P0 results), 5 (adversarial audit, 2026-10-11) and 6 (CI on the branch). · Target version: **3.4.0** (from 3.3.0) · Author: Sandy Figueroa

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
- **Ownership respects owner *and* group:** WP-CLI runs as the plugins directory's owner **and group** (`sudo -n -u <owner> -g <group>`), so `ftpuser:www-data` sites stay `ftpuser:www-data` with the group bits the web server needs. Mismatches are repaired without root (`chgrp`, `chmod g+`); a root `chown` runs only on sites that opt in, under strict preconditions.
- **Uploads:** each zip is inspected locally, streamed over SSH into a private **staging dir** (`/tmp/apvm.XXXXXXXXXX`, owned by the identity, `0700`), checksum-verified, installed from there, and always cleaned up.
- **Transport:** system OpenSSH (your `~/.ssh/config`, agent, ProxyJump), multiplexed on Unix. Scripts travel over SSH's stdin, so the remote login shell (bash, zsh, fish, …) never parses them.
- **Phases:** 44 small ones; libraries first, then the CLI and NAPI front ends in parallel tracks. No new crates.io dependencies.

---

## 0. Change log

### Revision 6 (CI on the branch, 2026-10-11)

| Topic | Change |
|---|---|
| CI | `ci.yml` temporarily runs on pushes to `feat/site-deployment` (P0); new last phase P12 removes the trigger after the merge |
| First CI run | Two fixes: `tests/e2e/sites/.shellcheckrc` (`source-path=SCRIPTDIR`, so the lint follows sourced files from any directory), and the git-isolation test now compares canonical paths (Windows prints `C:/…`, `canonicalize` yields `\\?\C:\…`). P0 is done |

### Revision 5 (adversarial audit of P0 and revision 4, 2026-10-11)

| Topic | Change |
|---|---|
| Root repair | The root `chown` rung is opt-in per site (`ownership: mirror-root`) and constrained: absolute path checked by root `stat`, `--from=<the foreign uid>` (never root, never the login user), no multi-linked files, `fs.protected_hardlinks = 1`. The default `mirror` never runs root and reports `ownership_unfixable` with the exact commands (§6.8) |
| Identity source | Never taken from what WordPress reports: U:G come from the login/root `stat` of `<path>/wp-content/plugins`. A different `WP_PLUGIN_DIR` blocks with `plugins_dir_mismatch` unless the site pins `plugins_dir` (§6.2) |
| Hermetic WP-CLI | Every call is `env WP_CLI_CONFIG_PATH=/dev/null <wp_cli> --skip-packages …`. The identity's WP-CLI config and packages never run; P0 showed both execute code on every call (§6.1) |
| umask | Set inside WP-CLI with `--exec='umask(umask() & ~0<g>);'` on every call, replacing the `sh -c` wrapper: no shell under sudo, and the identity's "other" bits are kept (§6.1) |
| `wp run` | Every WP-CLI global parameter except the allowed overrides is refused (`--exec`, `--require`, …). `search-replace` is read-only only with `--dry-run` and no `--export`/`--log`. Every `wp run` takes the site lock. Exit 255 means apvm could not run it (§6.12) |
| sudo | `-l` listing a rule is not enough (it may need a password), so the probe runs the exact command shape. Identity rules need `NOPASSWD: ALL`, and refused probes are logged by sudo (§6.2, §10) |
| Scripts | Every script starts with `LC_ALL=C`. `UploadScript` values use a strict allowlist, also applied to `remote_tmp_dir` (§6.1, §7) |
| Reads | Must-use plugins and drop-ins load under `--skip-plugins`, so a fatal there breaks reads. The boot check relies on the exit code only (§6.4, §6.14) |
| `DISALLOW_FILE_MODS` | Blocks file-changing operations (`file_mods_disallowed`) unless `--allow-file-mods` (§6.7) |
| BackWPup model | BackWPup has no activation hook: the loaded edition installs on `init` when the stored version differs. The plan notes and the fixtures follow that (§3.1, §6.9) |
| Evidence | §3.5 states only what checks assert, and names its single target, so the probe measures instead of assuming |

### Revision 4 (P0 results, 2026-10-09)

| Topic | Change |
|---|---|
| sudo refusals | `sudo -n` says "a password is required" for every refusal except requiretty, so the probe classifies with `sudo -n -l` (§6.2) |
| PHP fatals | Once WordPress has loaded, a fatal exits **1** ("There has been a critical error on this website."), not 255. The 255→254 remap stays, and the health check fails on any non-zero exit (§6.1, §6.4) |
| Upload quoting | `UploadScript` values also exclude `'`, and templates never contain `\\` or `\'` (fish) (§6.1) |
| umask | WordPress keeps setgid only when the identity's umask matches `FS_CHMOD_DIR`. File-creating steps now set a umask derived from the plugins dir (§6.1, §6.8) |
| Output noise | Parsers skip PHP notices before the JSON (WP-CLI 2.12.0 on PHP 8.5). The SSH classifier ignores the `accept-new` notice and locale warnings (§9, §3.5) |
| `DISALLOW_FILE_MODS` | WP-CLI ignores it, so the plan shows a warning (§6.7) |
| Harness | §12 and P0 describe the harness as built (`tests/e2e/sites/`). Phase tests reference its users and samples |

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
| D5 | Ownership | `ownership = mirror`: verify owner, group and group bits after file-creating steps and owner/group before removals; repair without root; a root `chown` only with `ownership = mirror-root` (revision 5) | Your FTP-user/`www-data` layout; a root repair trusts the site's code, so it is the operator's explicit choice | Always chown; root chown by default |
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
- **Shared main-file guard:** line 25 of both main files is `if ( defined( 'BACKWPUP_PLUGIN_LOADED' ) || class_exists( \BackWPup::class, false ) ) { return; }`. With two variants active, only the first one loaded runs; the other returns before registering anything, including its `register_deactivation_hook` (line 96 in 5.7.6).
- **No activation hook; install on `init`:** BackWPup 5.7 registers no activation hook.
  - On every request in which it is the loaded edition, `src/Plugin/Plugin.php` schedules `BackWPup_Install::activate` on `init` when the `backwpup_version` site option differs from its own version (or its cleanup cron is missing).
  - So the install runs on the first request after activation, never in WP-CLI's activation request.
  - With both editions active, only the one loaded first (pro, §3.5) ever runs it, and only after a version change.
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

### 3.5 Verified in P0 (harness; 2026-10-09, re-run after the audit 2026-10-11)

**How:** `tests/e2e/sites/verify.sh` checks every row below over real SSH (check ids in brackets). It asserts each fact; INFO checks only record versions and network-dependent behavior. CI runs it in the `e2e-sites` job.

**Where it ran:**
- Clients: macOS (OpenSSH 10.3p1), an Ubuntu 24.04 container (OpenSSH 9.6p1), and CI's GitHub-hosted `ubuntu-24.04` runner (job `e2e-sites`, run 38189801662, 2026-10-11: 43 pass, 0 fail, 2 INFO, 1 SKIP; the optional real-BackWPup check is skipped there).
- Target: Debian 13, OpenSSH 10.0p2, sudo 1.9.16p2, WordPress 7.1.3, PHP 8.4.26, WP-CLI 2.12.0.
- Also run once on a PHP 8.5.11 build of the same target.

**Result:** 43 asserted checks pass, 2 INFO, 0 fail. The optional real-BackWPup check passes too (locally).

**Scope:** one target. Other distributions (RHEL), sudo-rs, other WP-CLI and sudo versions are unverified, which is why the probe (§6.2) measures what it relies on instead of assuming it. ⚠ marks a deviation from what the plan first assumed; its fix is in revisions 4 and 5.

**WP-CLI 2.12.0**

| Item | Result | Plan impact |
|---|---|---|
| `--skip-delete`, `plugin list --skip-update-check` | Both present. `uninstall --skip-delete` runs `uninstall.php`, keeps the files, and leaves the plugin `inactive` [wpcli.uninstall-skip-delete] | — |
| `install --force` over an active plugin | It stays active at the new version, and the activation hook does not run [wpcli.force-over-active] | — |
| `WP_CLI::error` exit status | 1 [wpcli.exit-codes] | — |
| ⚠ PHP fatal exit status | **255** only before WordPress loads (`--skip-wordpress`), or during activation, where WordPress turns its fatal handler off while it sandboxes the plugin. `fatal-activate` exits 255 and stays inactive. **Once WordPress has loaded**, its default fatal-error handler prints `Error: There has been a critical error on this website.` and exits **1** [wpcli.exit-codes, wpcli.activation-failures] | §6.1, §6.4 |
| Network-only plugin without `--network` (multisite) | WP-CLI network-activates it anyway: exit 0, `Plugin 'x' network activated.` Deactivating a network-active plugin without `--network` is refused with exit 1: `Warning: Plugin 'x' is network active and must be deactivated with --network flag.` / `Error: No plugins deactivated.` [wpcli.network-only] | — (flags are derived) |
| `--skip-plugins=<slug>` and the deactivation hook | The hook is skipped. A plugin that breaks every request can be deactivated this way, repeatedly, and reads with `--skip-plugins --skip-themes` keep working [wpcli.skip-plugins-hook, wpcli.activation-failures] | — |
| ⚠ Must-use plugins and drop-ins | They still load under `--skip-plugins` (an `object-cache.php` drop-in included), and a fatal must-use plugin makes `plugin list` exit 1 with the critical-error message [wpcli.mu-plugins-load] | §6.14, §10 |
| ⚠ User config and packages | A `require:` in the identity's `~/.wp-cli/config.yml` and an installed package both run code on every call, `--skip-plugins` or not. `--skip-packages` stops only the package. `env WP_CLI_CONFIG_PATH=/dev/null` plus `--skip-packages` stops both, and output stays clean [wpcli.config-isolation] | §6.1 |
| `WP_CLI_VERSION` in `wp eval` | `2.12.0` [wpcli.version] | — |
| ⚠ `DISALLOW_FILE_MODS` | WP-CLI ignores it: install, activate and delete all succeed [wpcli.disallow-file-mods] | §6.7 blocks |
| Messages | Partial batch: `Warning: The 'x' plugin could not be found.` / `Error: Only activated 1 of 2 plugins.` Nothing activated: `… / Error: No plugins activated.` Uninstalling an active plugin: `Warning: The 'x' plugin is active.` / `Error: No plugins uninstalled.` All exit 1 [wpcli.exit-codes, wpcli.uninstall-active] | — |
| `Requires Plugins` | Activating the child first fails with exit 1 (`… requires 1 plugin to be installed and activated: …`). Deactivating the parent under an active child is allowed (exit 0; the child stays active) [wpcli.requires-plugins] | — |
| `plugin list` rows | Must-use: name = file name without `.php`. Drop-in: name = file name **with** `.php` (`advanced-cache.php`), version empty. Single-file plugin: `hello` → `hello.php` [wpcli.list-read] | Parser fixtures (P4.1) |
| ⚠ PHP 8.5 | Every WP-CLI 2.12.0 run raises `E_DEPRECATED` ("Case statements followed by a semicolon…", from react/promise). The image's default php.ini hides it. With `display_errors` on, it lands on stdout before the JSON, and the payload stays the last line. On PHP 8.4 nothing precedes it [wpcli.php-notices, run on both builds] | §9; the harness pins PHP 8.4 |
| `search-replace --dry-run` | With `--log=<file>` it still writes the log file; `--export` is refused with it (`Error: You cannot supply --dry-run and --export at the same time.`, exit 1); the database is untouched [wpcli.search-replace-dry-run] | §6.12 |

**sudo**

| Item | Result | Plan impact |
|---|---|---|
| ⚠ `sudo -n` messages | `sudo: a password is required` for **every** refusal: password needed, user not in sudoers, runas user or group not allowed, command not allowed. sudo authenticates before it applies the policy. Only requiretty differs: `sudo: sorry, you must have a tty to run sudo`. Always exit 1 [sudo.messages] | §6.2 |
| `sudo -n -l [-u U -g G] CMD` | Allowed: prints the command path, exit 0. The user has password-less rules but not this one: exit 1 with **no output**. The user has no password-less rule: "a password is required" [sudo.classification] | §6.2 |
| ⚠ `-l` vs a password rule | `sudo -n -l -u www-data true` prints `/usr/bin/true` (exit 0) for a rule that needs a password, because the user also has a NOPASSWD rule (`listpw=any`). The real command is still refused [sudo.classification, user `mixed`] | §6.2 |
| `sudo -n -u ftpuser -g www-data` | Works with the plain `(ftpuser)` Runas form, because `ftpuser` ∈ `www-data`, and with the explicit `(ftpuser : www-data)` form. `-g root` is refused [sudo.owner-and-group, sudo.classification] | — |
| `runuser -u U -g G --` (root login) | Works [sudo.runuser] | — |
| Environment | HOME is the target's (`/var/www` for www-data). `LC_ALL` set before sudo reaches the command. umask is `0002` for SSH logins and sudo (Debian `pam_umask`, user-private groups) but **`0022` under `runuser`** [sudo.env-and-umask] | §6.1 |
| Audit trail | Each refused `sudo -n` probe adds a sudo log entry [sudo.audit-trail] | §6.2, §10 |

**Groups and permissions** (on `ftpsite`: `ftpuser:www-data`, dirs 2775)

| Item | Result | Plan impact |
|---|---|---|
| ⚠ WP-created dirs vs setgid | WordPress `mkdir()`s, then chmods to `FS_CHMOD_DIR` only when `fileperms & 0777` differs, and PHP's `chmod()` clears setgid. With umask `0002`, new dirs are 2775 (setgid and group kept). With `0022`, they end up 0775 (setgid lost) [perms.install-matrix] | §6.1, §6.8 |
| ⚠ `ftpuser`'s own install (primary group `ftpuser`) | With umask `0002`: no mismatch, thanks to setgid inheritance. With `0022`: every entry `ftpuser:ftpuser`, no setgid. Rungs 1 (`chgrp -R -h`) and 3 (`chmod g+rwxs` / `g+rw`) repair it as `ftpuser`, without root [perms.install-matrix, perms.repair-without-root] | Ladder confirmed |
| umask relaxation with `--exec` | `--exec='umask(umask() & ~0070);'` turns an identity umask of 0022 into 0002, 0027 into 0007 and 0077 into 0007 inside WP-CLI ("other" bits kept); an install under a 0022 identity then needs no repair [perms.umask-exec, perms.install-matrix] | §6.1 |
| Activation side effects | Files written into `wp-content/` get `ftpuser:www-data` [perms.side-effects] | — |

**Shells and SSH**

| Item | Result | Plan impact |
|---|---|---|
| `sh -s` with `</dev/null` | Same on dash, bash and busybox ash, including the 255→254 remap. Without `</dev/null`, under bash a command eats the rest of the script (bash reads piped stdin unbuffered); dash and ash had already buffered it [shells.stdin-delivery] | Rule confirmed |
| `.bashrc` noise | Debian bash sources `~/.bashrc` for SSH commands. The noise lands on stdout before the nonce and on stderr; framing isolates the payload in both delivery modes [shells.bashrc-noise] | — |
| ⚠ `sh -c '…'` quoting under bash, dash, zsh, fish and tcsh login shells | Every printable ASCII character but `'` and `\` (`!`, `$` and backtick included), tab and UTF-8 survive all five. **fish** fails on a `'` inside a value ("quotes are not balanced") and turns `\\` into `\`; the other four keep both [shells.sh-c-quoting] | §6.1 |
| Upload: `sh -c` + `sudo tee` + SHA-256 | Works through every login shell (4 MiB) [shells.upload-mode] | — |
| `ssh [opts] -l USER -- HOST 'sh -s'` | Accepted [shells.ssh-argv] | — |

**Harness basics**

| Item | Result | Plan impact |
|---|---|---|
| Official image ownership | `/usr/src/wordpress` is `www-data:www-data` (755/644) [harness.image-ownership] | — |
| WP-CLI as `www-data` | HOME is `/var/www`, owned by root. Reads and zip installs print no warnings. Only cache-using commands warn (`Warning: Failed to create directory '/var/www/.wp-cli/cache/'`) [harness.wp-as-www-data] | — |
| `mktemp -d /tmp/apvm.XXXXXXXXXX` | Gives `/tmp/apvm.` + 10 × `[A-Za-z0-9]`, owned by the identity's owner:group, mode 0700 [harness.mktemp] | — |
| Root repair primitives | `chown -R -P -h --from=<uid>` changes only entries owned by that uid (a root-owned file inside stays root's) and changes links themselves, never their targets. Root `stat -c %F` tells a real slug (`directory`) from a symlinked one (`symbolic link`). `find ! -type d -links +1` lists hardlinked files. `fs.protected_hardlinks` is 1 on the kernels tested [harness.root-repair-guards] | §6.8 |
| Plugins dir the login user cannot reach | `stat` fails with `Permission denied`. Root `stat` works (sudoers `/usr/bin/stat`) [harness.stat-fallback] | — |

**BackWPup coexistence**

Checked on the fixtures (which model BackWPup 5.7's install-on-`init`, §3.1) and on real BackWPup **5.7.6** (free + pro-en, from the local apvm cache).

| Item | Result | Plan impact |
|---|---|---|
| Both active | The site boots, and the edition WordPress loads first runs. That is **pro**: `active_plugins` is sorted, and `backwpup-pro/` < `backwpup/` [family.both-active, family.real-backwpup] | Plan notes (§6.9) |
| ⚠ Install routine | Only the loaded edition installs, on the first request after activation, and only when the stored version differs. Pro activated next to free of the same version never installs; at another version it does; free activated next to pro never does [family.install-routine] | §3.1, §6.9 |
| Uninstall one while the other is active | The shared data is kept: real BackWPup keeps all its `backwpup*` options, and free's later uninstall removes every one of them [family.uninstall-shared-data, family.real-backwpup] | — |
| `uninstall --deactivate` (one process) | Exit 0 and the plugin is gone, but the guarded cleanup is skipped (`uninstall-kept`). Deactivate + uninstall as separate processes runs it (`uninstall-cleaned`) [family.one-process-removal] | D9 confirmed |
| Variant markers | `grep -F` of each marker, as the identity, tells pro-de from pro-en [family.variant-markers] | — |

**OpenSSH stderr samples** [ssh.*]

Exit 255 unless noted. P3.3 matches on these phrases, never on whole lines, because wording differs between versions, and only when ssh exits 255.

| Class (§4.5) | macOS, OpenSSH 10.3p1 | Ubuntu, OpenSSH 9.6p1 |
|---|---|---|
| `ssh_auth_failed` | `deploy@127.0.0.1: Permission denied (publickey).` (an unknown user gets the same) | Same |
| `ssh_host_key_unknown` | `No ED25519 host key is known for [127.0.0.1]:2722 and you have requested strict checking.` / `Host key verification failed.` | Same |
| `ssh_host_key_changed` | A `@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@` banner … `Host key verification failed.` | Same, plus `remove with: ssh-keygen -f … -R …` |
| `ssh_unreachable` (refused) | `ssh: connect to host 127.0.0.1 port 1: Connection refused` | Same |
| `ssh_unreachable` (DNS) | `ssh: Could not resolve hostname X: nodename nor servname provided, or not known` | `…: Name or service not known` |
| `ssh_timeout` (connect; recorded, network-dependent) | `ssh: connect to host 192.0.2.1 port 22: Operation timed out` | `…: Connection timed out` |
| `ssh_timeout` (banner) | `Connection timed out during banner exchange` / `Connection to 127.0.0.1 port N timed out` | Same |
| Remote exit 255 | Exit 255, stderr empty: indistinguishable from an SSH failure without the remap | Same |
| Noise (exit 0) | `Warning: Permanently added '[127.0.0.1]:2722' (ED25519) to the list of known hosts.`. A client forwarding an unknown locale (`SendEnv`) gets `bash: warning: setlocale: LC_ALL: cannot change locale (…)` | Same |

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
| sudo / identity | `sudo_missing`, `sudo_password_required`, `sudo_not_allowed`, `sudo_requiretty`, `owner_is_root`, `plugins_dir_mismatch` |
| WordPress | `wp_cli_missing`, `not_wordpress`, `wp_not_installed`, `plugins_dir_not_writable`, `multisite_unsupported` |
| Execution | `site_locked`, `insufficient_space`, `upload_corrupted`, `wp_command_failed`, `verification_failed`, `state_drifted`, `outcome_unknown`, `health_check_failed`, `timeout`, `file_mods_disallowed` |
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
| `--allow-file-mods` | Proceed on sites that set `DISALLOW_FILE_MODS` (install, uninstall, delete; §6.7) |

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
| `site add <ID>` | `--server <ID> --path <WP_ROOT> [--url <URL>] [--wp-cli "<CMD>"] [--run-as auto\|login\|<USER>] [--owner auto\|<USER>[:<GROUP>]] [--ownership mirror\|mirror-root\|check\|off] [--plugins-dir <PATH>] [--allow-root] [--tmp-dir <PATH>] [--timeout <SECS>] [--group <ID>]… [--name <TEXT>] [--notes <TEXT>] [--no-test] [--trust-new-host-key]` | Probes the site by default (§6.2) |
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
| 255 | `wp run` with one target only: apvm could not run the command (SSH, probe, lock, refused). WP-CLI's own 255 arrives as 254 (§6.1) |

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

1. **Typed scripts.** A `Script` is built from compile-time literals and quoted values (POSIX single quotes, `'` → `'\''`). Every command in it gets `</dev/null` so nothing consumes the script stream. Every script starts with `LC_ALL=C; export LC_ALL`, so sudo and coreutils messages are the English ones the classifiers match (sudo keeps `LC_ALL` for the command, §3.5).
2. **Framing.**
   `printf '%s\n' '<nonce>'; LC_ALL=C; export LC_ALL; cd / && <identity> <argv…> </dev/null; rc=$?; [ "$rc" -eq 255 ] && rc=254; exit "$rc"`
   - Only stdout after the per-call random nonce is parsed, which ignores `.bashrc`/MOTD noise.
   - 255→254 makes a real 255 always mean an SSH failure. 254 = remote exit 255: a PHP fatal before WordPress loads, or during activation. Once WordPress has loaded, its default fatal-error handler exits **1** (§3.5). Fatals are never detected by 254 alone, nor by WordPress's message, which a site can change.
   - `cd /` keeps a project `wp-cli.yml` out of play; `--path` and `--url` are always explicit.
3. **Delivery** (D3):
   - **Stdin mode** (everything except uploads): remote command `sh -s`; script bytes on stdin, then EOF. The login shell sees only `sh -s`.
   - **Upload mode:** stdin carries the zip, so the script is the remote command `sh -c '<script>'`.
     - A distinct `UploadScript` type accepts values matching `^[A-Za-z0-9._/+-]+$` only (paths and names). That excludes the `'` and backslash that fish reads as escapes inside single quotes. P0 showed every other printable ASCII character survives all five login shells, so the allowlist keeps a margin.
     - Its templates are single-line (csh/tcsh reject newlines in quotes) and contain neither `\\` nor `\'`.
     - With these rules the quoting survives sh, bash, dash, zsh, fish and tcsh login shells (P0).
4. **Identity prefix** (§6.2): empty, `sudo -n -u U -g G --`, `sudo -n -u U --`, or `runuser -u U -g G --` (root login without sudo). No shell ever runs under sudo or runuser. Root-level commands form a **closed enum**: `stat`, and `chown -R -P -h --from=…` for the opt-in root repair (§6.8).
5. **Hermetic WP-CLI.** Every WP-CLI call is `<prefix> env WP_CLI_CONFIG_PATH=/dev/null <wp_cli> --skip-packages --exec='umask(umask() & ~0<g>);' --path=… --url=… <args>`:
   - `WP_CLI_CONFIG_PATH=/dev/null` keeps the identity's `~/.wp-cli/config.yml` out. It could hold `require:` files or command defaults such as `plugin uninstall: deactivate: true`.
   - `--skip-packages` keeps installed WP-CLI packages out.
   - P0 showed both run code on every call, even with `--skip-plugins` (§3.5).
   - `--exec` relaxes, inside the WP-CLI process, only the group bits `P` grants (`<g>` = `P & 070`, constant digits). So WordPress keeps setgid and group bits at creation, and the identity's own "other" bits stay: a `0027` identity is never made world-readable.
   - It is needed because the default umask differs by login path: `0002` via Debian's `pam_umask`, `0022` under `runuser` (§3.5).
   - Staging needs neither: `mktemp -d` always creates `0700`, and the staged zip never reaches the site.
6. **Limits:** stdout 8 MiB, stderr 1 MiB (with a truncation flag); `kill_on_drop`.
7. **Timeouts:** `ConnectTimeout` 15 s; `ServerAliveInterval=15`, `ServerAliveCountMax=4`; per command 300 s, install 900 s, `wp run --timeout`. A timed-out mutation → re-read → done/failed/unknown.
8. **Retries:** reads only, on transport failures only, twice (1 s, 3 s). Mutations never.
9. **Multiplexing (Unix):** `ControlMaster=auto`, `ControlPath=<control_dir>/%C`, `ControlPersist=60s`, `control_dir` `0700`. Off on Windows, for paths > 100 bytes, or with `multiplex: false`.
10. **Concurrency:** `--jobs` (4) globally plus a per-server semaphore (4) below sshd's `MaxSessions 10`.

### 6.2 Probe and identity resolution (read-only)

| Step | Runs as | What (one round trip each) |
|---|---|---|
| H — host | login user | `id -un`, `id -u`, `id -Gn`, `$SHELL`; `command -v sudo runuser sha256sum shasum openssl`; `sudo -n -l` (lists the user's rules without running anything); `realpath` + `stat -c '%u %g %U %G %a'` (BSD `stat -f` fallback; root `stat` on EACCES) of `<path>/wp-content/plugins` (or the pinned `plugins_dir`) → reference `(U, G, P)`; `command -v <wp_cli>` |
| I — identity | — (pure) | the table below; then a probe of the exact command shape: `<prefix> env WP_CLI_CONFIG_PATH=/dev/null <wp_cli> --skip-packages cli version` |
| W — WordPress | identity | `wp eval` of a constant snippet (`--skip-plugins --skip-themes`): `WP_CLI_VERSION`, `PHP_VERSION`, WP version, `is_multisite()`, `WP_PLUGIN_DIR`, `WP_CONTENT_DIR`, `siteurl`, `home`, `DISALLOW_FILE_MODS`; failures classified |
| O — consistency | identity | `realpath(WP_PLUGIN_DIR)` (from W) must equal the step-H path, else `blocked: plugins_dir_mismatch` until the site pins `plugins_dir`. The identity is never switched to what WordPress reports, because wp-config.php and must-use plugins are under the site's control |
| C — capabilities | identity | `test -w` on the plugins dir and `<content>/upgrade`; `df -Pk` of `remote_tmp_dir` and `WP_CONTENT_DIR`; checksum tool; multisite flag; `siteurl` ≠ `url` → warning |

Identity table (`U:G` = owner:group from step H, or the explicit `owner`, which must then own that dir, else `plugins_dir_mismatch`; `L` = login user, `Lg` = its primary group; "correct at creation" assumes the §6.1 umask):

| `run_as` | Condition | Prefix | Group handling |
|---|---|---|---|
| `auto` | `U == L`, `G == Lg` | none | correct at creation |
| `auto` | `U == L`, `G ≠ Lg` | `sudo -n -u L -g G` if allowed, else none | if no sudo: `chgrp` repair as L when the post-install check finds offenders (needs L ∈ G; none on a setgid tree with umask `0002`, §3.5) |
| `auto` | `U ≠ L`, `U ≠ root` | `sudo -n -u U -g G` (allowed when U ∈ G or a Runas group permits it) | else `sudo -n -u U` + repair |
| `auto` | `U ≠ L`, `L == root`, no sudo | `runuser -u U -g G` | correct at creation |
| `auto` | sudo unusable | — | blocked: `sudo_*` code + exact fix |
| `auto` | `U == root` | — | blocked: `owner_is_root` |
| `login` | — | none (as L) | owner/group repaired afterwards (needs sudo); removal of foreign-owned files blocked |
| `<user>` | — | as `auto` with `U := <user>` | as `auto` |
| any → root | — | needs `allow_root: true`; adds `--allow-root` | — |

sudo classification (§3.5). It runs when the exact-shape probe fails. With `-n`, sudo prints "a password is required" for every refusal except requiretty, so the stderr of the failed command alone cannot tell the cases apart:

| Signal | Code |
|---|---|
| `sudo: sorry, you must have a tty to run sudo` | `sudo_requiretty` |
| `sudo -n -l -u U -g G true` exits 1 with no output (the user has password-less rules, but not this one) | `sudo_not_allowed` (the fix names the missing Runas rule) |
| `sudo -n -l -u U -g G true` succeeds, but the probe still says "a password is required" (the rule needs a password; `listpw=any` lists it anyway) | `sudo_password_required` (the fix: add `NOPASSWD` to that rule) |
| `sudo -n -l -u U -g G true` succeeds and the probe is refused otherwise (a command-restricted rule) | `sudo_not_allowed` (the fix: identity rules need `NOPASSWD: ALL`) |
| `sudo -n -l …` prints "a password is required" (no password-less rule; "not in sudoers" looks the same) | `sudo_password_required` |
| `command -v sudo` empty | `sudo_missing` |

Every refused probe is logged by sudo (syslog/journald; a file in the harness, §3.5). apvm probes each site once per run and never retries a refused sudo.

Recommended sudoers (documented). Identity rules must be `NOPASSWD: ALL` for the identity user: apvm runs `env` and the WP-CLI argv there, never a shell. Root is optional:

```
deploy ALL=(www-data) NOPASSWD: ALL               # sites owned by www-data
deploy ALL=(ftpuser : www-data) NOPASSWD: ALL     # FTP-owned sites; plain (ftpuser) suffices if ftpuser ∈ www-data
deploy ALL=(root) NOPASSWD: /usr/bin/stat         # probe fallback when the login user cannot reach a site
deploy ALL=(root) NOPASSWD: /usr/bin/chown        # only for sites with ownership: mirror-root; chown as root ≈ root
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
  - `Boots` via `wp option get siteurl` with plugins loaded. Any non-zero exit fails it. WordPress's default fatal handler gives exit 1 with `Error: There has been a critical error on this website.` (§3.5). The message is shown, never relied on: a `fatal-error-handler.php` drop-in or `WP_DISABLE_FATAL_ERROR_HANDLER` changes it.
- **Drift:** the read verifying step *i* is also step *i+1*'s precondition; a mismatch with the plan → `state_drifted`.
- **State decides the outcome:** the expected state → `done`, with a warning when the exit code was non-zero or stderr had `^Error:`; an unexpected state → `failed`, whatever the exit code.
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

`DISALLOW_FILE_MODS` (read in probe step W): WP-CLI ignores it (§3.5), so apvm honors the site's policy itself. Install, reinstall, uninstall and delete are `blocked` with `file_mods_disallowed` unless `--allow-file-mods` is given. Activate, deactivate and reset change no files and proceed.

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
  - `find`'s own exit status and stderr are captured: an unreadable subtree counts as an offender, never as clean.
- **When:**
  - after install/replace/reinstall: full check;
  - before uninstall/delete/reinstall: owner/group only, since a foreign-owned tree cannot be deleted by the identity;
  - `apvm wp ownership`: on demand.
- **Repair ladder** (each rung re-checked, stop when clean):

| Rung | When | Command | Privilege |
|---|---|---|---|
| 1 | Only the group differs and the identity owns the files and is in G (or runs with egid G) | `chgrp -R -h -- <gid> T` | none |
| 2 | Owner differs, and the site opted in with `ownership: mirror-root` | the root rung below | root (closed enum) |
| 3 | Group bits missing | `find T -type d -exec chmod g+<bits>[s] {} +` and `find T -type f -exec chmod g+<bits> {} +` (symlinks excluded by `-type`) | none (the identity owns T) |
| — | Still mismatched, or the owner differs on a site without `mirror-root` | item `failed` with `ownership_unfixable`: the entries, and the exact commands an administrator can run; the plugin stays installed | — |

- **Root rung** (opt-in; every precondition re-checked right before the command):
  - Absolute paths only. No `cd` as the login user, who may not reach FTP-style sites at all (§3.5).
  - `sudo -n stat -c %F -- '<plugins_dir>/<slug>'` (root) must print `directory`; a symlinked slug prints `symbolic link` and is refused.
  - No multi-linked files: `find T ! -type d -links +1` must print nothing. Symlink flags do not cover hardlinks.
  - `/proc/sys/fs/protected_hardlinks` must be `1` (it is on current Debian and Ubuntu kernels, §3.5).
  - `sudo -n chown -R -P -h --from=<F>:<FG> -- <uid>:<gid> '<plugins_dir>/<slug>'`, once per foreign owner F the check found. F is never root and never the login user; such trees are reported instead.
  - `--from` changes only entries owned by F, and `-P -h` never follow links. P0 verified both [harness.root-repair-guards].
  - **Residual risk, stated in the docs:** the identity owns `wp-content/` and `plugins/`, so no path check is free of races. A site whose code is not trusted must stay on `mirror`.
- **Guards:**
  - `<plugins_dir>` is the step-H `realpath` (or the pinned `plugins_dir`), and `<slug>` is allowlisted, so the target is always a direct child, never the plugins dir or `/`.
  - Root never deletes.
- **At creation:** every WP-CLI call relaxes the umask's group bits from `P` (§6.1). WordPress then keeps the group bits and setgid itself, and rungs 1 and 3 are the safety net. P0 measured both cases: umask `0022` loses setgid, and the ladder repairs it without root.
- **Policies** (`ownership`, per site):
  - `mirror` (default): check, plus rungs 1 and 3; zero root.
  - `mirror-root`: also rung 2.
  - `check`: report only.
  - `off`: skip everything.

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
| Activate pro while free is active (default) | **Switch:** deactivate free (own process, verified) → activate pro (own process, verified) → boot check; confirmation needed. The boot check is pro's first request as the loaded edition, so its install routine runs there when the stored version differs (§3.1) |
| Activate pro while free is active, `--keep-siblings` | Plain activation in its own process; both end up active. Plan note: only one edition runs per request. That is pro, which loads first. Its install routine runs only if its version differs from the one free stored, so pro-only setup may not happen next to a free of the same version, exactly as from the dashboard. The boot check still runs |
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
| Execution | The arguments become `wp` argv elements, quoted, in a stdin-delivered script: no shell interpretation of `; \| & $() * ~`, any byte but NUL allowed. Runs as the site identity (owner:group), hermetic and with the umask relaxation (§6.1), with apvm's `--path` and the site's `--url` |
| Allowed overrides | `--url=…` (subsite targeting), `--skip-plugins[=…]`, `--skip-themes`, `--user=` (WP user), `--format=…`, `--debug`, `--quiet` |
| Refused params | Every WP-CLI global parameter except the allowed overrides. Notably: `--exec` and `--require` (they run arbitrary PHP and would bypass the classification); `--path` (apvm owns it); `--ssh` and `--http` (would hop elsewhere); `--allow-root` (identity policy); `--prompt` (interactive); `--skip-packages` and `--context` (apvm sets them); a leading `@alias` |
| Refused commands | `shell`, `db cli` (interactive); `server` (long-running); `cli update`, `package …` (WP-CLI itself, not the site); `plugin uninstall … --deactivate` and `plugin deactivate … --uninstall` (break two-phase removal: the error points to `apvm wp uninstall`) |
| Classification | `read-only` = allowlisted command paths (e.g. `plugin list/get/status/is-installed/is-active/path`, `theme list/get/status`, `option get/list/pluck`, `core version/is-installed/check-update/verify-checksums`, `config get/list/has`, `user list/get`, `post list/get`, `post meta get/list`, `term list/get`, `comment list/get/count`, `site list`, `cron event list`, `cron schedule list`, `db size/tables`, `transient get`, `rewrite list`, `role list`, `language core/plugin/theme list`, `cli version/info`, `help`; `search-replace … --dry-run` without `--export` or `--log`, which write files even then, §3.5). Everything else, including `eval`, is `may-write` |
| Confirmation | `may-write` → plan + confirmation (CLI) / `allowWrites: true` (NAPI). The site lock is held for every `wp run`, read-only ones included: they load plugins, which may write |
| Output | One target: raw stdout/stderr, apvm exits with WP-CLI's status (255 arrives as 254), or 255 when apvm could not run the command (scripting: `apvm wp run -s shop -- plugin is-active x && …`). Several targets: a header block per site (or `--json` with `{site, exit, stdout, stderr, truncated}`); exit 0 only if all succeeded |
| Scope | One WP-CLI command per invocation; arguments are taken verbatim, so `run -- plugin delete x` really deletes (documented: use the dedicated verbs for safe lifecycle work) |
| Nature | The classifier is a guard against mistakes, not a sandbox: `eval` runs arbitrary PHP as the identity by design |

### 6.13 Site lock

- **Lock:** `mkdir <remote_tmp_dir>/apvm-lock-<first 16 hex of SHA-256(path)>` (atomic on POSIX), as the identity, holding a `holder` file: local `user@host`, pid, operation, start time (RFC 3339).
- **Held** for every mutating operation (install, activate, deactivate, uninstall, reset, delete, `ownership --fix`) and every `wp run`. apvm's own reads (`list`, `status`, probes) never lock: they skip plugins.
- **Busy:** wait up to `--lock-wait` (60 s) while printing the holder, then `site_locked`.
- **Stale:** a lock older than 30 min (longer than any operation) is removed with a warning; a lock owned by a different user (sticky `/tmp`) is reported instead.
- **Release** on every exit path; the path is validated before `rm -rf`.
- **Manual:** `apvm site unlock <ID>` shows the holder and removes a stuck lock (as the identity) after confirmation.

### 6.14 Recovery

- **Activation breaks the site:** `health_check_failed`, and the output prints `apvm wp deactivate <slug> -s <site> --no-load`. That command runs `--skip-plugins=<slug>`, so the deactivation hook does not run, which the output also says.
- **Reads survive a broken plugin:** they use `--skip-plugins --skip-themes`, so a regular plugin that breaks the site does not stop them.
- **They do not survive a broken must-use plugin or drop-in:** those still load under `--skip-plugins` (§3.5), so a fatal there fails every WP-CLI command.
  - `status` then reports `undetermined` and mutations `wp_command_failed`, with the error excerpt.
  - apvm cannot repair this: it never touches must-use plugins or drop-ins.

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
| `Site` | **`id`**, **`server`**, **`path`**, `url`, `wp_cli` (`["wp"]`), `run_as` (`auto`), `owner` (`auto` = owner:group of the plugins dir, §6.2), `ownership` (`mirror`; `mirror-root` allows the root rung), `plugins_dir` (pins a non-standard `WP_PLUGIN_DIR`), `allow_root` (`false`), `remote_tmp_dir` (`/tmp`), `command_timeout_secs`, `name`, `notes` |
| `Group` | **`id`**, **`sites`**, `notes` |

| Value | Allowlist | Goes to |
|---|---|---|
| ids | `^[a-z0-9][a-z0-9._-]{0,63}$`, unique per kind | local |
| `host` | `[A-Za-z0-9._:-]`, no leading `-`, ≤ 253 | ssh argv |
| `user`, `run_as`, `owner` parts | `[A-Za-z0-9._-]`, no leading `-`, ≤ 64 | ssh argv / remote |
| `port` | 1–65535 | ssh argv |
| `proxy_jump` | comma list of `[user@]host[:port]` | ssh argv |
| `path`, `plugins_dir` | absolute POSIX, no `..`, no NUL/CR/LF/backslash, ≤ 4096 | remote |
| `remote_tmp_dir` | absolute, `^[A-Za-z0-9._/+-]+$` (the `UploadScript` allowlist, §6.1, so the inventory never accepts what an upload would refuse), no `..`, ≤ 4096 | remote |
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
2. **Parsing:** reads parse framed `--format=json`.
   - PHP notices or deprecations before or after the JSON payload are skipped with a warning. Example: WP-CLI 2.12.0 on PHP 8.5 with `display_errors` on prints one before it (§3.5).
   - Unparseable output → `wp_command_failed` with an `escape_debug` excerpt.
   - Notices never read as "not installed".
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
| `wp run`: argv-only, every global parameter refused except the allowed overrides (no `--exec`/`--require`), refused commands, read-only allowlist, confirmation / `allowWrites`, identity-run, lock | T1, T7 |
| Hermetic WP-CLI: no user config, no packages (§6.1); the identity is derived from the plugins dir's `stat`, never from WordPress's own report (§6.2) | T1, T4 |
| Zip rules (§6.10) | T2 |
| `sudo -n` only; no shell under sudo or runuser, ever (`env` + the WP-CLI argv); root commands are a closed enum (`stat`; `chown -R -P -h --from=…`, opt-in per site, with the §6.8 preconditions); root never deletes; `chgrp` without root first; `mirror` and `check` → zero root | T3 |
| Nonce framing, output caps, strict parsers, `escape_debug`; `ssh_*` classes only when ssh exits 255 (remote stderr shares the stream) | T4 |
| No stored secrets; inventory and logs `0600`; no secrets in argv | T5 |
| `BatchMode=yes`; the user's known_hosts; `accept-new` only per run; `no` never | T6 |

**Known limits:**
- apvm verifies against WordPress's own state, so a site's must-use plugin or drop-in can falsify what it reads. Reads skip regular plugins, nothing more.
- Refused sudo probes are logged on the server; operators should expect those entries (§6.2).
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

- **Resolve vs reject:** promises **resolve with a report** whenever work ran (per-item `outcome`/`code`, the snake_case codes of §4.5). They **reject** only on bad input or an invalid inventory (`err.code` `InvalidArg` / `InventoryInvalid` / `ConfirmationRequired` for `runWp` may-write without `allowWrites` / `Undetermined` for `isInstalled`/`isActive` when the site can't be checked), via the `spawn_future_with_callback` route `ApvmCache` already uses. Rejection codes are PascalCase, like the existing NAPI errors; they are a different layer from the item codes.
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

Docker E2E setup: `tests/e2e/sites/` (built in P0; full reference in its `README.md`).
- **Driving it:**
  - `harness.sh up|down|env|ssh|reset|logs`.
  - One compose project per checkout.
  - Tests read `eval "$(harness.sh env)"` (`APVM_E2E_SITES=1`, SSH host/port/key/known_hosts/config).
  - `harness.sh reset [SITE…]` restores a site's files and database, and the shared state checks touch, in ≈ 0.3 s.
  - One `verify.sh` at a time (a lock); it resets before every group and on interrupt.
- **Users** (key-only):
  - `deploy`: the documented least-privilege sudoers of §6.2, plain `(ftpuser)` form.
  - `mixed`: the explicit `(ftpuser : www-data)` form, plus a password rule `sudo -n -l` still lists.
  - `plain`: no sudo.
  - `pwsudo`: sudo with a password.
  - `ttyreq`: NOPASSWD with requiretty.
  - `fishy`, `zshy`, `tcshy`, `dashy`: unusual login shells (`www-data` rights).
  - `noisy`: `.bashrc` prints to stdout and stderr.
  - `ftpuser`: no sudo, member of `www-data`.
  - `root`: for `runuser`.
- **Sites:**
  - `single`: `www-data:www-data`, plus a must-use plugin and a drop-in.
  - `multi`: a subdirectory multisite with subsite `sub1`.
  - `ftpsite`: `ftpuser:www-data`, dirs `2775`, files `0664`, in a `0750 ftpuser:www-data` home, so `deploy` needs root `stat`.
- **Samples:** `verify.sh` saves OpenSSH stderr samples under `<results>/ssh-stderr/` (`APVM_E2E_RESULTS_DIR`, default `.state/results`).

Fixture plugins (directories under `fixtures/plugins/`, zipped at test time with one top-level folder; the harness uses `lib/fixture_zip.py`, Rust tests the `zip` crate):

| Fixture | Purpose |
|---|---|
| `basic` | Activation sets an option, uninstall removes it |
| `guarded` + `guarded-pro` | BackWPup-style family: shared main-file guard, class and data; no activation hook but an install routine on `init` when the stored version differs (§3.1); an uninstall guard; events are logged in `apvm_fx_guarded_log`. `guarded-pro` (main file `guarded.php`, like `backwpup-pro/backwpup.php`) has variants `de`/`en`, told apart by `inc/License.php` (`fixtures/variants/`). Tests inject `PluginHints`. Used for coexistence (both installed, both active), switch, and independent removal |
| `network-only` | `Network: true` activation |
| `parent` / `child` | `Requires Plugins` ordering |
| `fatal-activate` | Activation fails |
| `fatal-load` | Site breaks while it is active (health check, recovery); can be reactivated after a no-load deactivation |
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
| Remote | POSIX `sh`, coreutils/findutils (`mktemp`, `tee`, `stat`, `find`, `chown`, `chgrp`, `chmod`, `df`), WP-CLI ≥ 2.12.0 (the version P0 verified; older ones get a probe warning), `sudo` when owner/group ≠ the SSH user's, optional `runuser` | documented |
| Local | OpenSSH client (≥ 7.6 for `--trust-new-host-key`) | Windows 10+ ships one |
| E2E | Docker Engine ≥ 25 + Compose ≥ 2.23.1 (`configs.content`); `wordpress:7.1.3-php8.4-apache`, `mariadb:12.3.3` (LTS) and the Dockerfile frontend 1.27.1, pinned by digest; WP-CLI 2.12.0 (SHA-512); host side: bash ≥ 3.2, OpenSSH, python3 ≥ 3.9 | PHP 8.4, not 8.5: WP-CLI 2.12.0 raises deprecations on 8.5 (§3.5) |

---

## 14. Phases

**Definition of done (every phase):** its **Goal** is met, its **Tests** exist and pass, and it:
- passes the full validation suite: `cargo fmt --check`, `clippy -D warnings`, `cargo test`, `cargo doc --no-deps`, plus `npm test` + `npm run typecheck` when napi changes;
- updates `SKILL.md` + `references/sites.md` in the same change when it touches the CLI surface (CLAUDE.md);
- documents every public item.

Phase-specific extra conditions appear as **Done when**.

Tracks: **lib** = `apvm-sites`/core, **cli**, **napi**.

### P0 — E2E harness and ground truth ✅ (implemented 2026-10-09, audited 2026-10-11, CI green 2026-10-11)
- **Goal:** a reproducible WordPress-over-SSH target (users, sites and fixtures of §12), with every §3.5 item verified.
- **Changes:**
  - `tests/e2e/sites/`:
    - `compose.yaml`, `harness.sh`, `verify.sh`, `README.md`, `.gitignore`;
    - `lib/{common.sh, verify.sh, fixture_zip.py}`;
    - `checks/{harness,sudo,perms,shells,wpcli,family,ssh}.sh`;
    - `target/{Dockerfile, entrypoint.sh, apvm-e2e.sh, sudoers, sshd_config, vhosts.conf}`;
    - `fixtures/{plugins,variants}/…`;
  - `.github/workflows/ci.yml` job `e2e-sites` (`harness.sh up` + `verify.sh`, results uploaded; gates `CI`);
  - a **temporary** `push` trigger for `feat/site-deployment` in `ci.yml`, so CI runs on this branch before its PR exists (`commit-artifacts` stays develop-only); P12 removes it;
  - §3.5 results recorded in this plan, which is adjusted in revisions 4 and 5.
- **Done when:** the harness is healthy locally and in CI; `verify.sh` passes, or each deviation is recorded and the plan adjusted.
- **Result:** 43 asserted checks pass on macOS and Ubuntu clients, plus the optional real-BackWPup check locally; 2 INFO checks record versions and a network-dependent timeout. The deviations are marked ⚠ in §3.5 and resolved in revisions 4 and 5. CI (run 38189801662) passes every job, `e2e-sites` included, with the same 43 checks; its first run caught two bugs, both fixed (revision 6).
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
- **Tests:**
  - render goldens; adversarial round-trips through real `sh -s` and `sh -c` (Unix); outer layer through the locally available shells (skipped if absent);
  - `UploadScript` accepts only `^[A-Za-z0-9._/+-]+$` values (so `'` and `\` are rejected at construction); its templates are single-line and contain neither `\\` nor `\'`;
  - every script starts with `LC_ALL=C`; identity commands never contain a shell (`sh -c`) under sudo or runuser;
  - the hermetic WP-CLI argv: `env WP_CLI_CONFIG_PATH=/dev/null <wp_cli> --skip-packages --exec='umask(umask() & ~0<g>);'`, with `<g>` from the plugins dir mode (golden per mode).
- **Depends:** P1.1 · **Parallel:** P1.2–P1.5, P2.x, P0

#### P3.2 — Transport trait, capture, local shell, fake
- **Goal:** run scripts and capture results safely.
- **Changes:** `remote/{transport,capture,local}.rs`, `#[cfg(test)] fake.rs`.
- **Tests:** caps; timeout kills the child; 20 MB stdin hash round-trip; nonce behind noise; 255 remap.
- **Depends:** P3.1 · **Parallel:** P4.1, P1.x, P2.x

#### P3.3 — SSH transport
- **Goal:** the real transport with actionable errors.
- **Changes:** `remote/{ssh,classify}.rs`; `tests/support/fake-remote/fake-ssh`.
- **Tests:** argv goldens across precedence; Windows argv; classification table from P0's stderr samples (§3.5, both OpenSSH versions; noise lines ignored; `ssh_*` only when ssh exits 255, so remote stderr with the same phrases never matches); hermetic fake-ssh; E2E (connect, unknown host key, `accept-new`, wrong key).
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
- **Tests:** argv goldens per command, **asserting the banned flags never appear**; parser fixtures (clean, notice-prefixed incl. the PHP 8.5 deprecation line, notice-suffixed (shutdown), HTML error, truncated, empty, unknown status, single-file plugin, must-use and drop-in rows).
- **Depends:** P3.1 · **Parallel:** P3.2–P3.4

#### P4.2 — lib: probe, identity (owner + group), session
- **Goal:** §6.2 end to end.
- **Changes:** `wp/probe.rs`, `remote/identity.rs` (decision table), `session.rs`; `tests/support/fake-remote/{fake-wp, fake-sudo}`.
- **Tests:**
  - exhaustive identity table (including every group row); probe over `FakeTransport` for every §6.2 failure code;
  - E2E:
    - `deploy` → `www-data` via `sudo -u -g`; `deploy` on `ftpsite` → `ftpuser:www-data` (root `stat` for the probe);
    - `ftpuser` login (no sudo) → no prefix; `chgrp` only when the post-install check finds offenders (none with umask `0002` on the setgid tree); `root` → `runuser -u -g`;
    - `plain`/`pwsudo` → `sudo_password_required`; `mixed` targeting `www-data` (listed by `-l`, needs a password) → `sudo_password_required`; `ttyreq` → `sudo_requiretty`; `deploy` with `run_as` = a user outside its rules → `sudo_not_allowed`;
    - a `WP_PLUGIN_DIR` that differs from `<path>/wp-content/plugins` (set in wp-config.php) → `plugins_dir_mismatch`, and the identity is unchanged; with `plugins_dir` pinned → probe passes;
    - an explicit `owner` that does not own the plugins dir → `plugins_dir_mismatch`;
    - a hostile `~/.wp-cli/config.yml` and package for the identity have no effect on the probe;
    - `fishy`/`noisy` work.
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
  - transport loss → done/failed/unknown; the expected state with a non-zero exit or `Error:` → done + warning; an unexpected state → failed, whatever the exit code;
  - drift; health failure; hermetic fake-wp run.
- **Depends:** P6.1, P5.1 · **Parallel:** P8.1

#### P6.3 — Ownership, group, group access
- **Goal:** §6.8 check + repair ladder.
- **Changes:** `ops/ownership.rs`; `Own`/`Ownership` steps wired into plan/apply; `ownership` site setting.
- **Tests:**
  - goldens (guards adjacent, `-R -P -h`, numeric ids, `--`);
  - ladder table (group-only/member → `chgrp`; owner → root only with `mirror-root`, else `ownership_unfixable` with the commands; modes → `chmod g+`; unfixable);
  - root-rung preconditions: `--from` is never root or the login user; a symlinked slug, a multi-linked file or `protected_hardlinks` ≠ 1 → refused, nothing run;
  - `mirror`/`mirror-root`/`check`/`off`; single-file plugin; a `find` error counts as an offender.
  - **E2E:**
    - `mirror-root`: a plugin tree with links to the harness sentinel and a root-owned file inside is repaired; the links' targets and the root-owned file keep their owners;
    - `mirror-root` on `ftpsite` (the login user cannot reach it): the repair works through absolute paths and root `stat`;
    - a hardlinked file or a symlinked slug → refused with `ownership_unfixable`;
    - `mirror` (default): a foreign-owned tree is reported with the commands, and no root command runs;
    - an `ftpsite` tree installed by `ftpuser` with umask `0022` (group `ftpuser`, no setgid) is repaired to `ftpuser:www-data` + `g+rw`(+setgid) without root (as `ftpuser` ∈ `www-data`);
    - with the `--exec` umask relaxation (§6.1), the same install needs no repair.
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
- **Tests:** hermetic report; E2E `--fix` on `ftpsite` (no root, then `mirror-root` through absolute paths) and `single` (`mirror` reports, `mirror-root` repairs); `--dry-run`; `site unlock` shows the holder, asks, removes; nothing to unlock → exit 0.
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
  - fake: policy matrix; version mismatch → `verification_failed`; cleanup + lock release on every failure path; `replace` keeps active; `DISALLOW_FILE_MODS` → `file_mods_disallowed` unless `--allow-file-mods`.
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
- **Tests:** classification tables (read-only paths at depth 1–3, `--dry-run` cases incl. `search-replace --dry-run --log/--export` → may-write, every global parameter outside the allowed overrides refused incl. `--exec`/`--require` in any position and `--exec=…` forms, refused commands, `@alias`, two-phase violators); every `wp run` takes the lock; single-target exit codes (WP-CLI's, 254, 255 = not run); hermetic run with arguments holding quotes, backslashes, newlines and `$()` arriving verbatim; a read-only command also acquires and releases the lock, waits for a busy one and then fails with `site_locked`; the lock is released on every exit path; timeouts; per-site results.
- **Depends:** P6.4 · **Parallel:** P7.x, P8.x

#### P9.2 — cli: `apvm wp run`
- **Goal:** the command.
- **Changes:** `commands/wp/run.rs` (trailing args after `--`, single-target passthrough of output and exit code, multi-target blocks/`--json`); `SKILL.md`.
- **Tests:** clap trailing args; confirmation for may-write; exit-code passthrough; E2E `option get siteurl`, `plugin is-active` exit codes, `eval` with a backslash/newline payload.
- **Depends:** P9.1, P7.1 · **Parallel:** P9.3

#### P9.3 — napi: `runWp`, `classifyWp`
- **Goal:** passthrough from Node.
- **Changes:** `sites.rs`, types, tests.
- **Tests:** `classifyWp` table parity with the library (incl. `--exec`/`--require` refused); may-write without `allowWrites` → `ConfirmationRequired`; refused → `InvalidArg`; hermetic run with verbatim tricky arguments.
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

### P12 — Remove the temporary CI trigger
- **Goal:** CI triggers go back to `develop` pushes and PRs to `develop` only.
- **When:** right after `feat/site-deployment` is merged into `develop`, as a one-file PR to `develop`. Until then, once a PR to `develop` is open, every push runs CI twice (`push` + `pull_request`); that is expected.
- **Changes:** `.github/workflows/ci.yml`: drop the `feat/site-deployment` entry and its comment from `on.push.branches` (back to `branches: [develop]`); mark P12 done here.
- **Tests:** none (workflow config only).
- **Done when:** `grep -n 'feat/site-deployment' .github/workflows/ci.yml` finds nothing, and the next CI run on `develop` passes.
- **Depends:** P11 (branch merged) · **Parallel:** —

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
all ─ P11 ─ P12
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
