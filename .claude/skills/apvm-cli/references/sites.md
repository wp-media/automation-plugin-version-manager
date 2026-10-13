# apvm site inventory — servers, sites, groups

apvm keeps the WordPress sites it can deploy to in a **site inventory**: SSH
**servers**, the WordPress **sites** on them, and **groups** of sites, plus
default SSH settings every server inherits. This release ships the server
commands (`apvm server …`); the `site`, `group` and `wp` commands come later.

No inventory command connects to your servers: it reads or edits the file
only, and no SSH connection is made (the connection test, `apvm server
test`, comes with a later release). Like every apvm command run on a
terminal, it may check GitHub for a newer apvm in the background
(`APVM_NO_UPDATE_CHECK=1` turns that off).

## The inventory file

| Topic | Behavior |
|-------|----------|
| Location | `~/.apvm/inventory.json`. **`APVM_INVENTORY=<path>`** replaces it for one invocation (tests, CI, a second set of servers); an empty value means the default; a relative path is relative to the current directory |
| Missing file | An empty inventory. Reads (`list`, `show`, `defaults` without flags) never create it; the first change does. A change refused after taking the lock (invalid value, unknown id) writes no inventory, but leaves the directory it created (`0700`) and the empty lock file (`inventory.json.lock`, `0600`); contradicting flags (`--user u --unset user`) touch nothing |
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
