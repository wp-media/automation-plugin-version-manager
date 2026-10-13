# apvm site inventory — servers, sites, groups

apvm keeps the WordPress sites it can deploy to in a **site inventory**: SSH
**servers**, the WordPress **sites** on them, and **groups** of sites, plus
default SSH settings every server inherits. This release ships the server,
site and group commands (`apvm server …`, `apvm site …`, `apvm group …`);
the `wp` commands come later.

No inventory command connects to your servers: it reads or edits the file
only, and no SSH connection is made (the connection tests, `apvm server
test`, `apvm site test` and the probe on `site add`, come with a later
release). Removing a site, a server or a group changes nothing on the
machine itself. Like every apvm command run on a
terminal, it may check GitHub for a newer apvm in the background
(`APVM_NO_UPDATE_CHECK=1` turns that off).

## The inventory file

| Topic | Behavior |
|-------|----------|
| Location | `~/.apvm/inventory.json`. **`APVM_INVENTORY=<path>`** replaces it for one invocation (tests, CI, a second set of servers); an empty value means the default; a relative path is relative to the current directory |
| Missing file | An empty inventory. Reads (`list`, `show`, `server defaults` without flags) never create it; the first change does. A change refused after taking the lock (invalid value, unknown id) writes no inventory, but leaves the directory it created (`0700`) and the empty lock file (`inventory.json.lock`, `0600`); contradicting flags (`--user u --unset user`) touch nothing |
| Permissions | The file and its lock file are `0600`; a missing directory is created `0700` |
| Safe edits | Every change is one locked read-modify-write (lock `inventory.json.lock`, written atomically), so concurrent apvm processes never lose each other's edits. A refused change writes nothing. A change that changes nothing (`--user` to the same value) writes nothing |
| Locked | Writers wait at most **10 s** for another process's lock, then fail: `cannot lock the inventory with <path>.lock: another process held it for 10 s` + `edits hold the lock for milliseconds, so that process is likely stuck: stop it, then retry` (nothing written). Reads take no lock |
| Invalid file | Every command fails (exit 1), listing each problem with its location (`servers[1].host: …`), and the file is **never rewritten**: fix it by hand. Unknown fields are errors |
| Newer version | A file written by a newer apvm that this one can fully read is **read-only**: `list`/`show` work with a `warn` line; every change fails with `… it is read-only: upgrade apvm to change it`. One it cannot fully read fails every command (`… cannot be read: …; upgrade apvm`) |
| Symlinks | The file and its lock file must be regular files: a symbolic link (even a dangling one), a directory or a special file is refused (`refusing <path>: it is a symbolic link, not a regular file`) |
| No secrets | Only key *paths* are stored, never passwords or key contents |

## `apvm server` — SSH hosts

| Command | Arguments | Notes |
|---------|-----------|-------|
| `apvm server defaults` | `[SSH flags] [--ssh-binary <PATH>] [--unset <FIELD>]… [--json]` | No change flags = show. `--key` is the **one main key** every server uses unless it sets its own |
| `apvm server add <ID>` | `--host <HOST> [SSH flags] [--name <TEXT>] [--notes <TEXT>]` | `<HOST>` may be an `~/.ssh/config` alias; `--key` overrides the default key for this server |
| `apvm server edit <ID>` | the `add` flags (`--host` optional) + `[--unset <FIELD>]…` | At least one change (else exit 2). The id cannot change |
| `apvm server list` | `[--json]` | Inventory order |
| `apvm server show <ID>` | `[--json]` | The record plus the **effective** (merged) SSH settings and where each comes from |
| `apvm server remove <ID>` | `[--cascade] [-y\|--yes]` | Refused while sites use it, unless `--cascade` (its sites are removed too and leave their groups). Asks first unless `-y` |

SSH flags (the same for `defaults`, `add` and `edit`): `[--user <USER>]
[--port <N>] [--key <PATH>] [--connect-timeout <SECS>]
[--jump <[USER@]HOST[:PORT]>[,…]] [--known-hosts <PATH>] [--ssh-config <PATH>]
[--multiplex | --no-multiplex]`. A server's own value wins over the default.

