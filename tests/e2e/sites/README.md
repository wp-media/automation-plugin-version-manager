# Site-deployment E2E harness

A reproducible WordPress-over-SSH target for the site-deployment work ([plan](../../../docs/plans/site-deployment.md), phase P0). It runs in Docker:

- **db:** MariaDB 12.3.3 (LTS).
- **target:** WordPress 7.1.3 on PHP 8.4, WP-CLI 2.12.0, OpenSSH and sudo, with three sites, twelve login users and fixture plugins.

`verify.sh` re-checks, over real SSH, the facts the plan's remote design relies on (plan §3.5). Almost every check asserts; the few INFO checks only record versions or network-dependent behavior.

## Quick start

```sh
tests/e2e/sites/harness.sh up        # build + start (≈ 15 s once built), write .state/
tests/e2e/sites/verify.sh            # all checks (≈ 2 min); -v prints every observation
tests/e2e/sites/harness.sh ssh deploy
tests/e2e/sites/harness.sh down      # discard everything
```

**Requirements:**
- Docker Engine ≥ 25 with Compose ≥ 2.23.1 (`configs.content`).
- bash ≥ 3.2, an OpenSSH client and python3 ≥ 3.9.
- Works on macOS and Linux.

**Always go through `harness.sh`.** It creates the SSH key the target trusts, so a bare `docker compose up` fails on purpose.

**Each checkout gets its own compose project** (`apvm-e2e-<checksum of the path>`), so worktrees never touch each other's containers. Two harnesses running at once need different ports.

## Commands

| Command | Does |
|---|---|
| `harness.sh up` | Builds the target and starts both services. Waits until they are healthy, writes `.state/`, smoke-tests SSH, then writes `.state/env`. Running it again is a no-op. |
| `harness.sh down` | Removes the containers and `.state/env`. All site state lives in the containers, so it is gone. |
| `harness.sh env` | Prints the exports tests use: `eval "$(tests/e2e/sites/harness.sh env)"` sets `APVM_E2E_SITES=1`, `APVM_E2E_SSH_HOST`, `APVM_E2E_SSH_PORT`, `APVM_E2E_SSH_KEY`, `APVM_E2E_KNOWN_HOSTS`, `APVM_E2E_SSH_CONFIG` and `APVM_E2E_HTTP_PORT`. It fails while the harness is down. |
| `harness.sh ssh USER [CMD]` | SSH as a harness user (aliases `apvm-e2e-<user>` in `.state/ssh_config`). |
| `harness.sh reset [SITE…]` | Restores sites (default: all), files and database, to their post-install snapshot in ≈ 0.3 s. Also restores the shared state checks may touch (see below). |
| `harness.sh logs` | Container logs, including sshd's. |
| `verify.sh [-v] [GROUP…]` | Runs the checks of `harness sudo perms shells wpcli family ssh`. Writes `report.md` and `ssh-stderr/*.txt` to the results dir. Exits 1 if any check fails. |

**What a reset restores:**
- each site's files (owners and modes included) and database;
- every user's `~/.wp-cli`;
- `ftpuser`'s home mode;
- sudo timestamps;
- the chown sentinel;
- `/tmp/apvm.*` staging dirs, `/tmp/apvm-lock-*` locks and `/tmp/apvm-e2e-*`.

**One `verify.sh` at a time per checkout.** It takes a lock in `.state/`. While it runs, `harness.sh reset` and `down` refuse. It resets the sites before every group and when it ends. On SIGINT or SIGTERM it resets them and exits 130.

**Environment:**
- `APVM_E2E_SSH_PORT` (default `2722`) and `APVM_E2E_HTTP_PORT` (default `8722`), read by `up`; `verify.sh` uses what `up` recorded in `.state/env`.
- `APVM_E2E_RESULTS_DIR`: where `verify.sh` writes its results (default `.state/results`). CI points it outside the repo, because `upload-artifact` skips dot-folders.
- `APVM_E2E_WORDPRESS_IMAGE`: build on another `wordpress:*-apache` image. Example, PHP 8.5: `wordpress:7.1.3-php8.5-apache@sha256:c12bed9c5af9d76261221c2af3a6216a074dd80b4fc2459588b3cbf2dd8e1798`.
- `APVM_E2E_BACKWPUP_FREE_ZIP` + `APVM_E2E_BACKWPUP_PRO_ZIP`: enable `family.real-backwpup` with real builds, e.g. `~/.apvm/cache/backwpup/releases/5.7.6/backwpup-{free,pro-en}-5.7.6.zip`.

