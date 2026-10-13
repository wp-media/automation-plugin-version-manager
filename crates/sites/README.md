# apvm-sites

Manage WordPress plugins on remote sites over SSH. This crate is the library
behind `apvm server|site|group` and `apvm wp`, the Node bindings, and
apvm-core's build-and-install path. The design and the phase plan are in
[`docs/plans/site-deployment.md`](../../docs/plans/site-deployment.md).

**Status:** phase P1 (foundations): the data layer (P1.1), inventory
persistence (P1.2), settings and targets (P1.3), the plugin hint types that
apvm-core fills (P1.4) and local package inspection (P1.5); phase P3.1
adds the remote script builder. It has the inventory model and its
validation, safe reading and writing of the inventory file, settings
resolution and target selection, plugin hints, plugin zip inspection, the
shared file names, and the scripts apvm will run on servers. Running them
over SSH, WP-CLI parsing and the plugin operations arrive in later phases.

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
  (`inventory.json.lock`, `std::fs::File::try_lock`). A busy lock is
  retried for up to 10 s (`io::LOCK_WAIT`), then the write fails with
  `inventory_locked`: edits hold the lock for milliseconds, so only a stuck
  process makes a writer wait that long. The document is written
  to a temp file created `0600`, synced, renamed over the inventory, and the
  directory is synced, so a crash or a failed write never leaves a partial
  file. Missing directories are created `0700`.
- **`update`**: load, change and save under the lock, so concurrent edits
  from threads or processes all land. The file is not written when the
  change fails or changes nothing (the lock file and missing directories
  are still created). A change to a newer, read-only inventory is refused,
  even one that replaces it whole.
- **Refused targets create nothing:** every write (`save`, `update`, the
  tracked forms) refuses a path that names no file, or a symbolic link, a
  directory or a special file at the path, before taking the lock, so the
  refusal leaves neither a lock file nor a directory behind.
  `io::check_path` is that path check alone, for front ends that change a
  path (making it absolute drops a trailing `/.`) before passing it on.
- **`load_tracked` + `save_if_unchanged`**: for callers that keep an
  inventory in memory between loading and saving it (the Node bindings).
  `load_tracked` also returns a `Fingerprint` (the SHA-256 of the exact
  bytes read, or "missing"). `save_if_unchanged` re-reads the file under the
  lock and writes only when it still matches; otherwise it fails with
  `inventory_changed` and writes nothing, so an edit another process made in
  between is never lost (load again, reapply, save). It returns the new
  fingerprint, so saves chain. A byte-identical rewrite is not a conflict; a
  file deleted, created or grown past 16 MiB since the load is. The file is
  compared once before taking the lock, so a refusal usually creates no lock
  file or directory. `Fingerprint::missing()` expects no file, so an empty
  inventory never wipes an existing one.
- **`save`** overwrites blindly (use it only to force: a damaged file is
  replaced, a backup restored) and returns the fingerprint of what it
  wrote. It never replaces an inventory with a newer format version
  (checked under the lock), with the codes `update` gives for that file:
  `inventory_read_only` when this build reads it, `inventory_unsupported`
  when not. Damaged files (not an inventory, invalid, empty, over 16 MiB)
  and older or same-version ones are replaced; to restore a backup over a
  newer file, move it aside first. The version is the JSON `version` read
  as a whole number up to `u32::MAX`, the last one when the key is
  repeated; any other value (`"2"`, `2.0`, `-2`, larger) makes the file
  damaged. A file that cannot be read (no read permission) is refused
  (`inventory_io`, context `io::VERSION_CHECK_CONTEXT`): it might be a
  newer inventory. **`update_tracked`**
  is `update` that also returns the resulting inventory and its
  fingerprint; **`update_tracked_with`** also hands the change the
  fingerprint of the bytes it loaded, for a caller that lets the loaded
  copy escape (the Node bindings' `update(fn)`). They let a caller keep
  tracking the file without loading it again, which could pair its
  inventory with another process's later edit.
- File names ending with `.` or a space are refused on every platform:
  Windows drops them when opening a file but not from the lock file's
  name (`inventory.json.` opens `inventory.json` but locks
  `inventory.json..lock`), so two writers could both get in.
- Documents over 16 MiB are never written (`load` would refuse them), and
  paths that name no file (empty, `/`, `..`, or ending with a separator or
  `/.`) are refused.
- **Symbolic links are refused** for the inventory file and its lock file
  (even dangling ones), so apvm never reads, locks or replaces a file
  elsewhere. Directories above the file may be links.
- **Never delete `inventory.json.lock`** while apvm may run: a writer
  holding the lock keeps it on the deleted file, so the next writer locks a
  new file and both get in.

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

## Remote scripts (`remote`)

Every script apvm sends to a server is built from typed parts, so an unsafe
one cannot be built (plan §6.1). sshd runs commands through the login shell
(bash, dash, zsh, fish, tcsh, …), so data never reaches that shell:

| Type | What it is |
|---|---|
| `Script` | stdin delivery: the remote command is `sh -s`, the script travels on stdin, and values are POSIX-quoted (`quote`), so they may hold any character but NUL |
| `UploadScript` | upload delivery (stdin carries a file): `sh -c '<script>'`, parsed by the login shell, so its values must match `^[A-Za-z0-9._/+-]+$` and its one-line template has neither `\\` nor `\'` |
| `Identity` | who runs a command: the login user, `sudo -n -u U [-g G] --` or `runuser -u U -g G --`; names follow the user rule (`AccountName`) |
| `RootCommand` + `RootAccess` | the closed set of root commands, through `sudo -n -u root --` or a root login: `stat` of a directory (uid, gid, mode, file type, then the names, one per line) or of a plugin entry (its file type), and the guarded `chown -R -P -h --from=+F:+FG -- +U:+G` of `mirror-root` sites (numeric ids: GNU `chown` would read a bare `1001` as a user name first) |
| `WpCli` | `env -u EDITOR -u PHPRC … -u WP_CONFIG_PATH WP_CLI_CONFIG_PATH=/dev/null PAGER=cat <wp_cli> --skip-packages --exec='umask(umask() & ~0<g>);' --path=… [--url=…] <args>`, with `<g>` from the plugins directory's `FileMode`. `env -u` removes the 18 variables that make WP-CLI 2.12 or PHP load code, run another program or behave differently (`WP_CLI_REQUIRE`, `EDITOR`, `WP_CLI_CACHE_DIR`, `WP_CLI_EARLY_REQUIRE`, `PHPRC`, `PHP_INI_SCAN_DIR`, `WP_CLI_ALLOW_ROOT`, …). Arguments may hold any character but NUL, but cannot set a WP-CLI global parameter (WP-CLI reads them anywhere, even after `--`) other than plan §6.12's overrides: `--url=…` (a valid site URL), `--user=…`, `--skip-plugins[=…]`, `--skip-themes[=…]`, `--debug[=…]`, `--quiet` |
| `Nonce` | the per-call random marker; only stdout after its line is the payload |

Every script is one brace group, `{ LC_ALL=C; export LC_ALL … }`: the shell
reads it whole before running any of it, so a script cut short runs
nothing. It prints the nonce, runs its command from `/` with stdin from
`/dev/null`, and exits with the command's status, 255 mapped to 254 (a 255
always means ssh failed). No shell ever runs under `sudo` or `runuser`.
Refused values are `invalid_input` errors naming the rule.

