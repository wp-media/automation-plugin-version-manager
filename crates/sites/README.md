# apvm-sites

Manage WordPress plugins on remote sites over SSH. This crate is the library
behind `apvm server|site|group` and `apvm wp`, the Node bindings, and
apvm-core's build-and-install path. The design and the phase plan are in
[`docs/plans/site-deployment.md`](../../docs/plans/site-deployment.md).

**Status:** phase P1 (foundations): the data layer (P1.1), inventory
persistence (P1.2), settings and targets (P1.3), the plugin hint types that
apvm-core fills (P1.4) and local package inspection (P1.5). It has the
inventory model and its validation, safe reading and writing of the
inventory file, settings resolution and target selection, plugin hints,
plugin zip inspection and the shared file names. SSH, WP-CLI and the plugin
operations arrive in later phases.

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

## The inventory file (`inventory::io`)

The caller passes the path: front ends use `~/.apvm/inventory.json`, or
`APVM_INVENTORY` when it is set and not empty (`io::env_override()`).

- **`load`**: a missing file is an empty inventory. Errors carry the path,
  and content problems their locations. Files over 16 MiB, and content that
  is not UTF-8, are refused.
- **`save`**: replaces the file under an exclusive lock
  (`inventory.json.lock`, `std::fs::File::lock`). The document is written
  to a temp file created `0600`, synced, renamed over the inventory, and the
  directory is synced, so a crash or a failed write never leaves a partial
  file. Missing directories are created `0700`.
- **`update`**: load, change and save under the lock, so concurrent edits
  from threads or processes all land. The file is not written when the
  change fails or changes nothing (the lock file and missing directories
  are still created). A change to a newer, read-only inventory is refused,
  even one that replaces it whole.
- Documents over 16 MiB are never written (`load` would refuse them), and
  paths that name no file (empty, `/`, `..`, or ending with a separator or
  `/.`) are refused.
- **Symbolic links are refused** for the inventory file and its lock file
  (even dangling ones), so apvm never reads, locks or replaces a file
  elsewhere. Directories above the file may be links.

## Settings and targets

Resolution is pure: it reads an `Inventory`, never the disk or the network.

- **Effective settings.** `Inventory::resolve_site` returns a `ResolvedSite`;
  `resolve_server` returns a server with its effective SSH settings. Each
  value says where it came from (`built-in`, `defaults`, `server`, `site`).
  - SSH, field by field: the server's `ssh`, then `defaults.ssh`, then the
    built-in value (connect timeout 15 s, multiplexing on, program `ssh`).
    A field none of them sets is left to ssh and your ssh_config. `binary`
    comes from `defaults.ssh` only.
  - The site's own settings, else the built-in ones: `wp_cli` `["wp"]`,
    `run_as` and `owner` `auto`, `ownership` `mirror`, `plugins_dir`
    `<path>/wp-content/plugins`, `allow_root` `false`, `remote_tmp_dir`
    `/tmp`.
- **Targets.** `Targets` names sites, groups, servers, or all sites.
  `Inventory::resolve_targets` returns their union in inventory order, each
  site once.
  - Errors (`invalid_input`, located like `groups[1]`): an unknown id, with
    a hint when only letter case or spaces differ; a selection that names
    nothing or matches no site. An empty selection never means "everything".
  - Warnings: a selected group or server without sites, and sites that are
    the same WordPress install: the same path on the same ssh destination
    (host, port, login user, jump hosts and ssh_config file, compared
    exactly as apvm passes them to ssh, since ssh matches `Host` patterns
    case-sensitively and resolves anything left unset through ssh_config).

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

## Package inspection

`PluginPackage::inspect(path, &hints)` reads a plugin zip in memory (never
extracting it) and returns its slug, main file, headers, variant, SHA-256,
sizes and warnings, or refuses it with `package_invalid` and the rule it
breaks (`PackageDefect`):

| Rule | Detail |
|---|---|
| File | a regular `*.zip` file of at most 256 MiB |
| Limits | at most 100 000 entries and 1 GiB uncompressed; every entry is decompressed once, so its declared size and CRC-32 are checked on the bytes actually read; reading the zip may take at most 4 times its size plus 1 MiB, which refuses entries that share their data and files full of fake directory records (zip-bomb shapes that make a reader go over the same bytes again and again) |
| Directory | read the same way by every zip reader WordPress may use (libzip, then PclZip when libzip refuses a zip): one end record, closing the file; a central directory that ends at it and holds exactly the records it declares; no two entries with the same name; no Unicode path field that renames an entry; no file with the DOS folder attribute. The entry count is checked from the end record, before any entry is parsed |
| Entries | Stored or Deflated, not encrypted |
| Paths | relative and canonical: no `..` (nor `../` inside a name), `.` or empty component, leading `/`, NUL or backslash. Symbolic links only give a warning (WordPress writes them as plain files) |
| Structure | exactly one top-level folder, named like a slug; `__MACOSX/` is ignored, as WordPress ignores it. Any top-level file, `.DS_Store` included, is refused: WordPress would then install into a folder named after the uploaded zip |
| Plugin | a `<slug>/*.php` (not starting with `.`) with `Plugin Name` in its first 8 KiB; the main file is `<slug>/<slug>.php` when it has one, else the first in byte order; several give a warning |
| Headers | read like WordPress 7.1.3's `get_file_data()`: first matching line, any letter case, CR and CRLF line endings, values cut at `*/` or `?>`; `Network`, `Requires Plugins` and the others as `get_plugin_data()` reads them |
| Variant | the hints' markers are looked for inside the zip: exactly one match names the variant; none or several leave it undetermined |

Inspection hashes and decompresses: run it on a blocking thread in async
code.

## Errors

`Error::code()` returns a stable `Code`:

- `inventory_invalid`: parse or validation failure;
- `inventory_unsupported`: a newer document this build cannot read;
- `inventory_read_only`: a change to a newer document;
- `inventory_io`: the inventory file, its lock file or its directory cannot
  be read, written, locked or created, or is not a regular file (a symbolic
  link, a directory);
- `invalid_input`: a refused record, a taken or unknown id, a server in use,
  or invalid hints;
- `package_invalid`: a plugin zip that breaks an inspection rule.

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
- the hint rules;
- the inventory file: missing = empty, located errors, `0600`, no partial
  file after a failed write or a panicking change, concurrent updates from
  threads, refused symbolic links (including one swapped in while the file
  is opened), a newer inventory never overwritten, the size cap on reads
  and writes;
- settings precedence (every field at every level) and target selection
  (union, order, de-duplication, errors, warnings), including an exhaustive
  check of every selection on a sample inventory;
- package inspection, on zips the tests generate: every structure, path,
  directory and limit rule (including zips that libzip, PclZip and the zip
  crate would read differently, and ZIP64 end records), damaged and
  encrypted entries, other compression methods, WordPress header parsing
  (8 KiB window, CRLF, a multibyte character cut at the boundary) and
  variant markers, plus a small fuzz test (mutated zips never panic).

`tests/inventory.rs` loads the example document of the plan through the
public API; `tests/targets.rs` resolves targets and settings on it.
`tests/inventory_io.rs` runs concurrent updates from several processes and
checks `APVM_INVENTORY`. `tests/package.rs` inspects a BackWPup Pro zip
through the public API.