## Sites

| Site | Path | URL | Owner:group | Layout |
|---|---|---|---|---|
| `single` | `/var/www/single` | `http://single.test` | `www-data:www-data` | Official image modes (755/644), plus a must-use plugin and an `advanced-cache.php` drop-in |
| `multi` | `/var/www/multi` | `http://multi.test` | `www-data:www-data` | Subdirectory multisite with subsite `sub1` |
| `ftpsite` | `/home/ftpuser/public_html` | `http://ftpsite.test` | `ftpuser:www-data` | Dirs `2775`, files `0664`. The home dir is `0750 ftpuser:www-data`, so Apache can reach it but `deploy` needs root to `stat` it |

**Databases** are `wp_<site>` (user `wp`, password `wp`) on a tmpfs. A restarted database restarts the target, which restores the sites from their snapshots.

**Apache** serves each site by name, for manual inspection only; no check uses HTTP. Pick a site with a `Host` header: `curl -H 'Host: single.test' http://127.0.0.1:8722/`. Subsite URLs on `multi` need rewrite rules, which are not configured.

## Users

All of them log in with the harness key only. sudo logs to `/var/log/apvm-e2e-sudo.log` in the target.

| User | Shell | sudo | Purpose |
|---|---|---|---|
| `deploy` | bash | `(www-data)`, `(ftpuser)` NOPASSWD; root only for `chown`, `stat` | The plan's documented least-privilege operator (§6.2), with the plain `(ftpuser)` Runas form |
| `mixed` | bash | `(ftpuser : www-data)` NOPASSWD; `(www-data)` with a password | The explicit Runas form, and a password rule `sudo -n -l` still lists |
| `plain` | bash | none | No sudo at all |
| `pwsudo` | bash | `ALL`, with a password | `sudo -n` must fail |
| `ttyreq` | bash | `(www-data)` NOPASSWD + `requiretty` | `sudo -n` without a TTY must fail |
| `fishy`, `zshy`, `tcshy`, `dashy` | fish, zsh, tcsh, dash | `(www-data)` NOPASSWD | Login shells that parse the remote command |
| `noisy` | bash | `(www-data)` NOPASSWD | `.bashrc` prints to stdout and stderr on every command |
| `ftpuser` | bash | none | Owns `ftpsite`; primary group `ftpuser`, member of `www-data` |
| `root` | bash | — | Exercises the `runuser -u U -g G` identity path |

## Fixture plugins

Fixture plugins live in `fixtures/plugins/<slug>/`. `lib/fixture_zip.py` zips them with one top-level `<slug>/` folder:
- `--variant` overlays `fixtures/variants/<slug>/<variant>/`;
- `--version` rewrites the `Version:` header;
- hidden files are skipped and symlinks refused;
- the same inputs give the same bytes on the same Python/zlib build.

| Slug | Purpose |
|---|---|
| `basic` | Counts activations and deactivations in options; `uninstall.php` deletes them |
| `guarded` + `guarded-pro` | Mirrors BackWPup 5.7 free/pro. See below. |
| `network-only` | `Network: true` |
| `parent` / `child` | `Requires Plugins: parent` |
| `fatal-activate` | The activation hook throws |
| `fatal-load` | Activates cleanly, then breaks every request while active (health check, recovery). It can be reactivated after a no-load deactivation |
| `writes-content` | Activation writes into `wp-content/` (side-effect ownership) |

`guarded` and `guarded-pro` share:
- a main-file guard and a class;
- no activation hook, but an install routine that runs on `init` when the stored version differs from the loaded edition's;
- data;
- an `uninstall.php` that cleans the shared data only when no edition is loaded.

Their events go to `apvm_fx_guarded_log`. `guarded-pro` has variants `de`/`en`, told apart by `inc/License.php`.

## How `verify.sh` talks to the target

