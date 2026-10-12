# apvm-sites

Manage WordPress plugins on remote sites over SSH. This crate is the library
behind `apvm server|site|group` and `apvm wp`, the Node bindings, and
apvm-core's build-and-install path. The design and the phase plan are in
[`docs/plans/site-deployment.md`](../../docs/plans/site-deployment.md).

**Status:** the data layer (phase P1.1, audited). It has the inventory
model, its validation, the plugin hint types and the shared file names.
Persistence, SSH, WP-CLI and the plugin operations arrive in later phases.

## Design rules

- **No `apvm-*` dependencies.** apvm-core depends on this crate, never the
  reverse, and zip installs or `wp run` need no GitHub or build stack. A unit
  test checks `Cargo.toml`.
- **No default locations.** Callers pass every path. `layout` only shares
  the file names, so the CLI and the Node bindings use the same files.
- **Allowlists live here.** Every value that may reach the `ssh` argv, a
  shell command ssh builds, or a remote command is checked by the library,
  so every front end gets the same protection, Node callers included.
- **Safe messages.** Values in messages are quoted with `'` and `\` escaped,
  and control and invisible formatting characters as `\u{…}`. So a value can
  neither drive the terminal nor pass for message text.
- **Always valid.** An `Inventory` is built empty, parsed from a document
  that passes every rule, or changed by methods that check the record first
  and change nothing when it is refused. A randomized property test checks
  this. `PluginHints` are validated when they are built.

## Inventory (`inventory.json`)

```json
{
  "version": 1,
  "defaults": { "ssh": { "user": "deploy", "identity_file": "~/.ssh/id_ed25519" } },
  "servers": [ { "id": "eu-1", "host": "203.0.113.10" } ],
  "sites": [ { "id": "shop", "server": "eu-1", "path": "/var/www/shop/public" } ],
  "groups": [ { "id": "qa", "sites": ["shop"] } ]
}
```

- **Parsing is strict.**
  - `version` is required.
  - Syntax errors, wrong types and unknown fields are refused, with their
    line and column.
  - A record written as a JSON array (which serde would read by position) is
    refused with its location.
  - `null` means "not set". A leading UTF-8 byte-order mark is ignored.
- **Validation reports every problem** with its location
  (`sites[1].server: unknown server 'eu-2'`).
- **Newer versions.** A document whose `version` is newer than this build
  loads **read-only** when this build can read it fully: every field and
  value known, and every rule passed. Otherwise it is refused
  (`inventory_unsupported`), with its problems still listed, so a setting
  this build does not understand is never silently dropped.
- **Changes keep it consistent.** Ids are unique per kind. A site must name
  an existing server, and a group must list existing sites, each once.
  Removing a site takes it out of its groups. A server with sites is only
  removed when the removal cascades to them.

The rules, in short (plan §7 has the full table and the reasons):

| Value | Rule |
|---|---|
| ids | `^[a-z0-9][a-z0-9._-]{0,63}$`, unique per kind |
| `host` | `[A-Za-z0-9._:-]`, no leading `-`, at most 253 |
| user and group names (`ssh.user`, `run_as`, `owner`) | `[A-Za-z0-9._-]`, no leading `-`, at most 64, not all digits, not `.`/`..`; `root` (any case) needs `allow_root: true`; `auto` and `login` never name a user |
| `port`; `connect_timeout_secs` / `command_timeout_secs` | 1–65535; 1–3600 / 1–86400 |
| `proxy_jump` | at most 8 `[user@]host[:port]` hops and 1024 bytes, not `none`; a bracketed IPv6 address only in the last hop |
| `path`, `plugins_dir` | absolute and canonical, not `/`; no backslash, control or invisible formatting character; no whitespace but inner spaces; at most 4096 bytes |
| `remote_tmp_dir` | as `path`, of `[A-Za-z0-9._/+-]` only |
| `url` | `http(s)://host[:port][/path]`: DNS name, IPv4 or `[IPv6]`; no user info, query, fragment or dot segment; at most 2048 bytes |
| `wp_cli` | one program (command name or absolute path), or a PHP interpreter, its `-n`/`-q` options and an absolute WP-CLI script |
| `identity_file`, `known_hosts_file`, `ssh_config_file`, `binary` | `[A-Za-z0-9._/\:+-]`, `~` only as a leading `~/`, no `.` or `..` component; `known_hosts_file` not `none`, `NUL` or `/dev/…`; `binary` only in `defaults.ssh` |
| `name` / `notes` | 1–128 / 1–1024 characters, no control or invisible formatting character |

## Plugin hints

`PluginHints` describe each apvm project's plugins:
- the slug each variant installs to;
- the marker that tells same-slug variants apart: a file and a fixed text,
  such as BackWPup Pro's `inc/Pro/License/License.php` with
  `https://backwpup.de/`;
- the family traits of projects with several slugs (`conflicts_when_active`,
  `shared_data`).

apvm-core builds them from its builders, so this crate holds no
plugin-specific code. A project has variants on every slug or on none.
Same-slug variants need markers that cannot overlap.

## Errors

`Error::code()` returns a stable `Code`:

- `inventory_invalid`: parse or validation failure;
- `inventory_unsupported`: a newer document this build cannot read;
- `inventory_read_only`: a change to a newer document;
- `invalid_input`: a refused record, a taken or unknown id, a server in use,
  or invalid hints.

`Error::problems()` lists the located problems, so a front end can print
them one per line.

## Tests

```sh
cargo test -p apvm-sites
```

**Unit tests cover:**
- every rule: accept and reject tables, with the expected reasons, at the
  boundaries;
- located validation;
- the serialization of records;
- inventory changes, including a randomized property test that every
  accepted change keeps the inventory valid, every refused change leaves it
  untouched, and every result reloads unchanged;
- versions;
- the hint rules.

`tests/inventory.rs` loads the example document of the plan through the
public API.