### Flags

| Flag | Meaning | Rule (checked by apvm; exit 1 otherwise) |
|------|---------|------------------------------------------|
| `<ID>` | Server id | `^[a-z0-9][a-z0-9._-]{0,63}$`, unique |
| `--host` | Host name, IP address or ssh_config alias | `[A-Za-z0-9._:-]`, no leading `-`, ≤ 253 |
| `--user` | SSH login user | `[A-Za-z0-9._-]`, no leading `-`, ≤ 64, not all digits, not `.`/`..` |
| `--port` | SSH port | 1–65535 (a non-number or > 65535 is exit 2) |
| `--key` | Private key file on this machine | `[A-Za-z0-9._/\:+-]`, `~` only as a leading `~/` (or `~\`), no `.`/`..`/empty component, no leading `-` |
| `--known-hosts` | known_hosts file to use instead of yours | as `--key`; never `none`, `NUL` or `/dev/…` (they would let any host key pass) |
| `--ssh-config` | ssh_config file to use instead of yours | as `--key` |
| `--jump` | Jump hosts, as `ssh -J` takes them | ≤ 8 comma-separated `[user@]host[:port]` hops, ≤ 1024 bytes, not `none`; a bracketed IPv6 address only in the last hop |
| `--multiplex` / `--no-multiplex` | Reuse one SSH connection per server (OpenSSH multiplexing; Unix only) | mutually exclusive (exit 2) |
| `--name` / `--notes` | Display name / free-form notes | 1–128 / 1–1024 characters; no control or invisible formatting characters (zero-width, bidi, …). Any other Unicode is fine |
| `--connect-timeout` | Seconds to establish a connection | 1–3600 (built-in: 15; a non-number, a negative number or one above 4294967295 is exit 2) |
| `--ssh-binary` (defaults only) | The ssh program: a path or a command name on `PATH` | as `--key` (built-in: `ssh`) |

- `--unset <FIELD>` clears a setting; repeatable or comma-separated
  (`--unset user,port`). The field is the flag's name: `user`, `port`, `key`,
  `connect-timeout`, `jump`, `known-hosts`, `ssh-config`, `multiplex`, plus
  `name`, `notes` (`server edit`) or `ssh-binary` (`server defaults`). The
  host cannot be unset. Any other value is exit 2. Setting and unsetting the
  same field in one command is exit 1, naming the flag you typed (`--user
  and --unset user contradict each other: pass only one`; `--no-multiplex
  and --unset multiplex …`).
- Paths are stored as given; `~` is expanded when used. A relative `--key`,
  `--known-hosts`, `--ssh-config` or `--ssh-binary` is saved but warned about
  (`warn  --key 'keys/id' is a relative path, …`): it is resolved from
  whatever directory apvm later runs in. Relative means not under the home
  directory (`~/`; on Windows also `~\`) and without a root (`/…`; on Windows also `C:\…` or `\…`). An `--ssh-binary`
  without a path separator (`/`; on Windows also `\`) is a command name
  looked up on `PATH`, not a path. Only a value the command changes is warned
  about (re-running `edit --key keys/id` does not warn again).
- An empty value (`--name ""`) is an error, not "unset"; `edit` and
  `defaults` then name the remedy (`to clear it: --unset name`).

### Effective SSH settings

Field by field: the server's value → `server defaults` → the built-in value
(connect timeout 15 s, multiplex on, program `ssh`). A field nobody sets is
left to your `~/.ssh/config` (or ssh's own default) — shown as
`not set … left to ssh_config`.

### Output

```text
$ apvm server add eu-1 --host 203.0.113.10 --user ubuntu --name "EU VPS"
Added server 'eu-1'.
$ apvm server edit eu-1 --port 2222 --unset name
Updated server 'eu-1': port, name.
$ apvm server edit eu-1 --port 2222
Server 'eu-1' is unchanged.
$ apvm server defaults --user deploy --key /home/me/.ssh/id_ed25519
Updated the SSH defaults: user, key.
$ apvm server list
ID           DESTINATION               SITES  NAME
eu-1         ubuntu@203.0.113.10:2222  2
staging-box  deploy@staging            0
$ apvm server show eu-1
Server 'eu-1'
  host   203.0.113.10
  sites  shop, blog

Effective SSH settings
  SETTING          VALUE                     FROM
  user             ubuntu                    server
  port             2222                      server
  key              /home/me/.ssh/id_ed25519  defaults
  connect timeout  15 s                      built-in
  known hosts      not set                   left to ssh_config
  ssh config       not set                   left to ssh_config
  jump hosts       not set                   left to ssh_config
  multiplex        on                        built-in
  ssh program      ssh                       built-in
$ apvm server remove eu-1 --cascade
? Remove server 'eu-1' and its 2 sites: 'shop', 'blog' (they also leave 1 group: 'qa')? [y/N] y
Removed server 'eu-1' and its 2 sites: 'shop', 'blog'.
They were removed from 1 group: 'qa'.
```

`DESTINATION` is what ssh connects to: `[user@]host[:port]` with the effective
user and port (an IPv6 host is bracketed before a port). With no servers,
`list` prints `No servers yet. Add one with: apvm server add <ID> --host <HOST>`.
`server defaults` (no flags) lists every SSH default, unset ones as
`not set (left to ssh_config)` or `not set (built-in: …)`.

### Confirmation (`server remove`)

- Without `-y`/`--yes`, apvm asks once on stderr (`[y/N]`; only `y`/`yes`
  confirms). Declining prints `Aborted. Nothing changed.` (exit 0).
- **Stdin must be a terminal to ask.** In scripts, pipes and CI pass `-y`;
  otherwise it fails with exit 1 (`… stdin is not a terminal; pass -y (--yes)
  to remove it without asking`) — a piped `y` is not accepted.
- End of input (Ctrl-D) declines, like an empty answer.
- If what the question showed changes while you are answering (another
  process changes the server record — its host or settings —, its sites, or
  the groups they are in), nothing is removed: `server 'eu-1' or its sites
  changed while you were confirming; nothing was removed: run the command
  again`.
- An unknown server, a server in use without `--cascade`, or a read-only
  inventory fails before asking.

### `--json`

`list`, `show` and `defaults` accept `--json` (stdout, `"schema": 1`); records
appear as the inventory stores them (unset fields omitted).

```text
apvm server list --json      → {"schema": 1, "servers": [{"id", "host", …, "sites": ["shop", …]}]}
apvm server show <ID> --json → {"schema": 1, "server": {…, "sites": […]},
                                "effective": {"user": {"value": "ubuntu", "source": "server"} | null, …}}
apvm server defaults --json  → {"schema": 1, "defaults": {"ssh": {…}}}   (after a change: the new defaults)
```

`effective` fields: `user`, `port`, `identity_file`, `known_hosts_file`,
`ssh_config_file`, `proxy_jump` (each `{value, source}` or `null` = left to
ssh_config), and `connect_timeout_secs`, `multiplex`, `binary` (always set).
`source` is `server`, `defaults` or `built-in`.

On failure with `--json`, stdout holds
`{"schema": 1, "error": {"code", "message", "problems": [{"location", "message"}]}}`
(and stderr the usual `Error: …`), exit 1. `location` names the flag for
refused input (`--port`), or the place in the file (`servers[1].host`).

### Errors and exit codes

Exit `0` success (also a declined confirmation), `1` any refusal or error,
`2` invalid arguments (clap: missing `--host`, unknown `--unset` field, a
port above 65535, `edit` without a change, `--multiplex` with
`--no-multiplex`).

Refused input lists every problem, one per line, under the flag you typed;
values are quoted, and control or invisible characters escaped (`U+001B`):

```text
Error: invalid server 'eu-1':
  --port: must be between 1 and 65535
  --key: contains U+0020; only letters, digits, '.', '_', '/', '\', ':', '+' and '-' are allowed, …
```

| Code (`--json`) | When | Message / remedy |
|-----------------|------|------------------|
| `invalid_input` | Refused flag values, unknown or taken id, set + unset of one field, a server in use | an empty value in `edit`/`defaults` + `to clear it: --unset <field>`; `server 'x' does not exist` + `'apvm server list' shows the servers`; `server 'x' already exists` + `change it with 'apvm server edit', or choose another id`; `server 'x' is used by N site(s): …` + `pass --cascade to remove those sites with it` |
| `inventory_invalid` | The file does not parse or breaks a rule | `<path>: invalid inventory:` + one problem per line + `fix the file by hand …, or point APVM_INVENTORY at another inventory` |
| `inventory_unsupported` | Written by a newer apvm this one cannot read | Upgrade apvm |
| `inventory_read_only` | A change to a newer (readable) inventory | Upgrade apvm |
| `inventory_io` | The file, its lock or its directory cannot be read/written/created, or is a symlink, directory or special file | Fix the path or its permissions |
| `inventory_locked` | Another process held the lock for 10 s | `edits hold the lock for milliseconds, so that process is likely stuck: stop it, then retry` |
| `confirmation_required` | `server remove` without `-y` and without a terminal, or the answer could not be read on the terminal (`cannot ask for confirmation: …`) | Pass `-y` |
| `inventory_changed` | The server, its sites or their groups changed during the confirmation | Run the command again |

```sh
apvm server defaults --user deploy --key ~/.ssh/id_ed25519
apvm server add eu-1 --host 203.0.113.10 --user ubuntu --name "EU VPS"
apvm server add staging --host staging-alias          # an ~/.ssh/config alias
apvm server add behind --host 10.0.0.5 --jump bastion.example.com
apvm server edit eu-1 --port 2222 --unset name
apvm server list --json
apvm server show eu-1
apvm server remove staging -y
apvm server remove eu-1 --cascade -y                   # also removes its sites
APVM_INVENTORY=/tmp/test-inventory.json apvm server list
```

## `apvm site` — WordPress installs on servers

| Command | Arguments | Notes |
|---------|-----------|-------|
| `apvm site add <ID>` | `--server <ID> --path <WP_ROOT> [site flags] [--group <ID>]…` | The server must exist. `--group` (repeatable or comma-separated) adds the site to **existing** groups, in the same change (create them with `apvm group add`) |
| `apvm site edit <ID>` | `[--server <ID>] [--path <WP_ROOT>] [site flags] [--group <ID>]… [--unset <FIELD>]…` | At least one change (else exit 2). The id cannot change. `--server` moves the site to another server. `--group` only joins groups: it never leaves one (`apvm group edit <GROUP> --remove <SITE>` does) |
| `apvm site list` | `[--server <ID>]… [--group <ID>]… [--json]` | Inventory order. Filters (repeatable or comma-separated): a site is listed when it is on any listed server **and** in any listed group. An unknown server or group id is an error (exit 1), never an empty list |
| `apvm site show <ID>` | `[--json]` | The record, its groups, where ssh connects, and the **effective** settings with where each comes from |
| `apvm site remove <ID>` | `[-y\|--yes]` | Removes it from the inventory (and from its groups); nothing on the server changes. Asks first unless `-y` |

Site flags (the same for `add` and `edit`): `[--url <URL>] [--wp-cli "<CMD>"]
[--run-as auto|login|<USER>] [--owner auto|<USER>[:<GROUP>]]
[--ownership mirror|mirror-root|check|off] [--plugins-dir <PATH>]
[--allow-root] [--tmp-dir <PATH>] [--timeout <SECS>] [--name <TEXT>]
[--notes <TEXT>]`.

### Flags

| Flag | Meaning (built-in when unset) | Rule (checked by apvm; exit 1 otherwise) |
|------|-------------------------------|------------------------------------------|
| `<ID>` | Site id | `^[a-z0-9][a-z0-9._-]{0,63}$`, unique among sites |
| `--server` | The server the site is on | an existing server id |
| `--path` | The WordPress root on the server | absolute and canonical (no empty, `.` or `..` component, no trailing `/`, not `/`); no backslash, control or invisible character; no whitespace but spaces, no component starting or ending with a space; ≤ 4096 bytes |
| `--url` | Site URL, passed to WP-CLI as `--url` (needed on multisite) | `http(s)://host[:port][/path]`: lowercase scheme; DNS name, IPv4 or `[IPv6]`; path `[A-Za-z0-9._~/-]` and `%`-escapes, no `.`/`..` segment; no user info (never echoed back), query or fragment; ≤ 2048 bytes |
| `--wp-cli "<CMD>"` | The WP-CLI command (`wp`) | One string, split at ASCII spaces, tabs and newlines into ≤ 4 tokens of `[A-Za-z0-9._/+-]` (quotes are not syntax: they are refused). Either one program (a command name or an absolute path), or a PHP interpreter (`php`, `php8.3`, `php-8.3`, `ea-php81`, or an absolute path to one) + optional `-n`/`-q` + the absolute WP-CLI script. Wrappers (`sudo wp`, `env -u X wp`, `wp eval-file`) are refused. A value starting with `-` needs `--wp-cli=…` |
| `--run-as` | Who WP-CLI runs as: `auto` (the plugins directory's owner and group), `login` (the SSH user, no sudo) or a user (`auto`) | a user: `[A-Za-z0-9._-]`, no leading `-`, ≤ 64, not all digits, not `.`/`..`; the keywords are lowercase, and a user named `auto` or `login` in any letter case (`Auto`, `LOGIN`) is refused; `root` (any case) needs `--allow-root` |
| `--owner` | Who must own the plugin files: `auto` (the plugins directory's owner), `USER` or `USER:GROUP` (`auto`) | user and group as `--run-as`, but only `auto` is a keyword here, in any letter case: `Auto` alone is refused as the keyword (`a user cannot be named 'Auto': …`), and `auto:<group>` or `AUTO:<group>` because the keyword takes no group; `login`/`Login` is an ordinary user here; `root` (any case) as user or group needs `--allow-root` |
| `--ownership` | How ownership is checked and repaired (`mirror`) | `mirror` (repair without root), `mirror-root` (also the guarded root `chown`), `check` (report only), `off`; anything else is exit 2 |
| `--plugins-dir` | Pins a plugins directory for a site that sets `WP_PLUGIN_DIR` (`<path>/wp-content/plugins`) | as `--path` |
| `--allow-root` | Allows WP-CLI as root and root-owned files (no) | a switch; `--unset allow-root` turns it off again (refused while `--run-as`/`--owner` name root). The library's refusal names the file field (`'root' needs allow_root: true`); apvm adds the flag remedy |
| `--tmp-dir` | Remote temporary directory for uploads and the site lock (`/tmp`) | as `--path`, and only `[A-Za-z0-9._/+-]` |
| `--timeout` | Seconds a remote command may run (each operation's own default) | 1–86400 (a non-number, a negative number or one above 4294967295 is exit 2) |
| `--name` / `--notes` | Display name / free-form notes | 1–128 / 1–1024 characters; no control or invisible formatting characters |
| `--group` (`add`/`edit`) | Join these existing groups | an existing group id (`group '<id>' does not exist` otherwise; nothing written) |

- `--unset <FIELD>` clears a setting back to its built-in value; repeatable
  or comma-separated (`--unset url,timeout`). The field is the flag's name:
  `url`, `wp-cli`, `run-as`, `owner`, `ownership`, `plugins-dir`,
  `allow-root`, `tmp-dir`, `timeout`, `name`, `notes`. The server and the
  path cannot be unset (exit 2), nor can a group be left with `--unset`.
  Setting and unsetting one field is exit 1 (`--url and --unset url
  contradict each other: pass only one`).
- An empty value (`--url ""`, `--wp-cli " "`) is an error, not "unset"; `edit`
  names the remedy (`to clear it: --unset url`).
- Refused `--wp-cli` tokens are named by position, from 0:
  `--wp-cli[0]: must be a PHP interpreter (php, php8.3, php-8.3, ea-php81) when
  more tokens follow`, `--wp-cli[1]: must be an absolute path (WP-CLI runs from '/')`.
- Nothing is a local path, so nothing warns about relative paths: a relative
  `--path`, `--plugins-dir` or `--tmp-dir` is refused.

### Output

```text
$ apvm site add shop --server eu-1 --path /var/www/shop --url https://shop.example.com --group qa
Added site 'shop'.
It joined 1 group: 'qa'.
$ apvm site edit shop --run-as login --unset url
Updated site 'shop': url, run-as.
$ apvm site edit shop --group qa
Site 'shop' is unchanged.
$ apvm site edit shop --group prod --notes "main store"
Updated site 'shop': notes, groups.
It joined 1 group: 'prod'.
$ apvm site list
ID    SERVER  PATH           URL                       NAME
shop  eu-1    /var/www/shop  https://shop.example.com  Shop
blog  eu-1    /var/www/blog
$ apvm site show shop
Site 'shop'
  name    Shop
  server  eu-1 (ubuntu@203.0.113.10:2222)
  path    /var/www/shop
  url     https://shop.example.com
  groups  qa, prod

Effective settings
  SETTING      VALUE                             FROM
  wp-cli       /usr/bin/php8.3 /opt/wp-cli.phar  site
  run as       login                             site
  owner        auto                              built-in
  ownership    mirror                            built-in
  plugins dir  /var/www/shop/wp-content/plugins  built-in
  allow root   no                                built-in
  tmp dir      /tmp                              built-in
  timeout      not set                           each operation's default
$ apvm site remove shop
? Remove site 'shop' from the inventory (it also leaves 2 groups: 'qa', 'prod')? [y/N] y
Removed site 'shop'.
It was removed from 2 groups: 'qa', 'prod'.
```

Changes report the changed fields in a fixed order (`server`, `path`, `url`,
`wp-cli`, `run-as`, `owner`, `ownership`, `plugins-dir`, `allow-root`,
`tmp-dir`, `timeout`, `name`, `notes`, then `groups`). `show` prints `name`,
`url` and `notes` only when set; `server` is followed by where ssh connects
(`[user@]host[:port]`, as in `server list`). With no sites, `list` prints
`No sites yet. Add one with: apvm site add <ID> --server <SERVER> --path <WP_ROOT>`;
when filters match nothing, `No site matches the filters.` (exit 0).

### Confirmation (`site remove`)

As `server remove`: asked once on stderr unless `-y`/`--yes` (only `y`/`yes`
confirms; declining prints `Aborted. Nothing changed.`, exit 0); without a
terminal on stdin it fails (exit 1, `not removing site '<id>': it needs a
confirmation, and stdin is not a terminal; pass -y (--yes) to remove it
without asking`). If the site record or its group memberships change while
you answer, nothing is removed: `site '<id>' or its groups changed while you
were confirming; nothing was removed: run the command again`. An unknown site
or a read-only inventory fails before asking.

### `--json`

`list` and `show` accept `--json` (stdout, `"schema": 1`); records appear as
the inventory stores them (unset fields omitted; `wp_cli` as its token list,
`run_as`/`owner`/`ownership` as written).

```text
apvm site list --json      → {"schema": 1, "sites": [{"id", "server", "path", …, "groups": ["qa", …]}]}
apvm site show <ID> --json → {"schema": 1, "site": {…, "groups": […]},
                              "effective": {"id", "path", "url" | null,
                                            "server": {"id", "host", "ssh": {…as server show…}},
                                            "wp_cli": {"value": ["wp"], "source": "built-in"},
                                            "run_as", "owner", "ownership", "plugins_dir",
                                            "allow_root", "remote_tmp_dir": {value, source},
                                            "command_timeout_secs": 600 | null}}
```

`source` is `site` or `built-in` for the site's settings (`server`,
`defaults` or `built-in` for SSH). Failures print the same
`{"schema": 1, "error": {…}}` document as `apvm server`; `location` names the
flag for refused input (`--run-as`, `--wp-cli[1]`).

### Errors

Exit codes as for `apvm server` (`0` success or declined, `1` refusal, `2`
invalid arguments: missing `--server`/`--path` on `add`, unknown `--unset`
field or `--ownership` value, `edit` without a change). The codes are those of
the server table above; site-specific messages:

| Message | Code | Line apvm prints after it (or, in *italics*, what to do when it prints none) |
|---------|------|--------------------------------------------------------------------------------|
| `site 'x' does not exist` | `invalid_input` | `'apvm site list' shows the sites` |
| `site 'x' already exists` | `invalid_input` | `change it with 'apvm site edit', or choose another id` |
| `invalid site 'x':` + `--server: unknown server 'y'` | `invalid_input` | *fix the id (`apvm server list`), or add the server first (`apvm server add`)* |
| `invalid site 'x':` + `--run-as: 'root' needs allow_root: true` (or `--owner: user/group 'root' …`) | `invalid_input` | `root needs --allow-root: pass it, or name another user in --run-as/--owner`; after `--unset allow-root`: `--run-as/--owner name root, which needs --allow-root: keep it (drop --unset allow-root), or name another user there first` |
| `group 'x' does not exist` (`--group`) | `invalid_input` | `'apvm group list' shows the groups` (create it with `apvm group add`) |
| `server 'x' does not exist` (`list --server`) | `invalid_input` | `'apvm server list' shows the servers` |
| `site 'x' or its groups changed while you were confirming; nothing was removed: run the command again` | `inventory_changed` | *none: the message already says to run the command again* |

```sh
apvm site add shop --server eu-1 --path /var/www/shop --url https://shop.example.com
apvm site add legacy --server eu-1 --path /srv/legacy --wp-cli "/usr/bin/php8.3 -n /opt/wp-cli.phar" --run-as login
apvm site add ftpsite --server eu-1 --path /home/ftpuser/public_html --owner ftpuser:www-data --ownership check
apvm site edit shop --plugins-dir /var/www/shop/app/plugins --timeout 600
apvm site edit shop --unset url,timeout
apvm site list --server eu-1 --group qa --json
apvm site show shop --json
apvm site remove legacy -y
```

## `apvm group` — named lists of sites

| Command | Arguments | Notes |
|---------|-----------|-------|
| `apvm group add <ID>` | `[<SITE>…] [--notes <TEXT>]` | The sites must exist; none is fine (an empty group). The group is added last |
| `apvm group edit <ID>` | `[--add <SITE>]… [--remove <SITE>]… [--notes <TEXT>] [--unset notes]` | At least one change (else exit 2). The id cannot change. `--remove` takes a site out of the group only: the site stays in the inventory |
| `apvm group list` | `[--json]` | Inventory order, with each group's site count and members |
| `apvm group show <ID>` | `[--json]` | Its sites, in the group's order, and its notes |
| `apvm group remove <ID>` | `[-y\|--yes]` | Removes the group only: its sites stay. Asks first unless `-y` |

| Flag | Meaning | Rule (checked by apvm; exit 1 otherwise) |
|------|---------|------------------------------------------|
| `<ID>` | Group id | `^[a-z0-9][a-z0-9._-]{0,63}$`, unique among groups |
| `<SITE>…` (`add`), `--add`, `--remove` (`edit`) | Site ids; repeatable and comma-separated (`add qa shop,blog api`, `--add a,b --add c`) | each an existing site (`site '<id>' does not exist` + `'apvm site list' shows the sites` for the first unknown one: the `add` sites in order, or the `--add` ones, then the `--remove` ones; nothing written), `--remove` included. The group's own refusals (taken or invalid id, invalid `--notes`, a read-only inventory) are reported first |
| `--notes` | Free-form notes | 1–1024 characters; no control or invisible formatting characters. `--unset notes` clears them; an empty `--notes ""` in `edit` adds `to clear it: --unset notes` |

- A site named twice counts once; adding a member or removing a non-member
  changes nothing. Members keep their order; added ones go last, in the
  order typed.
- One site in both `--add` and `--remove` is exit 1 (`--add 'blog' and
  --remove 'blog' contradict each other: pass only one`), and so are
  `--notes` with `--unset notes`.
- Removing a site (`apvm site remove`, or `apvm server remove --cascade`
  for its server's sites) takes it out of every group in the same change.
  The groups stay, even when that leaves them empty.

### Output

```text
$ apvm group add qa shop blog --notes "release candidates"
Added group 'qa'.
It lists 2 sites: 'shop', 'blog'.
$ apvm group add empty
Added group 'empty'.
$ apvm group edit qa --add api --remove blog
Updated group 'qa': sites.
Added 1 site to it: 'api'.
Removed 1 site from it: 'blog'.
$ apvm group edit qa --add shop --unset notes
Updated group 'qa': notes.
$ apvm group edit qa --add shop
Group 'qa' is unchanged.
$ apvm group list
ID     SITES  MEMBERS
qa     2      shop, api
empty  0
$ apvm group show qa
Group 'qa'
  sites  shop, api
$ apvm group remove qa
? Remove group 'qa' from the inventory (the sites it lists stay: 'shop', 'api')? [y/N] y
Removed group 'qa'.
The sites it listed stay in the inventory: 'shop', 'api'.
```

Changes report `sites` (when the members changed), then `notes`. `show`
prints `notes` only when set, and `sites  none` for an empty group. With no
groups, `list` prints `No groups yet. Add one with: apvm group add <ID> [<SITE>…]`.

### Confirmation (`group remove`)

As `server remove`: asked once on stderr unless `-y`/`--yes`; without a
terminal on stdin it fails (exit 1, `not removing group '<id>': it needs a
confirmation, and stdin is not a terminal; pass -y (--yes) to remove it
without asking`). If the group's sites or notes change while you answer,
nothing is removed: `group '<id>' changed while you were confirming; nothing
was removed: run the command again` (`inventory_changed`). An unknown group
or a read-only inventory fails before asking.

### `--json`

```text
apvm group list --json      → {"schema": 1, "groups": [{"id": "qa", "sites": ["shop", "api"], "notes": "…"}, …]}
apvm group show <ID> --json → {"schema": 1, "group": {"id": "qa", "sites": ["shop", "api"]}}
```

Records appear as the inventory stores them (`notes` omitted when unset).
Failures print the same `{"schema": 1, "error": {…}}` document as
`apvm server`.

### Errors

Exit codes as for `apvm server` (`0` success or declined, `1` refusal, `2`
invalid arguments: `add` without an id, `edit` without a change, an `--unset`
field other than `notes`).

| Message | Code | Line apvm prints after it |
|---------|------|---------------------------|
| `group 'x' does not exist` | `invalid_input` | `'apvm group list' shows the groups` |
| `group 'x' already exists` | `invalid_input` | `change it with 'apvm group edit', or choose another id` |
| `site 'x' does not exist` | `invalid_input` | `'apvm site list' shows the sites` |
| `invalid group 'X':` + `<ID>: …` / `--notes: …` | `invalid_input` | `to clear it: --unset notes` after an empty `--notes` in `edit` |
| `group 'x' changed while you were confirming; nothing was removed: run the command again` | `inventory_changed` | — |

```sh
apvm group add qa shop blog --notes "release candidates"
apvm group add later                          # empty for now
apvm group edit qa --add api,ftpsite --remove blog
apvm group edit qa --unset notes
apvm group list --json
apvm group show qa
apvm group remove qa -y
apvm site list --group qa                     # the sites of a group, with their servers
```