It uses the plan's execution contract (§6.1), so the checks exercise the shapes apvm will use:
- **Delivery:** scripts go to `sh -s` on stdin, behind a random nonce line, with `LC_ALL=C`. A check can require that the nonce came back (`expect_ran`), so an SSH failure never passes as a remote result.
- **Commands:** WP-CLI runs from `/` with stdin closed, as the site identity (`sudo -n -u OWNER -g GROUP --`), isolated from WP-CLI user configs and packages (`env WP_CLI_CONFIG_PATH=/dev/null wp --skip-packages`). Exit 255 maps to 254.
- **Uploads:** they follow the staging design (§6.6): `mktemp -d` as the identity, then the bytes over the SSH stdin into `sh -c '… tee …'`, then a SHA-256 comparison.

The helpers live in `lib/verify.sh`, and each check group is a file in `checks/`.

**Checks that need an SSH client home of their own** (`checks/ssh.sh`: what a jump host reads from `~/.ssh`, `accept-new` with a known_hosts file that keeps nothing) run the target's own OpenSSH client inside the container, as a throwaway user `apvm-e2e-client` that the check creates and removes (`in_target_client`, scripts in `lib/client_*.sh`). `ssh.system-config-include` runs `lib/client_system_config.sh` as root in the target: it changes the owner and mode of the target's `/etc/ssh/ssh_config` for a throwaway user `apvm-e2e-sysconf` (and, in that user's home, the mode of a file of its own, for Debian's exception to the include check), and restores the file (and removes the users) when it ends; its backup is `/etc/ssh/ssh_config.apvm-e2e-orig`, outside what `harness.sh reset` deletes, so a run killed before its cleanup is undone by the next one. ssh takes `~` from the password database, never from `HOME`, so this is the only way to test them without writing the real `~/.ssh` of whoever runs `verify.sh`. Checks that run this machine's client only read `~/.ssh` (`-F /dev/null` or a generated config keeps the user's ssh_config out) and write only to their scratch directory (`ssh.agent-isolation` also starts a throwaway `ssh-agent` on a socket in `/tmp/apvm-e2e-agent.*`, and stops it and removes the directory before it ends).

## Rust E2E tests

`crates/sites/tests/ssh_e2e.rs` runs apvm's SSH transport (plan P3.3) against the harness through this machine's ssh client: a script and an upload, ssh's own `UpdateHostKeys` default when apvm uses ssh's own known_hosts files (in the sandbox's home), an unknown host key refused and then recorded with `accept-new`, a changed key, a key marked `@revoked`, a wrong key, a key file others can read, a closed port, the target as its own jump host, an ssh_config that tries to take the session away (`SessionType none`, `StdinNull yes`, `ForkAfterAuthentication yes`, a 1 s `ChannelTimeout`), a timed-out call through a jump host that must leave no ssh process (`pgrep`), and a transport without `ssh_config_file`, which includes the sandbox's `~/.ssh/config` and this machine's system ssh_config (read only). It is gated on the variables `harness.sh env` prints:

```sh
tests/e2e/sites/harness.sh up
eval "$(tests/e2e/sites/harness.sh env)" && cargo test -p apvm-sites --test ssh_e2e
```

Every transport there gets a temporary home, its own empty ssh_config file and temporary known_hosts files; each test checks that the generated configuration names them before ssh runs. CI's `e2e-sites` job runs these tests after `verify.sh`.

**Adding a check:**
1. Write a `check_*` function. It returns 0 (PASS), 1 (FAIL), 2 (INFO: recorded, no expectation) or 3 (SKIP). It asserts with `expect_eq`, `expect_match` and `expect_ran`, and records observations with `note`.
2. Register it in the file's `group_*` function.
3. Start with `reset_sites SITE` if it changes a site.

## Maintenance

- **Pins:** the base images and the Dockerfile frontend are pinned by digest, and WP-CLI by version + SHA-512. Debian packages are not pinned (superseded versions leave the mirrors); `harness.versions` records the sudo and OpenSSH versions each run used. After a bump:
  1. Run `verify.sh`.
  2. Record any changed fact in the plan's §3.5 results.
- **Linting:** CI runs `shellcheck -x` on every script. Locally, also lint `target/Dockerfile` with hadolint and the workflow with actionlint.
- **Security:** everything here is test-only.
  - The published ports bind to loopback. Local processes can still reach the containers' bridge network, where WordPress logins are `admin`/`admin`.
  - The root SSH login accepts only the per-checkout key in `.state/`, which is git-ignored.
  - The database credentials are fixed.
