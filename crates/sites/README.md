# apvm-sites

Manage WordPress plugins on remote sites over SSH. This crate is the library
behind `apvm server|site|group` and `apvm wp`, the Node bindings, and
apvm-core's build-and-install path. The design and the phase plan are in
[`docs/plans/site-deployment.md`](../../docs/plans/site-deployment.md).

**Status:** phase P1 (foundations): the data layer (P1.1), inventory
persistence (P1.2), settings and targets (P1.3), the plugin hint types that
apvm-core fills (P1.4) and local package inspection (P1.5); phase P3.1
adds the remote script builder, P3.2 the transport contract that runs
scripts (with a local-shell transport), P3.3 the SSH transport and the
classification of ssh's failures, and P4.1 the WP-CLI commands and
parsers. It has the inventory model and its
validation, safe reading and writing of the inventory file, settings
resolution and target selection, plugin hints, plugin zip inspection, the
shared file names, the scripts apvm runs on servers, and the transport that
runs them over SSH. Multiplexing and the plugin operations arrive in later
phases.

## Design rules

- **No `apvm-*` dependencies but `apvm-process`.** apvm-core depends on
  this crate, never the reverse, and zip installs or `wp run` need no GitHub
  or build stack. `apvm-process` (child processes that never outlive their
  work, and the `ProcessScope` the Node bindings kill at exit) is a leaf
  crate with no apvm dependencies, shared with apvm-core. A unit test checks
  `Cargo.toml`.
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

## Running scripts (`remote::Transport`)

A `Transport` runs one `Request` (a `Script` on stdin, or an `UploadScript`
with its `Payload`, bytes or a file streamed as it is read, plus a timeout)
and returns an `Execution`:

| Part | What it holds |
|---|---|
| `exit()` | `Exit::Status(n)` (never 255; 254 = a command exited 255), `Exit::TransportFailed` (255: ssh itself failed), `Exit::Killed { signal }` (on Windows a killed process exits with status 1, so it reads as `Status(1)`), or `Exit::TimedOut` |
| `stdout()`, `stderr()` | at most 8 MiB and 1 MiB (`STDOUT_LIMIT`, `STDERR_LIMIT`), with a truncation flag; the rest is read and dropped, so the child never blocks |
| `payload()` | stdout after the first line exactly equal to the nonce (scripts print `\n<nonce>\n`, so login noise without a final newline cannot glue onto it). Errors, never "no output" or a shorter one: `PayloadError::NonceMissing { stdout_truncated }` (a login shell's startup files ate the script, the remote `sh` or ssh failed, or noise filled the 8 MiB; after a mutation the command may still have run: re-read) and `PayloadError::Truncated` (the payload went past 8 MiB; `stdout()` keeps its beginning), and `PayloadError::Interrupted { exit }` (the call did not run to its end: a transport failure, a kill or a timeout; whatever was printed is only a beginning, kept in `stdout()`) |
| `stdin_written()` | whether all of stdin was handed to the transport and closed (`false` when the remote side stopped reading: an upload is then incomplete). Not proof of receipt: bytes may sit unread in a buffer, so verify what matters (staging checks the upload's SHA-256) |
| `elapsed()` | from the start to the exit or the kill |

A call never outlives its future: dropped, or past its timeout, it kills
the local process **and every process it started**, through apvm-process
(as apvm-core does: by process ancestry, so a terminal's Ctrl+C still
reaches them; `/proc` on Linux, `/bin/ps` elsewhere; the child only on
Windows). Every call is recorded in the caller's `ProcessScope`, so the Node
bindings' `kill_all` at `process.exit()` kills a running `ssh` too. After the exit,
output gets 2 s more, so a background job left holding a pipe cannot hold
the call. An upload file that is missing, not permitted, or not a regular
file is `invalid_input` and runs nothing (its type is checked before and
after opening, and on Unix it is opened with `O_NONBLOCK`, cleared once it
is known to be a regular file, so a FIFO cannot block); a program that
cannot start, any other failure to open the file, or a file that fails
mid-read, is `process_failed`. Nothing is retried. The local shell's child
inherits the environment (for ssh's, see below).

`Execution::new` and `Captured::new` are public, so a transport (or a fake
in another crate's tests) can be written outside this crate;
`Execution::new` turns `Exit::Status(255)` into `Exit::TransportFailed`.

The crate's tests run requests through a local shell, the way sshd does:
`<login shell> -c 'sh -s'` with the script on stdin, or `<login shell> -c
"sh -c '<script>'"` with the upload on stdin. The SSH transport follows the
same pattern. apvm itself runs nothing through a local shell.

## Over SSH (`remote::SshTransport`)

`SshTransport::new(server, SshOptions)` runs each request as one process of
the system OpenSSH client (plan D2), in a private directory it creates for
that call, next to the file it writes there:

```text
<binary> -F apvm-ssh.conf [-l <user>] [-p <port>] [-J <hops>] -- <host> <remote command>
```

- **Every option is in the generated file**, because `-F` is the only part
  of a call ssh hands to jump hosts (it runs `ssh -l -p -F -v -W … <hop>`
  for them; verified on OpenSSH 9.6p1, 10.0p2 and 10.3p1: with `-o` options
  alone a jump host prompted on a terminal and used `~/.ssh/known_hosts`).
  Its options come first, so they win over the user's ssh_config for the
  server and every jump host: `BatchMode yes`; `StrictHostKeyChecking yes`
  (`accept-new` with `HostKeyPolicy::AcceptNew`, never `no`);
  `UserKnownHostsFile` = the inventory's `known_hosts_file`, else
  `~/.ssh/known_hosts ~/.ssh/known_hosts2` (an ssh_config can still add
  trusted keys with `GlobalKnownHostsFile`, `KnownHostsCommand` or
  `VerifyHostKeyDNS`); the inventory's key, first, with `IdentitiesOnly
  yes` (`IdentityFile` adds up: an ssh_config's keys are offered after
  it); `ConnectTimeout`; `ServerAliveInterval 15`,
  `ServerAliveCountMax 4`; `LogLevel INFO`; no pseudo-terminal, no remote
  command from ssh_config, no reused connection (`ControlMaster no`,
  `ControlPath none`; P3.4 adds apvm's own), no forwarding, no local
  command, the key kept out of the user's agent (`AddKeysToAgent no`: from
  an ssh_config, `yes` added it there and `ask` would open ssh-askpass,
  verified); when the client knows them, `SessionType default`, `StdinNull
  no`, `ForkAfterAuthentication no` (OpenSSH 8.7+) and `ChannelTimeout
  none` (9.6+, the client's): from an ssh_config they hang the call, drop the script,
  detach ssh from apvm, or kill a silent command (verified). Then it
  `Include`s what ssh would read without `-F`: the inventory's
  `ssh_config_file` alone, or `~/.ssh/config` and the system's
  `ssh_config` (one this user cannot read is left out: ssh skips it,
  while an `Include` of it fails, verified; when it does not exist, its
  vendor copy `/usr/etc/ssh/ssh_config`, if that exists, as openSUSE's
  ssh reads it, verified on Tumbleweed; no other ssh binary checked holds a
  vendor path). When the known_hosts files are ssh's own pair, the file
  ends with ssh's own default `UpdateHostKeys yes` (8.5+), which naming
  them would turn off: ssh then records the other host keys a server
  proves with the known one, as plain ssh does (an included file's value
  wins).
- **ssh's defaults** are read once per transport with `ssh -T -F none -G`
  (`-T` keeps ssh's pseudo-terminal notice off stderr), in a private
  directory like the calls (so a bare `ssh` is the same program): the
  compiled-in `globalknownhostsfile` sits next to the system `ssh_config`
  (checked on Debian, Ubuntu, Fedora, AlmaLinux 8 and 9, Alpine, Arch,
  openSUSE, nixpkgs and macOS builds), the keywords it prints tell which
  of those session options the client knows (an older one refuses unknown
  options; checked on 7.6p1, 8.2p1, 8.7p1, 8.9p1, 9.2p1, 9.6p1, 10.0p2,
  10.3p1, 10.6p1), and `updatehostkeys true` its `UpdateHostKeys` default.
- **Only what belongs to the server** is on the command line (`-l`, `-p`,
  `-J`: ssh keeps them from jump hosts).
- **`~` is the home apvm was given** (`SshOptions::with_home`), else `HOME`;
  relative paths are taken from the current directory. apvm writes absolute
  paths, so ssh opens what apvm checked (ssh would take `~` from the
  password database and ignore `HOME`, verified). Paths are double-quoted;
  `%` is doubled in the key path; a `"`, a control character, a `\`
  (Unix), `${`, a `%` in a known_hosts path (ssh before 8.4 reads `%%`
  literally there, verified on 7.6p1 and 8.2p1) or an `Include` pattern is
  refused (`invalid_input`).
- **Checked before ssh runs:** the key and the inventory's ssh_config file
  must be regular files (an `Include` of a missing file is silent), the
  ssh_config file not writable by group or others nor another user's
  (OpenSSH refuses to include it; Debian's and Ubuntu's ssh also accept
  one that only the owner's private group may write, which apvm still
  refuses); without one, the system's `ssh_config` must pass
  the same check (ssh reads it unchecked on its own, but checks it once
  included: one owned by another user, such as a Homebrew install of
  another account, or one its group may write, is refused with what to
  do; verified: another user's on 7.6p1, 8.2p1 and 10.0p2, a group-writable
  one on 10.0p2); the known_hosts file new keys go to must be a regular file or not
  exist yet (a link to `/dev/null` is refused: ssh "permanently adds" every
  key there and runs the command, verified), and with `accept-new` it, or
  its directory, must be writable. The ssh program's path must be
  shell-literal: ssh pastes it unquoted into a jump host's shell command.
- **The generated file** is `0600`, created exclusively in a new `0700`
  directory with a random name, and deleted with it when the call ends,
  however it ends. ssh runs in that directory and gets the file's path
  when it is shell-literal (a jump host's command goes through the user's
  shell, which pastes it unquoted, after a startup file such as `.zshenv`
  that may change directory), else its bare name. The directory goes in
  the system's temporary directory when its path is shell-literal, else on
  Unix in `/tmp` (when root's and sticky), so only a configured
  directory (`SshOptions::with_config_dir`) or a Windows temporary
  directory with a space gets the bare name. The directory is private because
  ssh, its jump hosts' `exec ssh …`, and an ssh_config's `ProxyCommand`
  and `Match exec` run in it: a relative `PATH` entry (`PATH=:/usr/bin`)
  finds programs there, and in a shared `/tmp` another user could plant
  one (verified). `SshTransport::config_text()` shows the file, for logs.
- **ssh's environment** is apvm's minus `GITHUB_TOKEN` and `GH_TOKEN`, plus
  `LC_ALL=C` (English system messages for the classifier) and
  `SSH_ASKPASS_REQUIRE=never` (no ssh-askpass dialog from an ssh that a
  ProxyCommand of the user's starts, OpenSSH 8.4+; such an ssh can still
  ask on apvm's terminal, which ssh shares so that Ctrl+C reaches it).
- **A missing ssh** is `ssh_missing` (a program that cannot report its
  defaults with `-G` too). An exit 255 is a result, not an error.

`remote::classify(&execution)` tells why ssh failed, only when it exited
255 (a remote command's stderr is never read this way; scripts map their
own 255 to 254), from phrases in its stderr captured from real OpenSSH
7.6p1 to 10.6p1 runs, jump hosts included: `ssh_host_key_changed`,
`ssh_host_key_unknown`, `ssh_auth_failed`, `ssh_timeout`,
`ssh_unreachable`, `ssh_connection_lost` (always, once the script printed
its nonce line: `Execution::script_started()`), else `ssh_failed`. **The
cause is ssh's last message** (audit 6): ssh stops at its first fatal
error, and ends every message it logs with `\r\n`, while what it relays
from the server (a login banner, a remote startup file's stderr) ends as
the server wrote it, so the last line ending with `\r\n` decides (lines
that only sum up the one before it skipped), and a banner that says
sessions are "timed out", or quotes `Host key verification failed.`,
decides nothing. Earlier lines refine it: a jump host's `stdio forwarding
failed` takes its class from the channel refusal before it; ssh's closing
`Host key verification failed.` from the lines before it (a key that
differs from the one known for the server's address, with `CheckHostIP`,
and a revoked key are `ssh_host_key_changed`; a host certificate an
`@cert-authority` line rejects, an unreadable `RevokedHostKeys` file and
the refusal alone (a ProxyCommand's own ssh) are `ssh_failed`, as neither
trusting new keys nor forgetting the known one fixes them). Noise (the
`accept-new` notice, a remote locale warning, OpenSSH 10.1's warning
about a key exchange that is not post-quantum, an ssh_config option the
build lacks, a missing ssh_config key file, the lines ssh prints after a
jump host failed) is skipped. `SshFailure::line()` keeps the telling
line, escaped: for a refused login, why ssh could not use the key file
(`Permissions 0644 for '…' are too open.`, `Load key "…": invalid
format`); for a jump host that opened no channel, its refusal; for
`ssh_failed`, ssh's last message (`… line 3: Bad configuration option:
…`, not `…: terminating, 1 bad configuration options`), or, when ssh
printed none, the last line as text. `Error::Ssh(failure)` carries it
with its code. `remote::host_key_notices(stderr)` lists, from ssh's own
lines, the hosts whose new key ssh recorded, whether one could not be
saved, and why ssh refused a host certificate first (with `accept-new`
ssh then trusts its plain key as new, bypassing the certificate
authority).

## WP-CLI commands and parsing (`wp`)

Pure, no I/O: the commands apvm runs, and strict parsers for what they print
(plan §6.2, §6.4, §9).

| Type | What it is |
|---|---|
| `WpCommand` | every command the lifecycle needs, built from typed values only (`Slug`, `StagedZip`): `plugin list --format=json --fields=name,status,version,file,title --skip-update-check --skip-plugins --skip-themes`, `cli version`, `eval` of the constant `snippets::SITE_FACTS`, `option get siteurl` (boot check), `plugin activate`/`deactivate` (`--network`, `--skip-plugins=<slug>`), `plugin uninstall [--skip-delete]` (`reset`: plugin files kept, translations deleted), `plugin delete`, `plugin install <staged zip> [--force]`, `plugin get <slug> --fields=name,requires_plugins`, `eval` of the constant `snippets::REGULAR_PLUGINS`. One slug per process; never `--all`, `--exclude`, `--deactivate`, `--uninstall` or `--activate`. `CommandKind` says which are reads (retried, no lock: plugins skipped), which load plugins (the boot check and mutations) and which change files |
| `StagedZip` | `<remote_tmp_dir>/apvm.XXXXXXXXXX/package-<n>.zip`, built from the site's `remote_tmp_dir`, the `mktemp` name and the package number |
| `WpSite` | a site's hermetic `WpCli` base (no arguments); `command()` appends a `WpCommand` |
| `parse::{plugin_list, regular_plugins, cli_version, site_facts, requires_plugins, boot_check}` | read a `CommandOutput` (exit status, stdout after the nonce, stderr). PHP notices before or after the one-line payload (the PHP 8.5 `Deprecated:` line, shutdown warnings, `PHP Warning:  …`, multi-line messages, PHP and Xdebug stack traces) and byte-order marks are skipped and returned as `Notices`; a non-zero exit, empty output, only notices, an HTML page, any other line, two payloads, truncated or invalid JSON, missing, unknown, duplicate or mistyped fields, and rows whose name does not match their file are refused. Site facts are observations: absolute paths and non-empty URLs without control characters, canonical or not (the `*_realpath` facts are canonical). The boot check passes when `option get siteurl` exits 0 and prints one `http(s)://` line (a plugin can print a page and `exit` with status 0); a different siteurl is a warning. It catches fatals, `die()`, HTML and empty output, not a plugin faking a boot. `Requires Plugins` is read as WordPress reads it |
| `PluginList`, `PluginRow`, `PluginStatus` | the rows, with unknown statuses kept as `Unknown`; lookups by slug follow WP-CLI's plugin fetcher (file `<slug>.php` or `<slug>`, or folder `<slug>`; names are display only, since WP-CLI renames plugins that share one) and return `Absent`, `Found`, `Ambiguous`, `Shadowed` (WP-CLI hides a regular plugin behind a must-use plugin or drop-in at `<slug>.php` or `<slug>`) or `Unreadable` (a file name that is not valid UTF-8 may match); `regular_plugins` + `find_regular` resolve the last two. Lifecycle commands and `requires_plugins` take a `Resolved` plugin (`PluginList::resolve`: `Found` only), so no command names a slug WP-CLI could take for another plugin, and `--no-load` skips by WP-CLI's own plugin name; rows WP-CLI prints with a `null` name (not valid UTF-8) are counted, never matched |

## Probe, identity and sessions (`wp::probe`, `Session`)

`wp::probe::probe(transport, site)` runs plan §6.2 read-only, one round trip
per step, each step's commands reporting their own status:

- **H (login user):** `id`, `$SHELL`, `command -v` of sudo, runuser, the
  checksum tools and `wp_cli`; the **walk** of the plugins directory: one
  `sh` process enters it component by component, then prints `pwd -P` and
  `stat .`, so the physical path and the owner describe the same directory.
  It follows a symbolic link only when the link and the directory holding
  it belong to root or the login user and neither group nor others may
  write that directory (a release layout's `current -> releases/<n>`), and
  checks every component of the link's target the same way; anything else
  is `plugins_dir_mismatch`. A directory the login user cannot enter is
  read by root's `stat` (`sudo -n -u root`), and the identity's own walk
  must then find the same owner and group right before WordPress loads.
- **I (identity):** the identity table (`remote::plan_identity`): by the
  plugins directory's owner and group **ids**, never names; `sudo -n -u U
  -g G`, `sudo -n -u U`, `runuser -u U -g G` for a root login, or no prefix;
  root only with the site's `allow_root` (`--allow-root` exactly when the
  identity's uid is 0). Each candidate runs `cli version` in the exact
  hermetic shape; sudo's refusals (sudo 1.9 and sudo-rs wording) are
  classified with a targeted `sudo -n -l`, and the message names the
  sudoers line to add.
- **W (WordPress):** the constant site facts snippet; WP-CLI's own "not a
  WordPress installation" and "not installed" errors become
  `not_wordpress` / `wp_not_installed`; `WP_CLI_VERSION` must match `cli
  version`.
- **O:** `realpath(WP_PLUGIN_DIR)` must be step H's directory.
- **C:** `test -w`, `df -Pk`: warnings (`ProbeWarning`), never failures.

`Session::open(transport, site)` probes, then runs `WpCommand`s as the
identity (`run`), with the transport's truncation flag passed to the
parsers. Plugin lists read in a session are bound to it (`SitePlugins`), and
commands that name a plugin run only through `run_on(&SiteTarget, TargetOp)`
with a target of that session.

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
- `package_invalid`: a plugin zip that breaks an inspection rule;
- `process_failed`: a local program a transport runs (`ssh`, a shell)
  could not be started, or its output read, or an upload's file could not
  be opened or read for a reason other than the caller's path (missing,
  not permitted or not a regular file are `invalid_input`).
- `wp_command_failed`: a WP-CLI command failed, or printed output a parser
  refuses (`Error::WpOutput` with its `OutputDefect`);
- `ssh_missing`: the ssh program cannot be started, or does not report its
  defaults like OpenSSH (`Error::SshMissing`, `Error::SshUnusable`); the
  file a call writes for ssh failing is `process_failed`
  (`Error::SshConfigFile`);
- `ssh_auth_failed`, `ssh_host_key_unknown`, `ssh_host_key_changed`,
  `ssh_unreachable`, `ssh_timeout`, `ssh_connection_lost`, `ssh_failed`:
  ssh itself failed (`Error::Ssh`, from `remote::classify`); `ssh_failed`
  also when the server's login shell did not run apvm's script
  (`Error::ScriptNotRun`);
- `timeout`: a remote command ran past its timeout (`Error::Timeout`, which
  says whether the script had started);
- `sudo_missing`, `sudo_password_required`, `sudo_not_allowed`,
  `sudo_requiretty`, `owner_is_root`, `plugins_dir_mismatch`,
  `wp_cli_missing`, `not_wordpress`, `wp_not_installed`, `probe_failed`: the
  probe's failures (`Error::Probe` with its `ProbeFailure`);
  `plugins_dir_not_writable` is a probe warning's code
  (`ProbeWarning::code`); a killed local transport is `process_failed`
  (`Error::Killed`).

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
  after 30 s;
- transports: capture limits at the exact boundary (8 MiB stdout, 1 MiB
  stderr) with truncation flags and 20 MiB drained without blocking;
  unframing (noise with and without a final newline, noise past the
  limit, look-alike lines, CRLF, a cut nonce, a missing one, a cut
  payload); 255 → 254 and 255 as a transport failure; 20 MB uploads
  hash-equal from memory and from a file; a child that stops reading stdin
  or closes its output early; a background job holding the output; a
  missing program, a missing upload file, a directory or a FIFO; the
  scripted fake transport later phases test with. A timeout, a dropped
  call and a `ProcessScope`'s `kill_all` kill the child and its
  descendants: the scripts record their PIDs,
  and each must disappear (`kill(pid, 0)`; a stopped process fails). The
  tests use no `ps`, but on macOS the code under test reads the process
  table with `/bin/ps`: where a sandbox forbids it (`sandbox-exec`), the
  descendant checks are skipped. Timeout tests check only what marker
  files prove happened, so they hold under CPU load.
- the SSH transport: the generated file for Unix and Windows paths, every
  setting at every level of the inventory in the argv (and nothing a jump
  host would need on the command line), `%` and quoting rules, refused
  paths with their field, `~` and relative paths, the checks before ssh
  (missing files, a known_hosts link to `/dev/null`, a read-only one with
  `accept-new`, a group-writable ssh_config; the system ssh_config ssh
  would refuse to include, an unreadable default one left out, the
  vendor copy when the system file is missing), finding
  the system ssh_config from `ssh -G` output, where the private directory
  goes, the environment; and, through the
  hermetic `tests/support/fake-remote/fake-ssh` (Unix), the argv and file
  ssh gets (mode `0600`, alone in a `0700` directory, deleted after the
  call, after a timeout and when the call is dropped, with ssh's process
  tree gone), the pinned session options only for a client that knows
  them, the system ssh_config included, refused or left out, concurrent
  calls in directories of their own,
  uploads, a missing or unusable ssh, `accept-new`, and every failure
  class;
- ssh failure classes on stderr samples captured from OpenSSH 7.6p1 to
  10.6p1 (glibc, musl and macOS wording; noise lines anywhere, a remote
  command's same phrases never classified, a login banner or remote text
  before ssh's last message deciding nothing, a lost connection once the
  script started);
- WP-CLI commands: argv goldens per command, banned flags never present,
  apvm's global options set exactly once, every command's argv delivered
  verbatim through `sh` (Unix); parsers on real outputs captured from the
  E2E harness (`tests/fixtures/wp/`, provenance in `wp/parse/tests.rs`) and
  on synthetic ones (truncated, unknown status, wrong types, extra or
  duplicate fields, large lists).

`tests/ssh_e2e.rs` runs the SSH transport against the E2E harness's real
sshd (`tests/e2e/sites/`), gated by `APVM_E2E_SITES=1` (`eval "$(
tests/e2e/sites/harness.sh env)"`; CI's `e2e-sites` job runs it): a script
and an upload, ssh's own `UpdateHostKeys` default with ssh's own
known_hosts files, an unknown host key refused then recorded with `accept-new`,
a changed key, a wrong key, a closed port, a jump host that checks host
keys with apvm's file and records both keys with `accept-new`, an
ssh_config with `SessionType none`, `StdinNull yes`,
`ForkAfterAuthentication yes` and a 1 s `ChannelTimeout` that changes
nothing, and a timed-out call
through a jump host that leaves no ssh process behind. It never touches
the user's own `~/.ssh`.

`tests/probe_e2e.rs` runs the probe against the same harness (same gate;
CI runs it too): each harness user's identity on each site, every sudo
refusal, a moved `WP_PLUGIN_DIR` (pinned and not), an explicit owner, a
hostile WP-CLI config and package, links the site planted (refused before
WordPress loads), a root-owned release link (followed), a 0750 plugins
directory read through root's `stat`, and the unusual login shells. Tests
that change a site restore it, even when they fail. The probe also runs
hermetically (Unix) through `fake-ssh`, `fake-sudo` and `fake-wp`
(`tests/support/fake-remote/`) and this machine's real `sh`, `id`, `stat`
and `readlink`.

`tests/inventory.rs` loads the example document of the plan through the
public API; `tests/targets.rs` resolves targets and settings on it.
`tests/inventory_io.rs` runs concurrent updates from several processes,
reproduces a tracked save's conflict with an edit from another process, and
checks `APVM_INVENTORY`. `tests/package.rs` inspects a BackWPup Pro zip
through the public API.