## Errors

`Error::code()` returns a stable `Code`:

- `inventory_invalid`: parse or validation failure;
- `inventory_unsupported`: a newer document this build cannot read;
- `inventory_read_only`: a change to a newer document;
- `inventory_io`: the inventory file, its lock file or its directory cannot
  be read, written, locked or created, or is not a regular file (a symbolic
  link, a directory);
- `inventory_locked`: another process held the inventory's lock for the
  whole wait (10 s); nothing was written;
- `inventory_changed`: the file changed since it was loaded, so a guarded
  save (`save_if_unchanged`) wrote nothing;
- `invalid_input`: a refused record, a taken or unknown id, a server in use,
  invalid hints, or a value refused while building a remote script (an
  account name, a path, a WP-CLI token or argument, a nonce, a file mode,
  a root command's owners);
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
  is opened), a newer inventory never overwritten (by a forced save
  either), refused targets leaving no lock file, the bounded lock wait
  (a busy lock times out, a released one is taken), the size cap on reads
  and writes, and tracked saves (a file changed, deleted or created since
  the load is refused and left untouched, fingerprints chain from tracked
  and forced saves and from `update_tracked`, a byte-identical rewrite or a
  no-op update is no conflict, a refusal creates no lock file, threads that
  reload on conflict all land);
- settings precedence (every field at every level) and target selection
  (union, order, de-duplication, errors, warnings), including an exhaustive
  check of every selection on a sample inventory;
- package inspection, on zips the tests generate: every structure, path,
  directory and limit rule (including zips that libzip, PclZip and the zip
  crate would read differently, and ZIP64 end records), damaged and
  encrypted entries, other compression methods, WordPress header parsing
  (8 KiB window, CRLF, a multibyte character cut at the boundary) and
  variant markers, plus a small fuzz test (mutated zips never panic);
- remote scripts: goldens per delivery mode, identity, root command and
  plugins directory mode; the upload allowlist and template rules; the
  WP-CLI global parameters an argument may not set; and, on Unix, real
  shells (whichever are installed): thousands of hostile and generated
  values quoted through `sh`, `bash`, `dash`, `zsh`, `ksh`, `mksh`, `yash`
  and busybox (`-s` and `-c`) arrive byte for byte (yash gets ASCII
  values only: it cannot read other bytes under the C locale); framed
  WP-CLI scripts deliver their argv, `LC_ALL=C`, `WP_CLI_CONFIG_PATH`,
  `PAGER=cat`, the removed variables and `/` as working directory, never
  run text after the script, and map 255 to 254; a script cut at any byte
  runs nothing; uploads stream 1 MiB byte for byte through every installed
  login shell (fish, tcsh and csh too, where installed); GNU `stat` and
  `chown` accept the root commands (Linux). Every child process is killed
  after 30 s.

`tests/inventory.rs` loads the example document of the plan through the
public API; `tests/targets.rs` resolves targets and settings on it.
`tests/inventory_io.rs` runs concurrent updates from several processes,
reproduces a tracked save's conflict with an edit from another process, and
checks `APVM_INVENTORY`. `tests/package.rs` inspects a BackWPup Pro zip
through the public API.
