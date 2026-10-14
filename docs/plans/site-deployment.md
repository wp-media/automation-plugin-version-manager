# Site deployment — manage WordPress plugins on remote sites

Status: **in progress**: P0 done (verified locally on macOS and Ubuntu clients, and in CI on 2026-10-11); P1 done: P1.1 (2026-10-11, audited 2026-10-12), P1.2–P1.6 (implemented and audited 2026-10-12); done 2026-10-12: P2.1, P2.2, P2.4, P3.1, P3.2, P4.1; done 2026-10-13: P2.3, P3.3, P3.4, P4.2. Rewritten 2026-10-09; revisions 2 and 3 (review feedback), 4 (P0 results), 5 (adversarial audit, 2026-10-11), 6 (CI on the branch), 7 (P1.1), 8 (P1.1 audit), 9 (P1.2–P1.5), 10 (P1.2–P1.5 audit), 11 (decisions after P1), 12 (no lost edits from Node) and 13 (P1.6 and its audit), 14 (P2.1, P2.4, P3.1), 15 (P2.2, P3.2, P4.1, and the shared `apvm-process` crate) and 16 (P2.3, P3.3 and their audits). · Target version: **3.4.0** (from 3.3.0) · Author: Sandy Figueroa

---

## TL;DR

- **What:** a new **library crate `apvm-sites`**, used by the CLI, the Node bindings (NAPI) and `apvm-core`.
  - **Inventory:** servers → sites → groups, with one default SSH key and per-server overrides.
  - **Plugin operations:** install (apvm builds, cached or built on demand, or **any zip**), list, **status checks** (is it installed/active, which version/variant), activate, deactivate, uninstall, reset (uninstall but keep the plugin's files; its translations are deleted), delete, and ownership check/fix. They work by slug (or by apvm project, or `--all`), on one site or a batch of sites.
  - **`apvm wp run`:** runs any WP-CLI command on many sites, safely.
- **How it stays safe:** every command runs **plan → confirm → apply → verify → report**. Each step is one WP-CLI process, confirmed by **re-reading WordPress state**; exit codes are never trusted alone. Removal is always **two-phase** (deactivate, verify, then uninstall in a separate process).
- **Variants are managed:** apvm knows which slug each variant installs to and tells pro-de from pro-en on a site.
  - **Free and pro are independent plugins:** each can be installed, activated, uninstalled or reset on its own, and both can be installed or active at the same time (coexistence tests). Nothing is blocked because of a sibling; the plan only notes what BackWPup's own guards will do.
  - **Activating next to an active sibling** switches by default (deactivate the sibling, then activate, as separate processes); `--keep-siblings` keeps both active.
  - **pro-de and pro-en share one folder:** asking for both is an error; `--force` replaces one with the other.
- **Ownership respects owner *and* group:** WP-CLI runs as the plugins directory's owner **and group** (`sudo -n -u <owner> -g <group>`), so `ftpuser:www-data` sites stay `ftpuser:www-data` with the group bits the web server needs. Mismatches are repaired without root (`chgrp`, `chmod g+`); a root `chown` runs only on sites that opt in, under strict preconditions.
- **Uploads:** each zip is inspected locally, streamed over SSH into a private **staging dir** (`/tmp/apvm.XXXXXXXXXX`, owned by the identity, `0700`), checksum-verified, installed from there, and always cleaned up.
- **Transport:** system OpenSSH (your `~/.ssh/config`, agent, ProxyJump), multiplexed on Unix. Scripts travel over SSH's stdin, so the remote login shell (bash, zsh, fish, …) never parses them.
- **Phases:** 45 small ones; libraries first, then the CLI and NAPI front ends in parallel tracks. No new crates.io dependencies.

---

## 0. Change log

### Revision 17 (P3.4 implementation, 2026-10-13)

| Topic | Change |
|---|---|
| Multiplexing | §6.1 item 9 rewritten from verified facts: the socket path limit is 86 bytes on macOS and 90 on Linux (not "100"), names carry an 8-character tag of the whole configuration before `%C`, the control directory must be private and hold a socket, and every reason for it to be off is reported, never an error. Details: P3.4 Result |
| Per-server limit | `ServerLimits` (4 per inventory server, shared through `SshOptions`), §6.1 item 10 |
| Audit round 5 (P3.4) | A master is looked for by connecting to its socket (its hello), not `ssh -O check`, which a slow ssh_config delayed; lows. Details: P3.4 Result "Audit round 5" |
| Audit round 4 (P3.4) | A stuck master is bypassed and its socket removed when `-O check` gets no answer (before 9.6 calls hung); a starter that lost a race is stopped on ssh's line; process-wide gates; `-G` as for a call and for a master (`Match command`). Details: P3.4 Result "Audit round 4" |
| Audit round 3 (P3.4) | Only direct connections are shared: ssh hides a jump host's or ProxyCommand's messages from a `ControlPersist` start (high); the starter is `-N` (medium); calls waiting on a failed start share it (medium); lows. Details: P3.4 Result "Audit round 3" |
| Audit round 2 (P3.4) | Calls never become masters: a starter of their own does, so killing a timed-out call cannot cut the calls riding the master (high); shared keep-alives 10 s × 3 (medium); a stopped master's socket is removed (medium); lows. Details: P3.4 Result "Audit round 2" |
| Audit (P3.4) | A dead master stalled calls 75 s: shared connections now use 5 s × 3 keep-alives and `ControlPersist 15s`, and a call that timed out before its script stops its master (`ssh -O stop`); stopped-master and dead-master lines are classifier noise; opt-in `/tmp/apvm-mux-<uid>` for a too-long control directory; ancestors checked; `SshOptions` loses `PartialEq`. Details: P3.4 Result "Audit" |

### Revision 16 (P2.3 and P3.3 implementation and audits, 2026-10-13)

Every fact below was verified with OpenSSH 10.3p1 (macOS), 9.6p1 (Ubuntu 24.04) and 10.0p2 (Debian 13) clients against the harness; the audit added 7.6p1 (Ubuntu 18.04, the plan's oldest client) and 8.2p1 (Ubuntu 20.04), the second audit 8.7p1 (Fedora 35), 8.9p1 (Ubuntu 22.04), 9.2p1 (Debian 12) and 10.6p1 (Alpine edge), with the whole Rust E2E suite on all eight Linux clients; the third audit re-checked what its fixes rely on with 7.6p1, 8.2p1, 9.2p1, 9.6p1, 10.0p2 and 10.3p1, the fourth with 7.6p1, 8.2p1, 9.2p1, 9.6p1 and 10.3p1, and added 10.5p1 (openSUSE Tumbleweed, the whole Rust E2E suite as a non-root user); the fifth captured every new classifier sample with 7.6p1, 8.2p1, 9.6p1, 10.0p2 (Debian 13, and Alpine 3.22's musl build) and 10.3p1 against a lab sshd 10.0p2 (host certificates, `DisableForwarding yes`, `MaxSessions 0`) and fake servers, and compared the generated file with plain ssh on Debian 13, Ubuntu 24.04, Fedora (10.2p1), AlmaLinux 8 (8.0p1) and 9 (9.9p1), Alpine 3.22, Arch (10.6p1), openSUSE Tumbleweed (10.5p1), nixpkgs (10.5p1) and macOS; the sixth recaptured every class (33 failure scenarios, a lab sshd 10.0p2 and fake servers) and a server's login banner with 7.6p1, 8.2p1, 8.9p1, 9.2p1, 9.6p1, 10.0p2 (glibc and musl) and 10.6p1, and ran the Rust E2E suite with 10.3p1 (§3.5 "Verified in P3.3", "Classification table").

| Topic | Change |
|---|---|
| Jump hosts (rev 8 obligation) | **Decision: every option goes into a generated `-F` file; apvm does not chain hops itself.** ssh runs a jump host as `exec <argv0> -l U -p P [-J rest] -F <file> [-v] -W '[%h]:%p' <hop>` through the user's shell: `-F` is the only channel. With `-o`/`-i` alone the hop used `~/.ssh/known_hosts` and the default key, and prompted on a terminal despite `BatchMode`; with the generated file it used only that file, never prompted, and `accept-new` recorded both keys there. Chaining hops would nest ProxyCommands (quotes inside quotes through the local shell, whose rules differ in fish and tcsh) and would still leave a ProxyJump from ssh_config unprotected; the file covers both |
| Generated file | Top level (so it wins and reaches every hop): `BatchMode yes`, `StrictHostKeyChecking yes`/`accept-new`, `UserKnownHostsFile`, `IdentityFile` + `IdentitiesOnly yes` (when set), `ConnectTimeout`, `ServerAliveInterval 15`, `ServerAliveCountMax 4`, `LogLevel INFO`, `RequestTTY no`, `RemoteCommand none`, `ControlMaster no` + `ControlPath none` (with multiplexing on, P3.4's lines in their place: a call's `ControlMaster no` + `ControlPath`, its master starter's (`-N`) `ControlMaster auto` + `ControlPath` + `ControlPersist 15s`, and keep-alives 10 s × 3 for both; §6.1 item 9), `ForwardAgent no`, `ForwardX11 no`, `ClearAllForwardings yes`, `PermitLocalCommand no`, `Tunnel no`, `AddKeysToAgent no` (audit 2); when the client knows them (the keywords `ssh -F none -G` prints; older clients refuse unknown options), `SessionType default`, `StdinNull no`, `ForkAfterAuthentication no` (8.7+) and `ChannelTimeout none` (9.6+: 9.2 added only sshd's, audit 2) (audit: from an ssh_config they hang the call, drop the script, detach ssh from apvm, or kill a silent command); then `Include` of the inventory's `ssh_config_file` alone (what `ssh -F` reads), else `~/.ssh/config` and the system `ssh_config`; last, for ssh's own known_hosts files, ssh's own default `UpdateHostKeys yes` when the client has it (8.5+, audit 5). `ssh -T -F none -G` runs once per transport, always, in a private directory like a call (audit 2; `-T` audit 5): the system file sits next to the compiled-in `globalknownhostsfile` (`-F` makes ssh skip it, so apvm includes it). Only `-l`, `-p` and `-J` go on the command line: ssh keeps them from hops (a jump list in the file would make every hop jump again) |
| File safety | `apvm-ssh.conf`, `0600`, created exclusively in a new private directory (`0700`, random name) in the temporary directory, both deleted when the call ends (exit, timeout, error, dropped future); ssh runs in that directory and gets the file's absolute path when it is shell-literal, else the bare name, so the `-F` value pasted into a hop's shell command is literal whatever `TMPDIR` or a Windows user name holds (a relative `-F` reaches the hop, verified). Absolute first (audit 2): the hop's command runs through the user's `$SHELL`, which reads a startup file first (zsh's `.zshenv`, fish's `config.fish`, bash's `BASH_ENV`), and one that changes directory made a relative `-F` miss (`Can't open user config file`, verified). The directory is private (audit): ssh, a hop's `exec ssh …`, and an ssh_config's `ProxyCommand`/`Match exec` resolve a relative `PATH` entry there (`PATH=:/usr/bin`: an empty entry is the current directory), and in a shared `/tmp` another user's planted `ssh` ran (verified). The ssh program's path is pasted too: it must be shell-literal after `~` expansion |
| known_hosts (T6) | Always apvm's: the inventory's `known_hosts_file`, else `~/.ssh/known_hosts ~/.ssh/known_hosts2`; a `UserKnownHostsFile` from ssh_config no longer applies (an ssh_config can still add trusted keys with `GlobalKnownHostsFile`, `KnownHostsCommand` or `VerifyHostKeyDNS`, never turn checking off) (NetBird's system config, for one, sets `/dev/null` with `StrictHostKeyChecking no` for its hosts). At use, in both modes, a file that exists but is not regular is refused (a link to `/dev/null`); with `accept-new`, also a file or (when it does not exist yet) a directory this user cannot write, or a missing directory: ssh "permanently adds" to `/dev/null`, or fails to add, and runs the command either way |
| Local paths | apvm expands `~` with the home it is given (else `HOME`) and writes absolute paths: ssh takes `~` from the password database, ignores `HOME`, and reads a relative `Include` from `~/.ssh`. Paths are double-quoted (spaces, `#`, `'`, `=` stay literal); `%` is doubled in `IdentityFile`; `"`, control characters, `\` (Unix), `${` (expanded in key paths), `%` in known_hosts paths (audit: ssh before 8.4 reads `%%` there literally, so no spelling works on every version) and `Include` patterns are refused (`invalid_input`); Windows paths get `/`. `IdentityFile` adds up: ssh_config's own keys are offered after the inventory's |
| `ssh_config_file` | Must exist (a missing `Include` is silent, so another host could be reached) and must not be writable by group or others nor belong to another user: ssh refuses to `Include` such a file, though `ssh -F` reads it |
| Classification | `remote::classify` (exit 255 only), phrases from stderr of the five clients, jump-host failures included (audit: `ssh_exchange_identification:`, 7.6p1's name for the banner phase, is unreachable, not a lost connection). New codes: `ssh_connection_lost` (exit 255 after the script printed its nonce, `Execution::script_started()`: never an auth or host-key class) and `ssh_failed` (unrecognized; the last line is kept). `host_key_notices` lists keys `accept-new` recorded (as ssh names them: `name,address` before 8.5, whose `CheckHostIP` default is `yes`), and an unsaved one. ssh ends its own lines with CR LF, a remote command's end with LF. Noise also: OpenSSH 10.1's three-line warning about a key exchange that is not post-quantum (audit 2: alone in a lost session's stderr, it was kept as the explanation) |
| Environment, errors (P3.2 obligations) | ssh gets apvm's environment minus `GITHUB_TOKEN`/`GH_TOKEN`, plus `LC_ALL=C` and `SSH_ASKPASS_REQUIRE=never` (audit 2: an ssh a user's ProxyCommand starts inherits it and opens no ssh-askpass dialog, 8.4+, verified). A spawn `NotFound`/`PermissionDenied` is `ssh_missing`, and so is a program that cannot report its defaults with `-G` |
| E2E | verify.sh: `ssh.jump-host-options`, `ssh.jump-host-shell-startup`, `ssh.jump-host-isolation`, `ssh.known-hosts-persistence`, `ssh.generated-config-rules`, `ssh.agent-isolation`, `ssh.system-config-include` (checks needing a client home of their own run the target's client as a throwaway user). `crates/sites/tests/ssh_e2e.rs` runs the transport against the harness, in CI's `e2e-sites` job (which fails, not skips, when `harness.sh env` fails) |
| `apvm group` (P2.3) | Implemented in parallel with P3.3 in a separate worktree; two fresh adversarial audits (no high or medium finding). §5.2 gains `group edit --unset notes` (an empty `--notes` is refused, so notes could not be cleared); unknown sites are checked under the lock after the group's own checks; the unknown-group remedy is `'apvm group list' shows the groups` everywhere. Details: P2.3 Result |
| Audit (2026-10-13) | Findings fixed: an ssh_config's `SessionType`/`StdinNull`/`ForkAfterAuthentication`/`ChannelTimeout` could take the session away (now pinned when the client knows them); the generated file's shared directory let a relative `PATH` entry run a planted program (now a private directory per call); `%%` in `UserKnownHostsFile` is literal before 8.4 (now `%` refused there); `ssh_exchange_identification:` was a lost connection; CI's E2E step skipped silently if `harness.sh env` failed; docs overstated "the inventory's key only" and "apvm's known_hosts only". The generated-file design itself held on 7.6p1 and 8.2p1 (quoting, hops reading `-F`, strict checking reaching them) |
| Audit 2 (2026-10-13) | Findings fixed: an ssh_config's `AddKeysToAgent yes` put the inventory's key into the user's agent for good, and `ask`/`confirm` open ssh-askpass whatever `BatchMode` says (now `AddKeysToAgent no`, valid 7.6p1–10.6p1, first value wins); a shell startup file that changes directory broke every jump host (now the absolute `-F` path when shell-literal); `-G` ran in apvm's directory, so a relative `PATH` entry could probe another ssh than the calls run (now a private directory); a user's ProxyCommand ssh could open ssh-askpass (now `SSH_ASKPASS_REQUIRE=never`); OpenSSH 10.1's post-quantum warning was taken as a failure's line; `ChannelTimeout` is a 9.6 client option, not 9.2 (docs); the missing-directory refusal now names the directory and the fix. Evaluated and kept: ssh shares apvm's session (Ctrl+C must reach it; apvm has no signal handling to forward it), so a ProxyCommand's ssh can still ask on the terminal; `SendEnv`/`SetEnv`/`UpdateHostKeys`/`KnownHostsCommand`/providers stay the user's (below). Tests: the Rust E2E suite now covers the session pins on the oldest clients that accept them (8.7p1, ChannelTimeout 9.6p1) and runs on eight Linux clients |
| Audit 3 (2026-10-13) | Findings fixed: **the system ssh_config** (included without `ssh_config_file`) failed every call where plain ssh works: ssh reads it unchecked on its own, but checks any included file, so one owned by another user (a Homebrew install of another account) or one its group may write gave `Bad owner or permissions`, and one this user cannot read gave `Can't open user config file` (plain ssh skips it); now a default file this user cannot read is left out (as ssh skips it, `~/.ssh/config` too), and a system file ssh would refuse is refused before ssh runs, `invalid_input` at `ssh_config_file`, with what to do (silently skipping it could change the host or options used); **the bare-name fallback** still broke jump hosts under a shell startup that changes directory whenever the temporary directory's path was not shell-literal (a `TMPDIR` with a space): the private directory now goes in `/tmp` then (Unix, when root's and sticky, so it stays private), and only a configured directory or a Windows temporary directory with a space still gets the bare name (10.3p1, `TMPDIR` with a space, a jump host, `BASH_ENV` running `cd /`: `Can't open user config file apvm-ssh.conf` before, the call runs after); **a key file others can read** read as a bare `Permission denied (publickey)`: the classifier now keeps `Permissions 0644 for '…' are too open.` as the line of an `ssh_auth_failed` (refusing such a file up front would break the agent case); ssh's defaults are now cached per program (a relative `binary` after apvm changed directory got another program's pinned options); the include check uses the real user id, as ssh does. Evaluated and kept: a ProxyCommand's ssh can still prompt on the terminal: dropping the controlling terminal without a new session (`TIOCNOTTY`) keeps Ctrl+C on Linux but loses it on macOS, where the terminal signals only processes that still have it (verified on a pty), and a new session loses it everywhere; `shell_literal`'s set (`[A-Za-z0-9._/:+-]`) is literal in sh, bash, dash, zsh, fish, tcsh and csh (none of `~`, `!`, `{}`, `*?[]`, a leading `=`, `#`, `^`, `%` or non-ASCII; a word of every allowed character came back unchanged from each `-c`, in the target). Re-verified audit 2's fixes (`AddKeysToAgent no`, the absolute `-F`, `-G` in a private directory, `SSH_ASKPASS_REQUIRE=never`, the post-quantum lines in the 10.3p1 binary, `ChannelTimeout` refused by 9.2p1 and printed by 9.6p1's `-G`, the 7.6/8.2 E2E assertions) |
| Audit 4 (2026-10-13) | Findings fixed: **openSUSE's system ssh_config** (medium) was silently left out: Tumbleweed ships it as `/usr/etc/ssh/ssh_config`, with none in `/etc/ssh`, and its ssh reads that copy when `/etc/ssh/ssh_config` does not exist (a dangling link counts as missing; an existing, even unreadable, one wins), so apvm's `Include` of the missing `/etc` file was silent and dropped the whole system configuration, `/etc/ssh/ssh_config.d/*.conf` drop-ins included (an admin's `Host` alias resolved by plain ssh was not by apvm: `hostname apvm-alias` with the generated include, `hostname target` with ssh alone, 10.5p1); now the vendor copy under `/usr` is included when the system file does not exist and it does (the call through the alias runs, 10.5p1); **a host key that differs from the one known for the server's address** (medium) read as `ssh_host_key_unknown` (its last line is the generic `Host key verification failed.`), whose remedy, `--trust-new-host-key`, cannot help (refused with `accept-new` too) and is wrong for a possible spoofing: with `CheckHostIP yes` (the default before 8.5, an ssh_config's choice after) it is now `ssh_host_key_changed`, kept line `Warning: the ECDSA host key for 'target' differs from the key for the IP address '172.19.0.3'` (7.6p1, 8.2p1, 9.6p1); likewise **a key the user marked revoked** (`@revoked` in known_hosts: `… was revoked and you have requested strict checking.`, 7.6p1, 8.2p1, 10.3p1, strict and accept-new; `RevokedHostKeys`: `Host key … revoked by file …`, 10.3p1) (low); the harness check `ssh.system-config-include` kept its backup in `/tmp/apvm-e2e-*`, which `harness.sh reset` deletes, and overwrote a stale one, so a run killed before its cleanup left the target's `/etc/ssh/ssh_config` changed for good (now the backup is `/etc/ssh/ssh_config.apvm-e2e-orig`, restored first when present; reproduced in a 9.6p1 and a 9.2p1 container) (low); two `fake-ssh` tests read this machine's `/etc/ssh/ssh_config` (refused on a machine whose file its group may write, another file on openSUSE now): they use the `-sysconf` link (low); `fake-ssh` listed its directory with `for entry in .* *`, and where `/bin/sh` is bash 5.2+ (openSUSE's 5.3, verified: `globskipdots` keeps `.*` from matching `.` and `..`) an unmatched `.*` stays literal, so the directory check failed (now skipped like an unmatched `*`) (low); docs said only a configured or a Windows temporary directory gets the bare name (also a Unix `/tmp` that is not root's and sticky) (low). Evaluated and kept: the include rule matches OpenSSH's `readconf.c` exactly on 7.6p1–10.2p1 (`(uid != 0 && uid != getuid()) \|\| (mode & 022)`, `fstat` of the opened file so links are followed, no check of parent directories; an `Include` is a `glob(…, GLOB_TILDE)`, so a path component this user cannot search, or one that is a file, matches nothing and is silent, as apvm keeps it; an existing file that cannot be opened fails the call, as apvm leaves it out); `access(2)` uses the real user id and ssh's `fopen` the effective one (the same for apvm, which is never set-user-ID); a file that becomes unreadable between apvm's check and ssh's read fails the call (`Can't open user config file`, `ssh_failed`), one that becomes readable stays out, as a call a moment earlier would have; the `/tmp` fallback is sound (a random name created with an exclusive `mkdir` at `0700`, so a planted name or link is skipped; a sticky root directory keeps others from renaming it; macOS's `/tmp` link is root's and its target `/private/tmp` root's and sticky; the mode only loses bits to the umask); the temporary directory itself is trusted, as by every program (documented); the classifier's `are too open.` line only explains an `ssh_auth_failed` (exit 255 before the nonce) and is escaped (`printable`); the `-G` cache is one entry per transport, keyed by the program as given (absolute unless a bare name), re-read uncached for another program: a bare `ssh` that `PATH` resolves to another program later keeps the first one's pins (low, documented); `-G` prints `Pseudo-terminal will not be allocated…` on stderr (stdin is not a terminal; ignored on success); the default-config E2E reads this machine's system file (read only; its known_hosts, key and home are the sandbox's) |
| Audit 5 (2026-10-13) | Systematic: every message the client prints at `INFO` or above before exiting 255 (connection, host key, authentication, session and channel setup; `sshconnect.c`, `sshconnect2.c`, `ssh.c`, `readconf.c`, `clientloop.c`, `packet.c`, `kex.c`, `channels.c`, `sshkey.c` at 7.6p1, 8.2p1, 8.9p1, 9.6p1 and 10.6p1, the banner-phase boundaries at 7.9p1, 8.0p1, 9.0p1–9.5p1), each mapped to a class (§3.5 "Classification table"), and the generated file against plain ssh on ten platforms (§3.5). Findings fixed: **a host certificate an `@cert-authority` line rejects** (medium) read as `ssh_host_key_unknown`: ssh prints `Certificate invalid: name is not a listed principal` (or `…: expired`), retries the certificate's plain key, which is new (`No ED25519 host key is known …`), so the remedy `--trust-new-host-key` would bypass the CA: with `accept-new` the call ran (exit 0) and the plain key was recorded next to the `@cert-authority` line (7.6p1, 8.2p1, 9.6p1, 10.0p2, 10.3p1); now `ssh_failed` with the certificate line, and `host_key_notices().refused_certificate` reports it on a call that ran; **a bare `Host key verification failed.`** (a ProxyCommand's own ssh asking nobody) and **an unreadable `RevokedHostKeys` file** (`Error checking host key … in revoked keys file …`) (medium) read as `ssh_host_key_unknown`, whose remedy cannot reach the one and is wrong for the other: now `ssh_failed` with that line (`ssh_host_key_unknown` needs `No … host key is known for …`, the one strict-checking message, the hop's included); **ssh's `UpdateHostKeys` default** (low) was lost: plain ssh 8.5+ turns it on when it uses its own known_hosts files, and records the other host keys a server proves with the known one, but naming the files (as apvm must) turns it off (`ssh -G`: `updatehostkeys true` alone, `false` with the generated file, on every 8.5+ client; with the harness, plain ssh learned the target's ECDSA and RSA keys, apvm's call did not), so a server's announced key rotation would have read as `ssh_host_key_changed` for apvm only; now, when the inventory names no `known_hosts_file` and `-F none -G` prints `updatehostkeys true`, the file ends with `UpdateHostKeys yes` after the includes (theirs wins, first value; E2E: three keys recorded, 10.3p1); **specific failures read as `ssh_failed` or with a useless line** (low): any other `ssh: connect to host …` error (musl's `Network unreachable` and `Host is unreachable`, macOS's `Address family not supported by protocol family`, `Host is down`, `Permission denied`), an SSH-1 server (`Protocol major versions differ: 2 vs. 1`), a banner ssh cannot read (`Bad remote protocol version identification`, 9.6+'s `banner exchange: …`), a reset during the handshake (`Connection reset by <ip> port <n>`) are now `ssh_unreachable`; a hop that forwards nothing (`channel 0: open failed: administratively prohibited: open failed`, `stdio forwarding failed`) was `ssh_failed` on 9.6+ and `ssh_unreachable` with `…_exchange_identification: Connection closed by remote host` on 7.6/8.2: now `ssh_unreachable` with the channel line on all; a server that opens no session (`MaxSessions 0`: `channel 0: open failed: connect failed: open failed`, no hop) read as `ssh_unreachable` through the hop phrase: now `ssh_failed`; an unrecognized failure kept `…: terminating, 1 bad configuration options` (the line that names the option came before; a `Received disconnect … : <reason>` likewise before `Disconnected from …`): now the last line that is more than a summary; a key file ssh cannot load kept the bare refusal: now `Load key "…": invalid format` / `…: error in libcrypto` (9.6p1+ on Linux); an ssh_config option the build lacks (`Unsupported option "…"`, printed on every call), a missing ssh_config key file (`no such identity: …`) and 10.6's rekey post-quantum warning are noise; on 7.6p1–9.4p1 the banner line ssh prints after a jump host or ProxyCommand ended no longer hides the hop's own line, so every version classifies a hop failure alike; **`-F none -G` prints `Pseudo-terminal will not be allocated because stdin is not a terminal.` before its output** (low; seen on eight clients, 8.0p1 to 10.6p1), so a failing `-G` would have shown it as its error: now `-T` (empty stderr on every client) and the line is skipped as noise; **Debian's and Ubuntu's include rule** (low): their ssh (`user-group-modes` patch) includes a file of the user's that only its private group may write (`0664` included, `0666` refused; 9.6p1 Ubuntu, 10.0p2 Debian; Fedora, AlmaLinux, Alpine, Arch and openSUSE refuse it), so the refusal's "ssh refuses to read it" was false there: the message now says OpenSSH refuses to include such a file, apvm keeps upstream's rule (it cannot tell a patched ssh; `chmod go-w` fixes it), and `ssh.system-config-include` asserts Debian's behavior; the vendor fallback computed a `/usr` copy for any build's directory (`/usr/opt/homebrew/etc/ssh/ssh_config`, which no ssh reads) (low): now only `/etc/ssh/ssh_config` → `/usr/etc/ssh/ssh_config` (the only vendor path in any binary checked); hardening: the host key classes now need ssh's `Host key verification failed.` line, so a login banner (printed among ssh's lines once the host key passed) that quotes a host key phrase cannot make a refused login one. Evaluated and kept: every platform's system ssh_config is next to the compiled-in `globalknownhostsfile` (`SSHDIR`, binary strings), every shipped `Include` is absolute, every included file is root's and not group- or other-writable (Debian/Ubuntu/Alpine drop-ins, Fedora's and AlmaLinux's crypto-policies link `50-redhat.conf` → `/usr/share/crypto-policies/DEFAULT/openssh.txt` inside `Match final all`, Arch's relative link to systemd's `20-systemd-ssh-proxy.conf`, macOS's `crypto.conf` link, nixpkgs' store files `0444`), `Match final` blocks give identical `-G` output, a user alias and a system drop-in alias resolve through the generated file everywhere, and the only other `-G` differences are the pinned options (`remoteforward` dropped by `ClearAllForwardings`, macOS); `ssh_failed`, not a new code, for host key refusals no class names (the line tells, P4.3 adds the remedy); a login banner or a remote startup file's stderr before the nonce can still hold phrases of the other classes (documented); `Connection closed by <ip> port <n>` during authentication (a server's `LoginGraceTime`) reads as `ssh_unreachable`; `UpdateHostKeys yes` is appended also when an ssh_config sets `VerifyHostKeyDNS yes` (plain ssh's default is then off; keys are still only recorded when proven by the known one); FreeBSD by source only (`SSHDIR` for both files; base `ssh_config` has no `Include`), Homebrew not installed here, NixOS not run (nixpkgs' binary and store modes checked) |
| Audit 6 (2026-10-13) | Findings fixed: **a server's login banner, or a remote startup file's stderr, decided the class** (medium): the classifier took the first class any line held, so text ssh relays from the server before its own last message counted like ssh's; a banner quoting `REMOTE HOST IDENTIFICATION HAS CHANGED` and `Host key verification failed.` (audit 5's gate needed that line anywhere) made a refused login, or a connection the server closed during authentication, `ssh_host_key_changed`, whose P4.3 remedy is `ssh-keygen -R` (real: sshd 10.0p2 `Banner`, clients 7.6p1, 8.2p1, 9.6p1, 10.0p2 glibc and musl, 10.6p1), and a banner that says "Idle sessions are timed out" made a close during authentication or a lost session `ssh_timeout`. **Now the cause is ssh's last message**: ssh stops at its first fatal error and ends every message it logs with `\r\n` (`log.c`, 7.6p1–10.6p1, Win32-OpenSSH), while relayed text ends as the server wrote it (`\n` in every capture), so the last line ending with `\r\n` (summaries skipped) decides and earlier lines only refine it: a jump host's channel refusal before `stdio forwarding failed` (now also `ssh_timeout` when it says `timed out`), the lines before ssh's closing `Host key verification failed.` (matched as the whole line), a refused login's `are too open.`/`Load key` line (ssh's own lines only), a run of one class's messages for the most telling line, and 7.6p1's multi-line `Bad remote protocol version identification` by its first line; with no message of ssh's (a remote shell that printed and exited 255 before the nonce) the failure is `ssh_failed` with the last line as text. `host_key_notices` reads ssh's own lines only, so a remote command copying `Warning: Permanently added …` reports nothing (low). Every one of the 62 samples keeps its class and line; recaptured on eight clients, 33 scenarios each, all as the table (§3.5). Tests: the banner samples (real), orders ssh cannot print (constructed), the `\r\n` gating, the whole-line refusal, a hop timeout; each new rule fails its test when reverted (mutation-checked), and so does each audit-5 rule (the bare refusal, certificates, the host key gate, `Connection reset by` at line start, summaries, the trailing banner line, `no such identity` and `Unsupported option` noise, `-T`, `updatehostkeys`, the vendor path, the trailing `UpdateHostKeys yes`). Changed test expectations: `the_most_specific_class_wins_whatever_the_order_of_lines` is now `sshs_last_message_decides_whatever_came_before_it` (same assertions; the old name described the replaced rule), and `every_class_has_its_code_and_sentence` checks the two new tables instead of one. Verified, kept: `-T` on `-G` (accepted, exit 0, empty stderr, output differing only in `requesttty`, 7.6p1–10.6p1; 7.6p1 also prints the pseudo-terminal line without it); `updatehostkeys` from `-F none -G` is `false` on 7.6p1 and 8.2p1 (no line appended) and `true` on 8.9p1–10.6p1; with `known_hosts_file` set no line is appended, as plain ssh with a named file; the line is top level after the includes (an `Include` restores the including file's `Match` state), so it reaches hops as plain ssh's default does, and an included `UpdateHostKeys no` wins. Evaluated and kept (low, documented): an ssh_config's `UpdateHostKeys` that applies only in ssh's final pass (`Match final`, or a `Host` block for a canonicalized name) loses to apvm's line, read in the first pass (`-G`: plain `false`, generated `true`); wrapping the line in `Match final` would fix that but forces a final pass, which re-matches every `Host` block against the `Hostname` (`-G` through an alias: `User admin` from a `Host <address>` block plain ssh does not apply), and a per-transport `-G` with the user's files would run their `Match exec` commands once more; with `VerifyHostKeyDNS yes` and a key found in secure DNS ssh skips known_hosts (`verify_host_key`), so apvm's line records the server's proven keys where plain ssh would not; a user's ProxyCommand ssh with its own `StrictHostKeyChecking yes` prints what a ProxyJump hop prints for an unknown key (`ssh_host_key_unknown`, whose remedy cannot reach it); `Connection closed by <ip> port <n>` during authentication stays `ssh_unreachable` (only a banner, which apvm does not read, would tell). During this audit the worktree's `target/` (its scratch files included) was deleted from outside it while the volume was full; the build, the eight-client recapture and the banner captures were redone afterwards |

### Revision 15 (P2.2, P3.2 and P4.1 implementation and audits, shared process crate, 2026-10-12)

Three workers implemented the phases in parallel in separate worktrees; each phase then went through fresh adversarial audits and fix rounds until no high or medium finding remained (P2.2: 2 rounds, P3.2: 3, P4.1: 4).

| Topic | Change |
|---|---|
| Shared process crate (user decision) | Core's `process.rs` moved into a new leaf crate `apvm-process` (`crates/process`, no `apvm-*` dependencies). Core re-exports `ProcessScope`; sites spawns only through it, so every transport child is recorded in the caller's `ProcessScope` and napi's teardown kills it at `process.exit()` (the ported copy in sites could not). D1 now allows exactly this one apvm dependency in sites (a test guards the manifest, section by section). Fixes made once: a failed `wait` no longer lets tokio signal a possibly reused PID; on Linux the held child is pinned by a pidfd (§6.1, §8) |
| Framing (P3.2) | The nonce line is printed after a newline (`printf '\n%s\n'`): login noise without a final newline made every call fail. `payload()` is an error for a missing nonce, a capped stdout and an interrupted call (timeout, transport loss, signal), never a shorter payload (§6.1) |
| Uploads (P3.2) | Files are opened `O_NONBLOCK` and type-checked on the handle, so a FIFO swapped in cannot hang a call; a bad path is `invalid_input` |
| WP-CLI lookups (P4.1) | Verified against WP-CLI 2.12.0: `plugin list` hides a regular plugin behind a same-named must-use plugin or drop-in and prints `null` for non-UTF-8 file names, while the fetcher can still act on both; WP-CLI renames plugins whose names repeat. Lookups follow the fetcher's file rules, report `Shadowed`/`Unreadable` (resolved by a constant `REGULAR_PLUGINS` read), and lifecycle commands take a `Resolved` plugin, so "apvm resolved X, WP-CLI acted on Y" cannot be built. `--no-load` skips by WP-CLI's plugin name |
| Boot check (P4.1) | Exit 0 with empty output, HTML or extra lines is a failed boot (a plugin printing a page and exiting returned 0). A changed siteurl is a warning, not a failure (force-https, domain mapping) |
| `reset` (P4.1) | `uninstall --skip-delete` deletes translation files: `reset` changes files and is blocked under `DISALLOW_FILE_MODS` (§6.7, P7.3) |
| Site CLI (P2.2) | `apvm site`; `server` moved onto shared field-table and removal modules with byte-identical output; `auto`/`login` keywords in any letter case (an inventory holding such a value is now invalid on load) |
| Process | A P3.2 fixer ran 24 busy loops to test under CPU load and froze the user's machine; agent prompts now forbid load generators and require cleaning up background and stopped processes |

### Revision 14 (P2.1, P2.4 and P3.1 implementation and audits, 2026-10-12)

Three workers implemented the phases in parallel; two fresh auditors per phase, fixes, then a fresh re-audit of each phase's fixes.

| Topic | Change |
|---|---|
| Script framing (P3.1) | Every script is one brace group `{ … }` whose first command is `LC_ALL=C; export LC_ALL` (was: nonce first, no group). A stdin script cut short at its end ran its partial last command (a `search-replace` lost its trailing `--dry-run`); now it is a syntax error that runs nothing, tested by cutting at every byte. Multi-statement bodies come from typed `Statement`s (`remote/statement.rs`) and one `frame()` (§6.1) |
| Numeric ids (P3.1) | GNU chown, chgrp and find read a bare number as a user/group *name* first: a user named `1001` defeated the chown guards. Ids are rendered `+uid:+gid`; §6.8 uses `find -uid/-gid` and `chgrp +gid` |
| Root (P3.1) | Root commands run under `sudo -n -u root --` (a sudoers `runas_default` changed the user). Root `stat` prints one value per line (`%u %g %a %F %U %G`, names last) and does not follow links: anything but `directory` is refused, since root cannot `realpath` (§6.2) |
| Hermetic WP-CLI (P3.1) | `env -u` removes the 18 environment variables that make WP-CLI 2.12 or PHP load code or run programs (`WP_CLI_REQUIRE`, `WP_CLI_EARLY_REQUIRE`, `PHPRC`, `EDITOR`, …; verified in the 2.12.0 phar), and `PAGER=cat`. Login and `runuser` identities passed them through (§6.1) |
| WP-CLI arguments (P3.1) | WP-CLI reads global parameters anywhere, even after `--` (`--require=`/`--exec=` ran code there). `WpCli::arg` refuses every global in every form except the §6.12 overrides, and a `--url=` override must be a valid site URL. Names match exactly, so `--req=` is not a global (§6.12) |
| Server CLI (P2.1) | Each field is declared once (`server/fields.rs`), after 17 of 27 audit mutants survived (e.g. `server defaults --connect-timeout 30` silently did nothing). New flags: `server defaults --jump/--known-hosts/--ssh-config`, `server add/edit --connect-timeout`, each with `--unset`. `server remove` re-checks the confirmed server, sites and groups under the lock (`inventory_changed`). New code `internal_error`. "Offline" reworded to "no SSH" (the background update check may still contact GitHub) (§4.5, §5.2) |
| Inventory library (P2.4) | `io::update_tracked_with` hands the change the fingerprint of the bytes loaded under the lock (Node no longer re-reads). `io::check_path` + a pre-lock target check: refused targets create no lock file or directory; file names ending in `.` or a space are refused (Windows strips them, so two spellings locked different lock files). A forced save refuses a newer inventory, readable or not, and a file it cannot read; damaged, older and same-version files are still replaced (§7) |
| Node (P2.4) | A rejecting `async` `update()` callback crashed Node even after the caller caught the refusal; the rejection is now handled through the `then` captured at addon load (a subclass overriding `catch` crashed the first fix) and any thenable refused. Paths are checked as given (`x/.` was accepted). Hints fixed: `InventoryLocked` names the `update()` callback only when it caused the wait; `InventoryChanged` no longer suggests `force` (it discards the other process's edit). Non-enumerable keys refused (§11) |
| Process | A P2.1 auditor ran commands against the real `~/.apvm` (a zsh glob error skipped its env setup) and left a test `inventory.json`. Agent prompts now prefix the env on every command |

### Revision 13 (P1.6 implementation and audit, 2026-10-12)

| Topic | Change |
|---|---|
| API | `io::Fingerprint` (SHA-256 of the exact bytes, or missing; opaque, `missing()`, `is_missing()`), `io::load_tracked`, `io::save_if_unchanged` and code `inventory_changed`, as revision 12 says. From the audit: `io::save` now returns the fingerprint of what it wrote, and `io::update_tracked` returns `TrackedUpdate { value, inventory, fingerprint }`. Reloading after a forced save or an `update` to get a fingerprint could pair the object's inventory with another process's later edit, and the next tracked save would then lose that edit (reproduced) |
| Refusals | `save_if_unchanged` compares the file once before taking the lock (a mismatch then is final: every writer replaces the file in one rename) and again under it, so a refused save usually creates no lock file and never re-creates a deleted directory. A file grown past 16 MiB is `inventory_changed`, not `inventory_invalid` |
| Read-only | A tracked save of a newer (read-only) inventory is refused with `inventory_read_only`, even when nothing changed; one replacing a newer file whole is refused too (checked under the lock, as in `update`) |
| Known limit | Deleting `inventory.json.lock` while a writer holds it lets the next writer in (it locks a new file), so two edits can race. apvm never deletes it; the crate README says not to. A post-lock inode check would not help: the second writer's lock file is genuine |

### Revision 12 (no lost edits from Node, 2026-10-12)

| Topic | Change |
|---|---|
| The problem | `SiteInventory` (NAPI) is load → edit in memory → `save()`. A plain save writes back the whole document, so an edit made by another process in between (a parallel test worker, `apvm site add` in a terminal while a long-running Node tool holds the inventory) disappears silently. The CLI is not affected: each command is one `io::update` |
| Library (new P1.6) | `io::load_tracked` returns the inventory with a `Fingerprint` of the exact bytes read (or "missing"). `io::save_if_unchanged` re-reads under the lock and writes only when the file still matches, else fails with the new code `inventory_changed` (nothing written); it returns the new fingerprint, so later saves chain. Byte-identical content is not a conflict; a file deleted or created since the load is |
| Node (P2.4) | `save()` refuses with `InventoryChanged` when the file changed since `load()`; the object keeps its unsaved changes, so the app reloads, reapplies and saves. `SiteInventory.update(fn)` runs load → `fn` → save in one locked step (the CLI's path), so it never conflicts. `save({ force: true })` overwrites on purpose (e.g. restoring a backup). §4.5, §7, §8.1, §11 |

### Revision 11 (decisions after P1, 2026-10-12)

| Topic | Change |
|---|---|
| Inventory lock | Writers wait at most 10 s (`io::LOCK_WAIT`) for a busy lock, polling `File::try_lock` (5 ms doubling to 100 ms), then fail with the new code `inventory_locked`; nothing is written. Before, a stuck holder blocked every writer forever (§4.5, §7) |
| `apvm info` | The `Family:` section added in P1.4 stays |
| Same WordPress | The revision 10 key stays (exact host, port, user, `proxy_jump`, `ssh_config_file`, path); P6.4's realpath key covers the rest |

### Revision 10 (adversarial audit of P1.2–P1.5, 2026-10-12)

| Topic | Change |
|---|---|
| Inventory writes | `update` refuses a newer (read-only) inventory even when the change replaces the document whole (it was rewritten as version 1). Documents over 16 MiB are refused on write as on read, so apvm never writes a file it would refuse to load. Paths that name no file (empty, `/`, `..`, a trailing `/` or `/.`) are refused by `load` and `save` alike (§7) |
| Same WordPress | Hosts are compared exactly, not case-folded: `ssh -G` (OpenSSH 10.x) matches `Host` patterns case-sensitively, so `Prod` and `prod` can reach different machines (verified). The key also holds `proxy_jump` and `ssh_config_file`: one internal address behind two bastions, or one alias in two ssh_config files, can be two machines (§4.2) |
| Zip readers | WordPress unzips with libzip (`ZipArchive::CHECKCONS`) and falls back to PclZip when libzip refuses a zip; PclZip uses the *first* end record, ignores Unicode path fields and treats DOS-folder-attribute files as folders. Verified with WordPress 7.1.3's code: a crafted zip inspected as `good` installed `evil/evil.php`. Inspection now refuses any zip the readers could read differently: one end record whose comment ends the file, a central directory that ends at it and holds exactly the declared records, consistent ZIP64 records, no renaming Unicode path field, no DOS folder attribute on files, no duplicate names (libzip refuses them; zip 8.6 merges them, which also bypassed the limits) (§6.10) |
| Zip limits | The entry count comes from the end record, before the zip crate parses entries (1 M entries took 663 MB before; now refused in 71 ms) (§6.10) |
| Headers | `Network: 0` falls back to `Site Wide Only`, as in WordPress (0 differences on 40 002 random headers against WordPress's own regexes under PHP); one-pass parsing (a 99 000-file zip took 12 s). Names stored as UTF-8 without the UTF-8 flag (macOS `zip`/`ditto`) are decoded as libzip does |
| Core hints | One helper feeds both `plugin_hints` and `project_for_slug`, so they cannot drift apart. Every value re-checked against all 43 cached artifacts (§3.1) |
| Later phases | Obligations noted in P2.1, P2.4, P8.1, P8.5 and P10.1 |

### Revision 9 (P1.2–P1.5 implementation, 2026-10-12)

| Topic | Change |
|---|---|
| Inventory file | `io::{load, save, update, env_override}`. `load` takes no lock (a save is one rename). `update` writes only when the change succeeds and changes something. The lock file is created `0600` and never removed. Parents are created `0700`. Directory fsync is best effort: once renamed, the save has landed. Reads are capped at 16 MiB. Symlink refusal covers the file and its lock file (not the directories above: macOS's `/tmp` is a link); directories and special files are refused too (§7) |
| Settings | Every resolved value carries its source (`built-in`, `defaults`, `server`, `site`). SSH merges field by field: server, `defaults.ssh`, built-in (connect timeout 15 s, multiplex on, program `ssh`); a field none sets is left to ssh_config. Site settings: the site's value, else the built-in one. `command_timeout_secs` stays optional (operations own their defaults) (§4.2) |
| Targets | Union in inventory order, each site once; unknown ids reported first, with a hint when only case or spaces differ; a selection that matches no site is an error like an empty one; a selected group or server that adds no site is a warning (`NoSites`) (§4.2) |
| Core hints | `Apvm::plugin_hints` and `ProjectRegistry::plugin_hints` return `Result<PluginHints>`: a registered custom builder can declare invalid hints. `Error::Sites` and its napi mapping land here, not in P8.5. `Project::plugin_slugs` and `BackWPupBuilder::SLUG_FREE`/`SLUG_PRO` added; `apvm info` also shows a `Family:` section (§8.1) |
| Package | Two extra files, `package/archive.rs` (all zip I/O) and `package/layout.rs` (raw directory checks, revision 10). The file must be named `*.zip`. Only Stored and Deflated entries (what every WordPress unzip path handles). Every entry is decompressed and its size and CRC-32 checked, under a read budget (4× the file size + 1 MiB) that stops shared-data entries and fake end records (zip 8.6 retries each candidate). A top-level `.DS_Store` is refused, not ignored: WordPress's `dirlist` counts hidden files, so `install_package` would see two items. `enclosed_name()` alone accepts absolute paths in zip 8.6, so apvm applies its own path rules (§6.10) |
| Codes | New `inventory_io` (the inventory file, its lock or its directory cannot be read, written, locked or created, or is not a regular file) (§4.5) |

### Revision 8 (adversarial audit of P1.1, 2026-10-12)

| Topic | Change |
|---|---|
| Round trip | `run_as` users named `auto` or `login`, and an `owner` user named `auto`, are refused. They were saved as the keywords, so a reload changed who WP-CLI runs as |
| `wp_cli` | Restricted by shape, not just characters: one program (a command name or an absolute path), or a PHP interpreter (`php`, `php8.3`, `ea-php81`, …) with its `-n`/`-q` options and an absolute WP-CLI script; at most 4 tokens. Before, `["sudo", …]`, `["env", "-u", …]` or `["wp", "eval-file", …]` could wrap or replace WP-CLI (verified: a root `chown` through the §6.2 sudoers) |
| SSH local paths | `identity_file`, `known_hosts_file`, `ssh_config_file` and `binary` hold only `[A-Za-z0-9._/\:+-]`, `~` only as a leading `~/`, no leading `-`, and no empty, `.` or `..` component (so `~/../../dev/null` cannot pass for a file). Why: ssh expands `%` tokens and `${VAR}` in key and known_hosts paths, and splits known_hosts paths on whitespace. For a ProxyJump it also pastes the ssh program and the `-F` path unquoted into a command run by the user's shell (verified: `-F 'c$(touch x)'` ran `touch`). `known_hosts_file` may not be `none`, `NUL` or `/dev/…`, which would let any host key pass (T6) |
| `proxy_jump` | `none` (any case) is refused: it turns off a jump host set in ssh_config. At most 8 hops. A bracketed IPv6 address is allowed in the last hop only (ssh passes the others through `$SHELL -c`, where zsh fails with "no matches found"; verified), and it must parse as IPv6 |
| Users and root | Names made only of digits, and `.` / `..`, are refused (tools read `0` as root). `root` is matched in any letter case, and in the `owner` group too |
| Text | Every Unicode Cf, Zl and Zp character (zero-width, tag, line-separator and bidi characters) is refused, and escaped in messages, along with control characters. Remote paths refuse whitespace other than spaces, and components that start or end with a space. Messages quote values with `'` and `\` escaped, so a value cannot pass for message text |
| `url` | User info is refused without being echoed (the old message showed the password). IPv6 hosts must parse as IPv6. DNS labels are 1–63 characters, with no leading or trailing `-`. A host of digits and dots must be an IPv4 address. No `.` or `..` path segment |
| Parsing | `null` also reads as unset for `defaults`, `ssh` and the lists. `ownership` is read from a string only. `version` is read from a JSON object only, so a positional `[2]` is invalid, not "newer". An unsupported newer document keeps its problems (`Error::problems`). A generic walk finds records written as arrays |
| Hints | A project has variants on every slug or on none |
| Later phases | The audit's obligations are noted in P3.1, P3.3, P3.4, P4.2, P6.3, P6.4 and P8.1 |

### Revision 7 (P1.1 implementation, 2026-10-11)

| Topic | Change |
|---|---|
| Hints | `PluginHints` hold every apvm project (≥ 1 slug), so `--project` also works for single-slug projects. Family traits (`conflicts_when_active`, `shared_data`) need ≥ 2 slugs. A slug with several variants needs a marker on each, and its markers must not overlap (same file, one needle inside the other) (§6.9) |
| Newer inventory | Read-only only when every field and value in it is known here. Otherwise it is refused with `inventory_unsupported`, never read with parts ignored: an ignored field could be a setting that restricts apvm (§7) |
| Strict parsing | Records written as JSON arrays are refused (serde reads them by position, which bypasses field names). `null` means unset. A leading UTF-8 BOM is ignored (§7) |
| Allowlists | Remote paths must be canonical (no empty, `.` or `..` component, no trailing `/`, not `/`), so one site cannot hide behind two spellings (same-WordPress warning, site lock). Control and bidirectional-formatting characters are refused in paths and free text, and escaped in messages. The first `wp_cli` token must not start with `-` (an `env` option). `root` as `run_as` or `owner` needs `allow_root`. `owner: auto:<group>` is refused. Timeouts, names and notes have ranges. `proxy_jump` IPv6 hosts need brackets (§7) |
| Codes | New `inventory_unsupported` and `inventory_read_only` (§4.5) |
| Modules | `allowlist` (value rules; split into `allowlist/{mod, net, paths}.rs` in revision 8) and `problem.rs` (located problems) join the crate (§8) |

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
| D1 | Placement | New crate `apvm-sites`; its only apvm dependency is the leaf `apvm-process` (rev 15). `apvm-core` depends on it for plugin hints and build→install. | Zip installs and `wp run` need no GitHub/build stack; core stays the glue library (CLAUDE.md); one dependency direction | All in core; CLI-only |
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
| wp-rocket | — | `wp-rocket` | `wp-rocket.php` | `WP Rocket` |
| imagify | — | `imagify` | `imagify.php` | `Imagify` |

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
| sudo-rs (P4.2 audit, 0.2.13 on Ubuntu 26.04; not in the harness) | Under `-n`: `sudo: interactive authentication is required` for a rule that needs a password (and the listing of a user whose rules all do); `sudo: I'm sorry <user>. I'm afraid I can't do that` for a command no rule allows, also for a user without rules; `-l` of a user without rules: `sudo: Sorry, user <user> may not run sudo on <host>.`; listing a rule that needs a password: the command path, exit 0; no `requiretty` setting. Ubuntu 25.10 prints `sudo-rs:` instead of `sudo:` | §6.2: phrases matched without the prefix; a user without rules is `sudo_not_allowed` there |

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


**Verified in P3.3** (2026-10-13; clients OpenSSH 10.3p1 on macOS, 9.6p1 on Ubuntu 24.04 and 10.0p2 on Debian 13, the last two in containers on the harness network; the audit added 7.6p1 on Ubuntu 18.04 and 8.2p1 on Ubuntu 20.04, the second audit 8.7p1 (Fedora 35), 8.9p1 (Ubuntu 22.04), 9.2p1 (Debian 12) and 10.6p1 (Alpine edge), also in containers there, and the third audit re-ran 7.6p1, 8.2p1, 9.2p1, 9.6p1 and 10.0p2 there (the whole Rust E2E suite on 10.0p2, as a non-root user), the fourth 7.6p1, 8.2p1, 9.2p1 and 9.6p1, and 10.5p1 on openSUSE Tumbleweed (the whole Rust E2E suite, as a non-root user), the fifth 7.6p1, 8.2p1, 9.6p1, 10.0p2 (glibc and musl) and 10.3p1 against a lab sshd and fake servers on the harness network, and `ssh -G` on ten platforms, the sixth 7.6p1, 8.2p1, 8.9p1, 9.2p1, 9.6p1, 10.0p2 (glibc and musl) and 10.6p1 the same way, and 10.3p1; OpenSSH sources at each release tag for what a version accepts) [ssh.jump-host-options, ssh.jump-host-shell-startup, ssh.jump-host-isolation, ssh.known-hosts-persistence, ssh.generated-config-rules, ssh.agent-isolation, ssh.system-config-include; `crates/sites/tests/ssh_e2e.rs`]

| Item | Result | Plan impact |
|---|---|---|
| What a jump host gets | `exec ssh -l <user> [-p <port>] -F <file> [-v] -W '[%h]:%p' <hop>` (from `ssh -v`); no `-o`, `-i` or other option of the main connection | P3.3: every option in a generated `-F` file |
| `-o` options and a hop | With `BatchMode`, `StrictHostKeyChecking`, `UserKnownHostsFile` and `-i` on the command line only, the hop used `~/.ssh/known_hosts` and `~/.ssh/id_ed25519`, and on a terminal asked `Are you sure you want to continue connecting`; with `accept-new` it still refused the unknown key | — |
| A generated `-F` and a hop | The hop read only that file: `No ED25519 host key is known for <hop> and you have requested strict checking.` with a usable `~/.ssh/known_hosts`, no prompt on a terminal, and `accept-new` recorded the hop's and the destination's key in the file's known_hosts; `~/.ssh/known_hosts` stayed untouched. A relative `-F` reaches the hop, which runs in ssh's directory | — |
| Precedence | Options at the top of a `-F` file beat an included `Host *` and `Match all` (first value wins); `-l`/`-p`/`-J` on the command line beat ssh_config; `-F` skips the system `ssh_config` | Include it |
| `Include` | A missing file is skipped silently; a relative path is read from `~/.ssh`; an included file writable by group or others is refused (`Bad owner or permissions`), while `ssh -F` reads the same file | Check `ssh_config_file` first |
| `~` and `HOME` | ssh expands `~` (and its default files) from the password database: `HOME=/x ssh -G` still names the real home | apvm writes absolute paths |
| Quoting | Double-quoted paths keep spaces, `#`, `'` and `=` (7.6p1–10.3p1); `%%` is a literal `%` in `IdentityFile` (7.6p1–10.3p1) and in `UserKnownHostsFile` from 8.4 only (7.6p1 and 8.2p1 look for a file named `…%%…`; a bare `%1` fails on 9.6p1+: `unknown key %1`); `${VAR}` is expanded in `IdentityFile` | §7 rules, P3.3 rendering (audit: `%` refused in known_hosts paths) |
| `-F none -G` | Prints the compiled-in defaults: `globalknownhostsfile /etc/ssh/ssh_known_hosts /etc/ssh/ssh_known_hosts2` on all nine; `sessiontype`, `stdinnull`, `forkafterauthentication` and `channeltimeout` lines only where the client has the option: none on 7.6p1 and 8.2p1 (a file setting `SessionType` fails there: `Bad configuration option: sessiontype`), the first three from 8.7p1 (8.7p1, 8.9p1, 9.2p1), all four from 9.6p1 (9.6p1, 10.0p2, 10.3p1, 10.6p1). `ChannelTimeout` is a 9.6 client option (9.2 added sshd's; 9.2p1's client refuses it). With those pins and an included `Host *` setting the opposite, `-G` shows apvm's values on each (audit 2) | Finds the system `ssh_config`; tells which options apvm can pin |
| An ssh_config's session options (audit, 10.3p1) | `SessionType none`: no command, ssh waits until killed. `StdinNull yes`: exit 0 and the script never arrives. `ForkAfterAuthentication yes`: exit 0 at once, the command runs on in a detached ssh. `ChannelTimeout session=2s`: a command silent for about 2 s is cut, exit 255 with an empty stderr (so after the nonce line, `ssh_connection_lost`). Top-level `SessionType default`, `StdinNull no`, `ForkAfterAuthentication no`, `ChannelTimeout none` win, also through a jump host (whose `-W` keeps its own session type); `ControlPersist yes` with apvm's `ControlMaster no` changes nothing | Pinned when known |
| The generated file's directory (audit) | ssh, and a hop's `exec ssh …` (when ssh was started by a bare name), look programs up through `PATH` from it: with `PATH=:/usr/bin:/bin`, an `ssh` placed in that directory ran instead of `/usr/bin/ssh`, from apvm's own spawn | A private `0700` directory per call |
| `IdentityFile`, `IdentitiesOnly` | `IdentityFile` adds up across files: `ssh -G` lists apvm's key first, then an ssh_config's; `BatchMode yes` never runs `SSH_ASKPASS`, even with `SSH_ASKPASS_REQUIRE=force` (a passphrase key just fails) | Docs say "first", not "only" |
| OpenSSH 7.6p1 and 8.2p1 | The generated file works unchanged without the 8.7+ options: quoting, `RemoteCommand none`, `Tunnel no`; a hop gets `exec ssh -l deploy -F apvm-ssh-a.conf -v -W '[localhost]:22' <hop>` and refuses an unknown hop key with the strict-checking message, so it read the file. Hop failures end with `ssh_exchange_identification: Connection closed by remote host` (7.6p1) or `kex_exchange_identification: Connection closed by remote host` (8.2p1) instead of `Connection closed by UNKNOWN port 65535`. Not an SSH server: `ssh_exchange_identification: read: Connection reset by peer` (7.6p1), `kex_exchange_identification: banner line contains invalid characters` / `…: Connection closed by remote host` (8.2p1) | The plan's 7.6 minimum holds; classifier phrases |
| `accept-new` without persistence | known_hosts → `/dev/null`: `Warning: Permanently added …`, the command runs, nothing is kept. A file in a missing directory: `Failed to add the host to the list of known hosts (…)`, the command runs | Refused before ssh runs |
| More stderr samples (exit 255) | `Received disconnect from <ip> port 22:2: Too many authentication failures` (7 keys offered); `ssh: connect to host 2001:db8::1 port 22: Network is unreachable`; something that is no SSH server: `Connection closed by 127.0.0.1 port 8722`; behind a hop: `channel 0: open failed: connect failed: Connection refused` / `…: Name or service not known`, then `stdio forwarding failed`; every hop failure ends with `Connection closed by UNKNOWN port 65535`; a lost session: `Timeout, server target not responding.` (ServerAlive), `Connection to target closed by remote host.`. ssh ends its own lines with CR LF | Classifier table |
| Session pins at run time (audit 2) | The Rust E2E suite passes with every client above; its hostile ssh_config test runs from 8.7p1 (the three 8.7 options; ChannelTimeout too from 9.6p1) and skips before. Control on 8.7p1 without the pins: `StdinNull yes` → exit 0, the script never arrived; `SessionType none` → hangs | — |
| `AddKeysToAgent` (audit 2) | An ssh_config's `AddKeysToAgent yes` added the inventory's key to a running agent (10.3p1, `ssh-add -l`); a top-level `AddKeysToAgent no` before it kept the agent empty. `ask`/`confirm` call `ask_permission`, which uses ssh-askpass whatever `BatchMode` says (source, 7.6–10.6). Accepted from 7.2, first value wins (`-G` on 8.2p1–10.6p1; 7.6p1 does not print it) | Pinned |
| Shell startup and a relative `-F` (audit 2) | ssh runs a hop's `exec ssh … -F <value>` through `$SHELL -c`, which reads its startup file first (bash's `BASH_ENV`; zsh's `.zshenv` and fish's `config.fish`, read by every shell): one that runs `cd /` made `-F apvm-ssh.conf` fail (`Can't open user config file apvm-ssh.conf`), an absolute `-F` worked (macOS 10.3p1, zsh and bash) | Absolute path when shell-literal |
| ssh-askpass from a ProxyCommand (audit 2) | A user's `ProxyCommand ssh …` (no `BatchMode`) asked to confirm an unknown hop key through `SSH_ASKPASS` when it had no terminal and `DISPLAY` was set; with `SSH_ASKPASS_REQUIRE=never` inherited it did not (10.0p2). With a terminal it asks there: ssh shares apvm's session, and `apvm-process` keeps it so Ctrl+C reaches the tree | `SSH_ASKPASS_REQUIRE=never`; the terminal case is a known limit |
| Post-quantum warning (audit 2) | 10.1+ prints `** WARNING: connection is not using a post-quantum key exchange algorithm.` and two more `**` lines (CR LF) on every connection to a server without a PQ kex (10.3p1 → an 8.2p1 sshd), unless `KexAlgorithms` is set; exit 0 otherwise unchanged | Noise |
| A remote shell killed by a signal (audit 2) | `kill -9 $$` in the remote command: ssh exits 255 with an empty stderr (only the PQ warning, if any), like a lost connection; a killed child of the shell is the shell's status (137) | After the nonce: `ssh_connection_lost` (P6.2 re-reads) |
| `Include` state (audit 2) | A `Host`/`Match` block left open at the end of the included user config does not apply to the next `Include` (the system file's options still applied; 7.6p1, 8.2p1, 10.0p2, 10.3p1); a relative `Include` in an included file is read from `~/.ssh` | — |
| Older clients' notices (audit 2) | Before 8.5, `CheckHostIP yes` (default) makes `accept-new` print `Permanently added 'target,172.19.0.3' (ECDSA)` and record `target,172.19.0.3`; strict refusals name the client's preferred key type (ECDSA on 7.6p1 and 8.2p1, ED25519 from 8.5) | `host_key_notices` keeps ssh's naming |
| `~/.ssh` missing with `accept-new` (audit 2) | From 8.4 ssh creates `~/.ssh` (of the password database's home only) before adding a key; before, and for any other directory, it fails to add the key and runs the command (source) | Refused, with the directory and the fix named |
| Key file permissions (audit 2) | A key file others can read is ignored (`WARNING: UNPROTECTED PRIVATE KEY FILE!`) and the call fails as `Permission denied (publickey…)` → `ssh_auth_failed`; an agent holding the same key still logs in, so apvm does not refuse such a file | Audit 3: the classifier keeps the `Permissions 0644 for '…' are too open.` line (same text on 7.6p1, 8.2p1 and 10.3p1) |
| The system ssh_config, included (audit 3) | Plain ssh reads the system file without the owner/mode check it applies to `~/.ssh/config` and to every `Include`d file: owned by another user, or root's `0664`, ssh alone reads it, an `Include` of it fails with `Bad owner or permissions on /etc/ssh/ssh_config`; a default file this user cannot read (system `0600`, user `000`) is skipped by ssh alone and fails an `Include` (`Can't open user config file …: Permission denied`). Another owner and an unreadable system file on 7.6p1, 8.2p1 and 10.0p2; the rest on 10.0p2 [ssh.system-config-include] | apvm leaves unreadable default files out, and refuses a system file ssh would refuse to include |
| Older clients and `CheckHostIP` (audit 3) | 7.6p1 and 8.2p1, strict checking, host key known by name only: `Warning: Permanently added the ECDSA host key for IP address '172.19.0.3' to the list of known hosts.`, and the address is written to the known_hosts file (no new trust: the key was known) | Noise; not a `host_key_notices` entry |
| Dropping the terminal (audit 3) | A child that drops its controlling terminal with `TIOCNOTTY` (not a session leader) and its children cannot open `/dev/tty` (`ENXIO`) on Linux and macOS; Ctrl+C on the terminal still reaches it on Linux (the whole foreground process group gets `SIGINT`), not on macOS (BSD signals only the group's processes that still have the terminal) (Python on a pty, Darwin 25, Debian 13) | ProxyCommand prompts stay a known limit |
| openSUSE's system ssh_config (audit 4) | Tumbleweed (10.5p1): `/etc/ssh` holds only `ssh_config.d`, the system file is `/usr/etc/ssh/ssh_config` (it includes `/etc/ssh/ssh_config.d/*.conf` and `/usr/etc/ssh/ssh_config.d/*.conf`); ssh reads it when `/etc/ssh/ssh_config` does not exist or is a dangling link, not when that one exists, even mode `000` (then it reads neither); `-F none -G` still names `/etc/ssh/ssh_known_hosts`; a `-F` file's `Include` of the `/usr/etc` file works | The vendor copy under `/usr` is included when the system file is missing |
| A key that differs from the address's (audit 4) | With `CheckHostIP yes` (default before 8.5): `Warning: the ECDSA host key for 'target' differs from the key for the IP address '172.19.0.3'` (LF), `Offending key for IP in …:5`, `Matching host key in …:1`, `Exiting, you have requested strict checking.`, `Host key verification failed.`, exit 255, strict and accept-new alike (7.6p1, 8.2p1; 9.6p1 with `CheckHostIP yes`) | `ssh_host_key_changed` |
| A revoked host key (audit 4) | `@revoked` in known_hosts: the `REVOKED HOST KEY DETECTED` banner, then `ED25519 host key for [127.0.0.1]:2722 was revoked and you have requested strict checking.` and `Host key verification failed.`, exit 255, strict and accept-new alike (10.3p1; ECDSA on 7.6p1 and 8.2p1); `RevokedHostKeys`: `Host key ED25519 SHA256:… revoked by file …` (10.3p1) | `ssh_host_key_changed` |
| `readconf.c` (audit 4, sources 7.6p1, 8.2p1, 9.6p1, 10.2p1) | Every `Include`d file gets `SSHCONF_CHECKPERM`: `fstat` of the opened file, refused when `(st_uid != 0 && st_uid != getuid()) \|\| (st_mode & 022)`; no parent-directory check; `glob(path, GLOB_TILDE)`, no match is silent, a matched file that cannot be opened (`errno != ENOENT`) fails; an included file is read as a user file (`SSHCONF_USERCONF`: a relative `Include` from `~/.ssh`, a `~` path allowed, and its `IdentityFile`/`CertificateFile` count as user-provided, so a missing one is reported at `INFO`, not `DEBUG3`) | Matches `files.rs` |
| The generated file against plain ssh (audit 5) | As a non-root user with a `Host` alias in `~/.ssh/config` and one in a root `0644` system drop-in, `ssh -G` and `ssh -F <generated> -G` agree on Debian 13 (10.0p2), Ubuntu 24.04 (9.6p1), Fedora (10.2p1), AlmaLinux 8 (8.0p1) and 9 (9.9p1), Alpine 3.22 (10.0p2), Arch (10.6p1), openSUSE Tumbleweed (10.5p1) and macOS (10.3p1) but for the pinned options and `updatehostkeys` (below); both aliases resolve; the system file is `SSHDIR/ssh_config` next to `SSHDIR/ssh_known_hosts` in every binary (nixpkgs 10.5p1 too; FreeBSD by source), openSUSE's binary alone holds a vendor path (`/usr/etc/ssh/ssh_config`); every shipped `Include` is absolute and every included file (drop-ins, Fedora/AlmaLinux `Match final all` crypto-policies link, Arch's systemd drop-in link, macOS `crypto.conf`) is root's and passes the include check | Matches; vendor fallback limited to `/etc/ssh/ssh_config` |
| `UpdateHostKeys` (audit 5) | `readconf.c` (8.5+): on by default unless `VerifyHostKeyDNS` is, or a `UserKnownHostsFile` other than `~/.ssh/known_hosts` alone is named. `-F none -G` and plain `ssh -G`: `updatehostkeys true` (every 8.5+ client; 8.0p1 and 8.2p1 `false`); with the generated file `false`. Against the harness with one of its three host keys known: plain-ssh-like options learned the other two (`UpdateHostKeys yes`, 10.3p1, a `true` command), apvm's file without the line learned none | The file ends with `UpdateHostKeys yes` for ssh's own known_hosts files |
| Debian's include rule (audit 5) | Ubuntu 9.6p1 and Debian 10.0p2 (`user-group-modes` patch, `secure_permissions`): an included file of the user's that its private group may write (`0664`) is read, `0666`/`0646` refused, and `0664` too once the group has another member; Fedora, AlmaLinux 8/9, Alpine, Arch, openSUSE refuse `0664` (upstream) [ssh.system-config-include] | apvm keeps upstream's rule; message and docs say so |
| `-G` and the terminal (audit 5) | `ssh -F none -G <host>` with stdin not a terminal prints `Pseudo-terminal will not be allocated because stdin is not a terminal.` on stderr first (8.0p1, 9.6p1, 9.9p1, 10.0p2, 10.2p1, 10.3p1, 10.5p1, 10.6p1; `ssh.c` logs it before `-G` dumps, 7.6p1–10.6p1); with `-T` stderr is empty | `-T`; the line is noise |
| Host certificates (audit 5) | An `@cert-authority` line and a certificate for another principal, or an expired one: `Certificate invalid: name is not a listed principal` / `Certificate invalid: expired` (7.6p1 first prints `key_cert_check_authority: invalid certificate`), then ssh retries the plain key: strict, `No ED25519 host key is known for … and you have requested strict checking.`, `Host key verification failed.`, exit 255; `accept-new`, the plain key is recorded (`Warning: Permanently added …`) and the call runs, exit 0 (7.6p1, 8.2p1, 9.6p1, 10.0p2 glibc and musl, 10.3p1) | `ssh_failed`; `refused_certificate` notice |
| Login banners (audit 6) | A server's `Banner` is printed as sent (`\n` lines) among ssh's own `\r\n` lines, before the outcome of authentication: then `deploy@target: Permission denied (publickey).`, `Connection closed by 172.19.0.3 port 2205` (`LoginGraceTime`), `Connection to target closed by remote host.` (the session's sshd killed; 8.2p1: `client_loop: send disconnect: Broken pipe`), or nothing of ssh's after a remote shell that printed and exited 255 (7.6p1, 8.2p1, 9.6p1, 10.0p2 glibc and musl, 10.6p1, against sshd 10.0p2) | The cause is ssh's last `\r\n` line |
| `-T` with `-G` (audit 6) | `ssh -T -F none -G` exits 0 with an empty stderr and the same output but `requesttty false` (else `auto`) on 7.6p1, 8.2p1, 8.9p1, 9.2p1, 9.6p1, 10.0p2 (glibc, musl), 10.3p1 and 10.6p1; without `-T` every one, 7.6p1 included, prints the pseudo-terminal line | — |
| `UpdateHostKeys` per version (audit 6) | `-F none -G`: `updatehostkeys false` on 7.6p1 and 8.2p1, `true` on 8.9p1, 9.2p1, 9.6p1, 10.0p2 (glibc, musl), 10.3p1, 10.6p1. 10.3p1, `-G` only: a user file with `Match final` + `UpdateHostKeys no`: plain `false`, generated file `true`; with `VerifyHostKeyDNS yes`: plain `false`, generated `true`; `Host *` + `UpdateHostKeys no`: both `false`; a `Match final` block around apvm's line forces a final pass, after which `Host <address>` matches an alias's `Hostname` (`user admin` instead of the login name) | Kept, documented |
| Every class recaptured (audit 6) | 33 failure scenarios through an apvm-like file on 7.6p1, 8.2p1, 8.9p1, 9.2p1, 9.6p1, 10.0p2 (glibc, musl) and 10.6p1 (direct and jump-host refusals, unknown, changed, revoked and address-mismatched keys, certificates, an unreadable `RevokedHostKeys`, auth failures with their explanations, connect, DNS and banner failures, SSH-1 and garbage banners, a reset, a silent server, `DisableForwarding`, `MaxSessions 0`, a bad option, a ProxyCommand's ssh): the samples' wording byte for byte, classes as the table. A ProxyCommand ssh with its own `StrictHostKeyChecking yes` prints `No … host key is known …` and `Host key verification failed.` as a ProxyJump hop does; asking nobody, the refusal alone. On 7.6p1 a hop's slow channel can read as `Connection timed out during banner exchange` (the 3 s `ConnectTimeout` bounds the banner wait) | `ssh_host_key_unknown` for the ProxyCommand case (P4.3 note) |

**Classification table** (audit 5): every message the client prints at `INFO` or above before it exits 255, from the sources of 7.6p1, 8.2p1, 8.9p1, 9.6p1 and 10.6p1 (`sshconnect.c`, `sshconnect2.c`, `ssh.c`, `readconf.c`, `clientloop.c`, `packet.c`, `kex.c`, `channels.c`, `sshkey.c`), and the system's own messages inside them. **The cause is ssh's last message** (audit 6): the last line ending with `\r\n` (how ssh logs every message; text it relays from the server, a banner or a remote startup file's stderr, ends with `\n`), lines that only sum up the one before skipped; the host key rows are read from the lines before ssh's closing `Host key verification failed.` (that whole line, the last), top to bottom; a jump host's `stdio forwarding failed` from the channel line before it; the other rows from the last message itself, top to bottom (`remote/classify.rs`; the bare `Host key verification failed.` row and the "anything else" row are the fallback), a phrase anywhere in a line unless marked "line start". "Sampled" means captured byte for byte from those clients (the classifier tests hold them), else source only. The remedy column is for P4.3.

| Message (phrase) | Versions, wording | Code | Remedy (P4.3) |
|---|---|---|---|
| `Host key for X has changed and you have requested strict checking`, the `REMOTE HOST IDENTIFICATION HAS CHANGED` banner (also after `POSSIBLE DNS SPOOFING`, or an SSHFP mismatch) | all; sampled 7.6p1–10.3p1 | `ssh_host_key_changed` | `ssh-keygen -R <name> -f <known_hosts>` once the new key is confirmed |
| `Warning: the … host key for 'X' differs from the key for the IP address 'A'` (`CheckHostIP`) | all; sampled 7.6p1, 8.2p1, 9.6p1 | `ssh_host_key_changed` | `ssh-keygen -R <A>` (the address) once confirmed |
| `… was revoked and you have requested strict checking`, `Host key … revoked by file …` | all; sampled 7.6p1, 8.2p1, 10.3p1 | `ssh_host_key_changed` | investigate; never `ssh-keygen -R` (it drops the revocation) |
| `Certificate invalid: …` (`name is not a listed principal`, `expired`; also `not yet valid`, `not a host certificate`); `Certificate lacks principal list`, `… contains unsupported critical option(s)`, `Certificate signed with disallowed algorithm` (source only) | all; sampled 7.6p1–10.3p1 | `ssh_failed` | fix the server's host certificate; never `--trust-new-host-key` (it trusts the plain key, bypassing the CA) |
| `Error checking host key … in revoked keys file …: <reason>`; `host key … not permitted by HostkeyAlgorithms` (source only) | all; sampled 7.6p1–10.3p1 | `ssh_failed` | fix the ssh_config (`RevokedHostKeys`, `HostKeyAlgorithms`) |
| `No <type> host key is known for X and you have requested strict checking` (the server, or a ProxyJump hop) | all; sampled 7.6p1–10.3p1 | `ssh_host_key_unknown` | `--trust-new-host-key` after checking the fingerprint |
| `Host key verification failed.` alone (a ProxyCommand's own ssh) | all; sampled 7.6p1–10.3p1 | `ssh_failed` | that ProxyCommand's own known_hosts (apvm's options do not reach it) |
| `<user>@<host>: Permission denied (<methods>).`, `Received disconnect from … : Too many authentication failures` | all; sampled | `ssh_auth_failed` | user, key, `authorized_keys`; kept line `Permissions 0644 for '…' are too open.`: `chmod 600` the key; `Load key "…": invalid format` / `…: error in libcrypto` (9.6p1+ on Linux): convert or replace the key |
| `timed out`: `ssh: connect to host … : Connection timed out` / `Operation timed out` (macOS, musl), `Connection timed out during banner exchange`, then `Connection to … port … timed out` (9.0+; 7.6p1 reports a silent banner as a connect timeout), a hop's `open failed: connect failed: Connection timed out` | sampled 7.6p1–10.3p1 | `ssh_timeout` | network, firewall, `connect_timeout_secs` |
| `ssh: connect to host … : <errno>` (`Connection refused`, `No route to host` / musl `Host is unreachable`, `Network is unreachable` / musl `Network unreachable`, macOS `Address family not supported by protocol family`; `Host is down`, `Permission denied`) | sampled 7.6p1–10.3p1 (glibc, musl, macOS) | `ssh_unreachable` | host, port, network |
| `ssh: Could not resolve hostname X: …` (`Name or service not known`, `nodename nor servname provided, or not known`, `Temporary failure in name resolution`, musl `Name does not resolve` / `Try again`) | sampled | `ssh_unreachable` | the host name, DNS |
| A hop: `channel 0: open failed: <reason>` then `stdio forwarding failed` (line start) (`connect failed: Connection refused` / `Name or service not known`, `administratively prohibited: open failed`) | sampled 7.6p1–10.6p1 | `ssh_unreachable`, or `ssh_timeout` when the channel line says `timed out` (kept: the channel line) | the hop's onward access (`AllowTcpForwarding`, `DisableForwarding`, the destination) |
| Before or instead of a banner: `kex_exchange_identification: …` (8.0–9.4; 9.5+ only at `VERBOSE`), `ssh_exchange_identification: …` (before 8.0), `Protocol major versions differ: 2 vs. 1`, `Bad remote protocol version identification: '…'`, `banner exchange: Connection to … : …` (9.0+, line start), `Connection closed by <ip> port <n>`, `Connection reset by <ip> port <n>` (line start) | sampled 7.6p1–10.3p1 | `ssh_unreachable` | not an SSH-2 server, or one refusing connections (`MaxStartups`, a firewall, 9.8+'s `PerSourcePenalties`: wait) |
| After a session was up: `Connection to X closed by remote host.`, `Timeout, server X not responding.`, `client_loop: …`, `Broken pipe`, `Connection reset by peer`; any exit 255 once the nonce line is out | sampled (the first two) | `ssh_connection_lost` | after a mutation, re-read (P6.2) |
| Anything else, keeping the last line that is more than a summary: `… line N: Bad configuration option: …` (before `…: terminating, N bad configuration options`), `Bad owner or permissions on …`, `Can't open user config file …`, `Unable to negotiate with … : no matching …`, `Received disconnect from … : <reason>` (before `Disconnected from …`), `channel 0: open failed: …` without a hop (`MaxSessions 0`), `RefuseConnection` (10.0+), an empty stderr (a remote shell killed by a signal) | sampled where named | `ssh_failed` | show the line |
| Noise: `Warning: Permanently added …`, `Pseudo-terminal will not be allocated …`, `…: warning: setlocale: …`, `Connection closed by UNKNOWN port 65535`, a last `…_exchange_identification: Connection closed by remote host` after other lines (7.6p1–9.4p1, after a hop or ProxyCommand), the post-quantum warnings (10.1+), `… line N: Unsupported option "…"` (every call, 7.6p1–10.6p1), `no such identity: …` (line start) | sampled | — | — |

ssh's `Host key verification failed.` ends every host key refusal (`fatal()`, so nothing of ssh's follows it but noise). A server's login banner or a remote startup file's stderr decides nothing: only ssh's own `\r\n` lines do (audit 6; a server that writes `\r\n` itself can still pass for ssh, as it controls what it sends once its host key passed). Without any line of ssh's (a remote shell that printed and exited 255 before the nonce line) the call is `ssh_failed`, its last line shown as text.

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

`--site a,b` · `--group qa` · `--server eu-1` · `--all-sites`. Flags are repeatable (union), kept in inventory order and de-duplicated. Sites with the same `path` on the same ssh destination (`host`, `port`, `user`, `proxy_jump`, `ssh_config_file`, compared exactly as apvm passes them to ssh; unset ≠ a value) → warning (revision 10); P6.4's probe-time realpath key covers the rest. An empty selection, or one that matches no site, is an error, never "everything"; a selected group or server that adds no site → warning. Settings resolve with their source: SSH field by field (server → `defaults.ssh` → built-in: connect timeout 15 s, multiplex on, program `ssh`; otherwise left to ssh_config), site settings from the site, else built-in (§7).

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
| SSH | `ssh_missing` (also: a program that cannot report OpenSSH's `-G` defaults), `ssh_auth_failed`, `ssh_host_key_unknown`, `ssh_host_key_changed`, `ssh_unreachable`, `ssh_timeout`, `ssh_connection_lost` (rev 16: exit 255 after the script started; the command may have run in part), `ssh_failed` (rev 16: exit 255 for a reason apvm does not recognize, or a host key refused for a reason neither trusting new keys nor removing the known one fixes: an invalid host certificate, an unreadable `RevokedHostKeys` file, a ProxyCommand's own ssh; the kept line says which) |
| sudo / identity | `sudo_missing`, `sudo_password_required`, `sudo_not_allowed`, `sudo_requiretty`, `owner_is_root`, `plugins_dir_mismatch`, `probe_failed` (P4.2: a probe step printed what apvm cannot read with certainty, or `runuser` failed) |
| WordPress | `wp_cli_missing`, `not_wordpress`, `wp_not_installed`, `plugins_dir_not_writable`, `multisite_unsupported` |
| Execution | `site_locked`, `insufficient_space`, `upload_corrupted`, `wp_command_failed`, `verification_failed`, `state_drifted`, `outcome_unknown`, `health_check_failed`, `timeout`, `file_mods_disallowed` |
| Ownership / variants | `ownership_unfixable`, `variant_conflict` (two variants of one slug, or two siblings activated together without `--keep-siblings`) |
| Input | `package_invalid`, `refused_command`, `confirmation_required`, `invalid_input`, `inventory_invalid`, `inventory_unsupported` (a newer inventory this build cannot read), `inventory_read_only` (a change to a newer inventory), `inventory_io` (the inventory file, its lock or its directory cannot be read, written, locked or created, or is not a regular file), `inventory_locked` (another process held the inventory lock for the whole 10 s wait), `inventory_changed` (the file changed since it was loaded; a guarded save wrote nothing; or a `server remove` whose server, sites or their groups changed during the confirmation), `internal_error` (a JSON document could not be built; not expected to happen, it replaces a panic) |
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

### 5.2 Inventory commands (no SSH, except `add`'s connection test and `test`)

**`apvm server` — SSH hosts**

| Command | Arguments | Notes |
|---|---|---|
| `server defaults` | `[--user <USER>] [--port <N>] [--key <PATH>] [--connect-timeout <SECS>] [--jump <[USER@]HOST[:PORT]>[,…]] [--known-hosts <PATH>] [--ssh-config <PATH>] [--ssh-binary <PATH>] [--multiplex \| --no-multiplex] [--unset <FIELD>]… [--json]` | No flags = show. `--key` is the **one main key** |
| `server add <ID>` | `--host <HOST> [--user <USER>] [--port <N>] [--key <PATH>] [--connect-timeout <SECS>] [--jump <[USER@]HOST[:PORT]>[,…]] [--known-hosts <PATH>] [--ssh-config <PATH>] [--multiplex \| --no-multiplex] [--name <TEXT>] [--notes <TEXT>] [--no-test] [--trust-new-host-key]` | Tests SSH by default; `--key` overrides the default key for this server; `<HOST>` may be an `~/.ssh/config` alias |
| `server edit <ID>` | the `add` flags (except `--no-test`) + `[--unset <FIELD>]…` | `<FIELD>` is the flag's name (comma lists allowed); the host cannot be unset; setting and unsetting one field is refused; an empty value is refused with `to clear it: --unset <field>` |
| `server list` | `[--json]` | |
| `server show <ID>` | `[--json]` | Shows the effective (merged) SSH settings |
| `server remove <ID>` | `[--cascade] [-y]` | Refused while sites use it, unless `--cascade`. Asks first; without a terminal `-y` is required (`confirmation_required`). If the server, its sites or their groups change while the prompt is open, nothing is removed (`inventory_changed`) |
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
| `group edit <ID>` | `[--add <SITE>]… [--remove <SITE>]… [--notes <TEXT>] [--unset notes]` | `--unset notes` added in P2.3 (an empty `--notes` is refused, as for `server`/`site`) |
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

1. **Typed scripts.** A `Script` is built from compile-time literals and quoted values (POSIX single quotes, `'` → `'\''`). Every command in it gets `</dev/null` so nothing consumes the script stream. Every script is one brace group `{ … }` whose first command is `LC_ALL=C; export LC_ALL`: the shell parses the whole group before running any of it, so a script cut short at its end (a dropped connection) is a syntax error that runs nothing, never a partial last command (P3.1 audit: a cut `… search-replace a b` lost its trailing `--dry-run` and ran). `LC_ALL=C` makes sudo and coreutils messages are the English ones the classifiers match (sudo keeps `LC_ALL` for the command, §3.5).
2. **Framing.**
   `{ LC_ALL=C; export LC_ALL⏎printf '\n%s\n' '<nonce>'⏎cd / && <s1> </dev/null [&& <s2> </dev/null …]; rc=$?; [ "$rc" -eq 255 ] && rc=254; exit "$rc"⏎}`
   - One private `frame()` writes the group, prelude, nonce and epilogue; later phases add typed statements (`remote/statement.rs`: a `'static` program plus quoted `Word`s, run as the identity, as root or as the login user), never hand-written framing. Statements are chained with `&&`, so `rc` is the first failing status. Upload scripts use the same frame on one line, `{ …; …; }`.
   - Only stdout after the line exactly equal to the per-call random nonce is parsed, which ignores `.bashrc`/MOTD noise. The newline printed before the nonce keeps noise without a final newline from gluing onto the nonce line (rev 15). No nonce line → `PayloadError::NonceMissing` (never "no output"); a call that did not end with an exit status (timeout, transport loss, signal) → `PayloadError::Interrupted`; capped stdout → `PayloadError::Truncated`. A script whose start a login shell ate may still have run: after a mutation, `NonceMissing` is treated like a transport loss (re-read).
   - 255→254 makes a real 255 always mean an SSH failure. 254 = remote exit 255: a PHP fatal before WordPress loads, or during activation. Once WordPress has loaded, its default fatal-error handler exits **1** (§3.5). Fatals are never detected by 254 alone, nor by WordPress's message, which a site can change.
   - `cd /` keeps a project `wp-cli.yml` out of play (only a root-owned `/wp-cli.yml` could still apply); `--path` and `--url` are always explicit.
3. **Delivery** (D3):
   - **Stdin mode** (everything except uploads): remote command `sh -s`; script bytes on stdin, then EOF. The login shell sees only `sh -s`.
   - **Upload mode:** stdin carries the zip, so the script is the remote command `sh -c '<script>'`.
     - A distinct `UploadScript` type accepts values matching `^[A-Za-z0-9._/+-]+$` only (paths and names). That excludes the `'` and backslash that fish reads as escapes inside single quotes. P0 showed every other printable ASCII character survives all five login shells, so the allowlist keeps a margin.
     - Its templates are single-line (csh/tcsh reject newlines in quotes) and contain neither `\\` nor `\'`.
     - With these rules the quoting survives sh, bash, dash, zsh, fish and tcsh login shells (P0).
4. **Identity prefix** (§6.2): empty, `sudo -n -u U -g G --`, `sudo -n -u U --`, or `runuser -u U -g G --` (root login without sudo). Every value is quoted, validated names included. No shell parses data under sudo or runuser; the one shell there is the probe's constant directory walk (§6.2 step H, P4.2 audit 2: `env -u BASH_ENV -u ENV -u CDPATH sh -c '<constant>' sh <params…>`, data only as positional parameters). It needs no new sudo grant: identity rules must be `NOPASSWD: ALL` already, since apvm runs `env` there, which runs any program. It runs as root only when the site's `allow_root` makes root the identity, never through the closed root set. Root-level commands run under `sudo -n -u root --` (a sudoers `runas_default` would otherwise change the user) and form a **closed enum**: `stat`, and `chown -R -P -h --from=+F:+FG -- +U:+G` for the opt-in root repair (§6.8). Ids always carry `+`: GNU chown, chgrp and find resolve a bare number as a user/group *name* first (a user named `1001` would win).
5. **Hermetic WP-CLI.** Every WP-CLI call is `<prefix> env -u <18 variables> WP_CLI_CONFIG_PATH=/dev/null PAGER=cat <wp_cli> --skip-packages --exec='umask(umask() & ~0<g>);' [--allow-root] --path=… --url=… <args>`:
   - `env -u` removes what makes WP-CLI 2.12 or PHP load code, run another program or behave differently when set for the identity (the 2.12.0 phar's `getenv` calls and `bin/wp`): `PHPRC`, `PHP_INI_SCAN_DIR`, `WP_CLI_{ALLOW_ROOT, CACHE_DIR, CUSTOM_SHELL, EARLY_REQUIRE, FORCE_USER_LOGIN, PACKAGES_DIR, PHP, PHP_ARGS, PHP_USED, REQUIRE, RUNTIME_ALIAS, SSH_BINARY, SSH_PRE_CMD, STRICT_ARGS_MODE}`, `WP_CONFIG_PATH`, `EDITOR`. sudo's `env_reset` already drops them; login and `runuser` identities did not. `PAGER=cat`: WP-CLI runs the pager for `help` even when not on a terminal. GNU, busybox and BSD `env` all take `-u`.
   - Arguments cannot set WP-CLI global parameters other than the §6.12 overrides: WP-CLI reads globals anywhere, even after the subcommand and after `--` (verified: `--require=` and `--exec=` ran code there). A `--url=…` override must be a valid site URL.
   - `--allow-root` is explicit, set by P4.2 exactly when the identity's uid is 0.
   - `WP_CLI_CONFIG_PATH=/dev/null` keeps the identity's `~/.wp-cli/config.yml` out. It could hold `require:` files or command defaults such as `plugin uninstall: deactivate: true`.
   - `--skip-packages` keeps installed WP-CLI packages out.
   - P0 showed both run code on every call, even with `--skip-plugins` (§3.5).
   - `--exec` relaxes, inside the WP-CLI process, only the group bits `P` grants (`<g>` = `P & 070`, constant digits). So WordPress keeps setgid and group bits at creation, and the identity's own "other" bits stay: a `0027` identity is never made world-readable.
   - It is needed because the default umask differs by login path: `0002` via Debian's `pam_umask`, `0022` under `runuser` (§3.5).
   - Staging needs neither: `mktemp -d` always creates `0700`, and the staged zip never reaches the site.
6. **Limits:** stdout 8 MiB, stderr 1 MiB (with a truncation flag), drained to the end so the child never blocks; a timeout or a dropped call kills the child's whole process tree (`apvm-process`), and every child is recorded in the caller's `ProcessScope`, so napi kills it at `process.exit()` (rev 15).
7. **Timeouts:** `ConnectTimeout` 15 s; `ServerAliveInterval=15`, `ServerAliveCountMax=4` (a shared connection, P3.4: 10 s × 3, so a dead master is given up after 30 to 40 s while a shorter blip, a Wi-Fi roam or a VPN reconnect, does not cut the long calls riding it; 5 s × 3 did, audit round 2); per command 300 s, install 900 s, `wp run --timeout`. A timed-out mutation → re-read → done/failed/unknown.
8. **Retries:** reads only, on transport failures only, twice (1 s, 3 s). Mutations never.
9. **Multiplexing (Unix, P3.4):** only for a server reached directly: `ssh -G` (the generated file and the destination) runs as for a call (remote command `sh -s`) and as for a master (`-N`), since OpenSSH 10's `Match command`/`Match sessiontype` can tell them apart (audit round 4, verified on 10.3p1); when either shows a `proxyjump` or `proxycommand`, multiplexing is off (`NoMultiplexing::Proxied`), and when their host, user, port or control path differ too (`SessionDependent`); the verdict is reused 30 s per configuration (`Match exec` runs twice per check) (Proxied: audit round 3: with `ControlPersist` ssh sends a proxy's stderr to `/dev/null`, so a jump host's unknown key, refused login or unknown name would read as nothing, and `accept-new` hop notices would be lost; verified on 10.3p1). A call's file says `ControlMaster no` and `ControlPath "<control_dir>/<tag>%C"` (`%` doubled, `${` refused): it rides a live master, else opens its own connection, and never becomes a master. Before a call (under its configuration's process-wide gate) apvm connects to the call's socket (its path as `ssh -G` expands it) and waits up to 3 s for the master's hello (audit round 5: `ssh -O check` first reads the whole ssh_config, so a slow `Match exec` or DNS lookup made a live master look stuck); with no master there, a starter `ssh -F <file> -N <destination>` with `ControlMaster auto` and `ControlPersist 15s` authenticates, forks the master (own session, `/dev/null` stdio) and exits at once, running nothing on the server (audit round 3: `true` kept a ForceCommand's side effects and the master's carrier in the starter's tree longer). So no call's process tree holds the shared connection (audit round 2). A starter exiting 255 is the call's failure (its stderr, classified); its stderr, even a timed-out one's, is put before the call's (host key notices). Calls with the same options share one gate in the process (any transport); those that waited while a start failed (255, no answer in time, a stuck master) take that result, without its host key notices, instead of trying again; a starter that lost the race to another process's master (ssh's `ControlSocket … already exists` line) is stopped on that line and the check repeated (audit round 4: it would wait as a plain `ssh -N` until killed); a starter's 255 that classifies as `ssh_failed` (a master that could not link its socket) lets the call run on its own connection; a start's time and the lock wait come before the call's own timeout. Keep-alives 10 s × 3 (item 7). After a call that timed out before its script started, `ssh -O stop`. A stopped or wedged master says no hello (a connection to its socket stays queued), and a call that meets it hangs before OpenSSH 9.6 (audit round 4, 8.9p1, 9.2p1): no hello in 3 s means a stuck master, whose socket is removed (its expanded path from `ssh -G`, only when it is this configuration's socket in the checked directory), and the call runs on its own connection with `ControlPath none`; ssh's own `muxclient: master hello exchange failed` line (ending `\r\n`) in a call's stderr (9.6+) removes it too; a stuck master that later resumes and exits removes its path, by then its successor's socket (one more master is started; apvm cannot identify the stuck master's process to end it first). `control_dir` created `0700` and used only when it is this user's, not a link, not group- or other-writable, every ancestor (links resolved) this user's or root's and not writable by others unless sticky (stricter than OpenSSH: a `0775` home turns it off), and holds a socket and a hard link to it (a test bind and link: the master links its socket into place). A caller may opt in to `/tmp/apvm-mux-<uid>` (sticky root `/tmp`, same checks) when its `control_dir` is too long. `<tag>`: 8 base32 characters of a SHA-256 of the ssh program, the generated file and the destination arguments, so only calls with identical options share a master. Off (never an error, `SshTransport::multiplexing()` says why) without a `control_dir` (the library has no default; the CLI's is `~/.apvm/ssh-mux`), on Windows, with `multiplex: false`, through a proxy, for an unusable directory, or when the expanded socket path exceeds 86 bytes (macOS) / 90 (Linux): `sun_path` minus the NUL and the master's `.<16 random>` bind suffix (one byte more fails the call, verified). Off, the file says `ControlMaster no` and `ControlPath none`, so no connection the user opened with other options is reused.
10. **Concurrency:** `--jobs` (4) globally plus a per-server limit (`ServerLimits`, 4 calls per inventory server, shared by every transport given the same `SshOptions`) below sshd's `MaxSessions 10`.

### 6.2 Probe and identity resolution (read-only)

| Step | Runs as | What (one round trip each) |
|---|---|---|
| H — host | login user | `id -u`, `id -g`, `id -un`, `id -gn`, `id -G`, `printenv SHELL`; `command -v` of sudo, runuser, sha256sum, shasum, openssl and `<wp_cli>` (one statement each); the **walk** of `<path>/wp-content/plugins` (or the pinned `plugins_dir`): one `sh` process `cd -P`s into it one component at a time, then prints `pwd -P` and `stat -c '%u⏎%g⏎%a⏎%F⏎%U⏎%G' .` (names last; `UNKNOWN` = no name), so path and ownership describe the same directory (P4.2 audits: a separate `realpath` and `stat` let the site swap a link between them). A non-link component must land at the expected physical path; a symbolic link is followed only when it and its directory belong to root or the login user and that directory is writable by neither group nor others (release layouts' `current -> releases/<n>`, `/var/www -> /srv/www`); anything else is `plugins_dir_mismatch`. A diagnostic `stat -L` says why a failed walk failed; BSD `stat -f` is retried only when `-c` is refused as an option. On EACCES root `sudo -n -u root -- stat` of the same path (describes a symlink rather than following it, so anything but `directory` is refused), then, immediately before step W, the identity walks the path itself and must find the owner and group root read; `id -u` of the users `run_as` and `owner` name → reference `(U, G, P)`. No generic `sudo -n -l`: the targeted listing runs only after a refusal |
| I — identity | — (pure) | the table below; then a probe of the exact command shape: `<prefix> env -u … WP_CLI_CONFIG_PATH=/dev/null PAGER=cat <wp_cli> --skip-packages --exec='umask(…);' --path=… cli version` (built with `WpCli`) |
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
| any | `G` is gid 0 (no `allow_root`) | `sudo -n -u U` without `-g` (P4.2 audit 2; a root login without sudo is blocked: `runuser` needs `-g`) | repaired after |
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

Recommended sudoers (documented). Identity rules must be `NOPASSWD: ALL` for the identity user: apvm runs `env` and the WP-CLI argv there (and the probe's constant directory walk, `env … sh -c '<constant>' sh <params…>`), never a shell that parses data. Root is optional:

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

`DISALLOW_FILE_MODS` (read in probe step W): WP-CLI ignores it (§3.5), so apvm honors the site's policy itself. Install, reinstall, uninstall, **reset** and delete are `blocked` with `file_mods_disallowed` unless `--allow-file-mods` is given (rev 15: `reset` deletes the plugin's translation files). Activate and deactivate change no files and proceed.

### 6.8 Ownership, group and group access

- **Reference** `R = (U, G, P)`: owner, group and mode of `WP_PLUGIN_DIR` (`owner: auto`), or an explicit `user[:group]` with `P` still taken from the plugins dir.
- **Expected** for every entry under the target `<plugins_dir>/<slug>` (or `<plugins_dir>/<file>` for single-file plugins):
  - owner U and group G;
  - dirs carry the group r/w/x bits `P` grants, plus setgid when `P` has it;
  - files carry the group r/w bits `P` grants.
  - Bits are only ever added; "other" bits are never touched; `o+w` entries are reported.
- **Check** (one script, as the identity, numeric ids, first 20 offenders listed):
  - `find T \( ! -uid <uid> -o ! -gid <gid> \) -print | head -n 20` (`-uid`/`-gid`: `-user 1001` matches a user *named* 1001 first)
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
| 1 | Only the group differs and the identity owns the files and is in G (or runs with egid G) | `chgrp -R -h -- +<gid> T` | none |
| 2 | Owner differs, and the site opted in with `ownership: mirror-root` | the root rung below | root (closed enum) |
| 3 | Group bits missing | `find T -type d -exec chmod g+<bits>[s] {} +` and `find T -type f -exec chmod g+<bits> {} +` (symlinks excluded by `-type`) | none (the identity owns T) |
| — | Still mismatched, or the owner differs on a site without `mirror-root` | item `failed` with `ownership_unfixable`: the entries, and the exact commands an administrator can run; the plugin stays installed | — |

- **Root rung** (opt-in; every precondition re-checked right before the command):
  - Absolute paths only. No `cd` as the login user, who may not reach FTP-style sites at all (§3.5).
  - `sudo -n -u root -- stat -c %F -- '<plugins_dir>/<slug>'` (root) must print `directory`; a symlinked slug prints `symbolic link` and is refused.
  - No multi-linked files: `find T ! -type d -links +1` must print nothing. Symlink flags do not cover hardlinks.
  - `/proc/sys/fs/protected_hardlinks` must be `1` (it is on current Debian and Ubuntu kernels, §3.5).
  - `sudo -n -u root -- chown -R -P -h --from=+<F>:+<FG> -- +<uid>:+<gid> '<plugins_dir>/<slug>'`, once per foreign owner/group pair (F, FG) the check found. F is never root, never the login user and never the target uid; such trees are reported instead. busybox `chown` has no `--from`, so it fails closed (`ownership_unfixable`).
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
| File | Regular file named `*.zip`, ≤ 256 MiB, read into memory once (hash and inspection see the same bytes; nothing is extracted) |
| Directory | One reading for libzip, PclZip and the zip crate (revision 10): exactly one end record in the last 64 KiB, its comment ending the file; a central directory ending at it with exactly the declared records; consistent ZIP64 records; no multi-disk archive; no renaming Unicode path field; no DOS folder attribute on a file; no duplicate names → else `Ambiguous` / `DuplicateEntry` |
| Bomb limits | ≤ 100 000 entries (from the end record, before entries are parsed); declared uncompressed total ≤ 1 GiB; every entry decompressed with its exact declared size and CRC-32; total reads ≤ 4× file size + 1 MiB |
| Compression | Stored or Deflated only; encrypted entries refused |
| Paths | apvm's own rules (zip 8.6's `enclosed_name()` accepts absolute paths): no `..`, `.` or empty component, no leading `/`, no NUL, no backslash; also checked under `__MACOSX/`; symlink entries warned |
| Structure | Exactly one top-level dir (ignoring `__MACOSX/` only: a top-level `.DS_Store` or any other file is refused, since WordPress counts hidden files); its name passes the slug allowlist |
| Plugin | A `<root>/*.php` with `Plugin Name:` in its first 8 KiB; main = `<root>/<root>.php` if headered, else the first headered (sorted); several → warning |
| Headers | WordPress 7.1.3 `get_file_data`/`get_plugin_data` semantics on bytes (short `<?` tags, `''`/`'0'` empty, `Network` falling back to `Site Wide Only`, `Requires Plugins` cleanup): Name, Version, Requires at least, Requires PHP, Network, Requires Plugins, Text Domain. Names are decoded as libzip does (UTF-8 guessed without the flag). Note: WordPress's own `check_package()` reads requirements from the first headered file in `glob` order, which can differ from the main file when several are headered (already a warning) |
| Variant | `PluginHints` markers searched while streaming each entry: exactly one match → that variant; a single-variant slug without marker → that variant; none → `Undetermined`; several → `Undetermined` + warning |
| Identity | SHA-256 + size |

### 6.11 Multisite (P10)

- **Activation:** network-only plugins (`Network: true`, from the package or a constant headers snippet) are activated with `--network`; `--network` on a single site is an input error.
- **Before destructive steps:** a constant snippet lists per-subsite activations (`get_sites()` × `active_plugins`, with URLs). Pass 1 also deactivates on each subsite (`--url=<subsite>`), verified by re-running the snippet.

### 6.12 `apvm wp run` — WP-CLI passthrough

| Aspect | Design |
|---|---|
| Execution | The arguments become `wp` argv elements, quoted, in a stdin-delivered script: no shell interpretation of `; \| & $() * ~`, any byte but NUL allowed. Runs as the site identity (owner:group), hermetic and with the umask relaxation (§6.1), with apvm's `--path` and the site's `--url` |
| Allowed overrides | `--url=…` (subsite targeting; must be a valid site URL), `--skip-plugins[=…]`, `--skip-themes[=…]`, `--user=…` (WP user), `--format=…` (not a global), `--debug[=…]`, `--quiet` |
| Refused params | Every WP-CLI global parameter except the allowed overrides. Notably: `--exec` and `--require` (they run arbitrary PHP and would bypass the classification); `--path` (apvm owns it); `--ssh` and `--http` (would hop elsewhere); `--allow-root` (identity policy); `--prompt` (interactive); `--skip-packages` and `--context` (apvm sets them); `--blog`, `--color`; the bare and `--no-…` forms of every global (`--no-skip-packages` would undo apvm's own); a leading `@alias`. WP-CLI matches names exactly (`--req=`, `--REQUIRE=` are not globals), anywhere in the arguments, after `--` too. Enforced by `WpCli::arg` (`allowlist::wp_cli_arg`) for every caller, not only `wp run` |
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
| `user`, `run_as`, `owner` parts | `[A-Za-z0-9._-]`, no leading `-`, ≤ 64, not all digits, not `.`/`..`; `root` (any case, user or group) needs `allow_root: true`; `owner` is `auto`, `user` or `user:group`; `auto` and `login` never name a user (`auto:<group>` refused) | ssh argv / remote |
| `port` | 1–65535 | ssh argv |
| `connect_timeout_secs` / `command_timeout_secs` | 1–3600 / 1–86400 | ssh argv / local |
| `proxy_jump` | ≤ 8 comma-separated `[user@]host[:port]` hops, ≤ 1024 bytes, not `none`; a bracketed IPv6 address only in the last hop | ssh argv / ssh's shell |
| `path`, `plugins_dir` | absolute and canonical (no empty, `.` or `..` component, no trailing `/`, not `/`); no backslash, control or invisible formatting character; no whitespace but spaces, and no component starting or ending with a space; ≤ 4096 bytes | remote |
| `remote_tmp_dir` | as `path`, and `^[A-Za-z0-9._/+-]+$` (the `UploadScript` allowlist, §6.1, so the inventory never accepts what an upload would refuse) | remote |
| `url` | `http(s)://host[:port][/path]`: lowercase scheme; a DNS name (labels 1–63 characters, no leading or trailing `-`), an IPv4 address or `[IPv6]`; path `[A-Za-z0-9._~/-]` and `%`-escapes, with no `.` or `..` segment; no user info, query or fragment; ≤ 2048 bytes | remote |
| `wp_cli` | `[program]` (a command name, or an absolute canonical path), or `[PHP interpreter, -n/-q…, absolute WP-CLI script]`, at most 4 tokens; tokens of `[A-Za-z0-9._/+-]`, none starting with `-` except the options (the command follows `env`, §6.1) | remote |
| SSH local paths (`identity_file`, `known_hosts_file`, `ssh_config_file`, `binary`) | `[A-Za-z0-9._/\:+-]`, `~` only as a leading `~/`, no leading `-`, canonical (no empty, `.` or `..` component), ≤ 4096 bytes; `known_hosts_file` is not `none`, `NUL` or `/dev/…`; `binary` only in `defaults.ssh`; `~` expanded and existence checked at use | ssh argv / ssh's shell |
| `name` / `notes` | 1–128 / 1–1024 characters, no control or invisible formatting (Unicode Cf, Zl, Zp) character | local |
| slug | `^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$` | remote |
| variant marker path / needle (hints, §6.9) | relative, canonical, ≤ 1024 bytes, `[A-Za-z0-9._-]` components ≤ 255 / printable ASCII, ≤ 256 bytes (it may hold `'` and `\`, so it is always quoted) | remote |

File handling:
- **Load:** a missing file is an empty inventory. Parsing uses `deny_unknown_fields`, requires `version`, accepts records only as JSON objects, reads `null` as unset and ignores a leading UTF-8 BOM. Then full validation runs, reporting every problem with its location (`sites[1].server: unknown server 'eu-2'`). A newer `version` (read from the JSON object) is read-only when this build can read it fully (every field and value known, every rule passed). Otherwise it is refused with `inventory_unsupported`, and its problems stay listable.
- **Save:** lock (`inventory.json.lock`, `std::fs::File::lock`, created `0600`, never removed) → temp file created `0600` with `create_new` → fsync → rename → fsync dir (best effort: once renamed, the save has landed). Missing parents are created `0700`. Edits are read-modify-write under the lock (`io::update`), which writes only when the change succeeds and changes something; `load` takes no lock (a save is one rename).
- **Limits:** documents over 16 MiB are refused on read and write. A newer inventory is never written, even when a change replaces it whole or a forced save targets it (`inventory_read_only` if this build can read it, else `inventory_unsupported`, as `update`). A forced save replaces damaged, older and same-version files (a `version` that is not a whole number up to u32 max counts as damaged; with duplicate `version` keys the last one counts), and refuses a file it cannot read (`inventory_io`), since it cannot rule out a newer one; to restore a backup over a newer or unreadable file, move that file aside by hand first.
- **Refused:** a symlinked path (the file or its lock file, even dangling; the directories above may be links, e.g. macOS's `/tmp`), a directory or special file at either path, and a path that names no file (empty, `/`, `..`, or ending with a separator, `/.` or `/..`) or whose file name ends with `.` or a space (Windows strips those, so two spellings would lock different lock files; refused on every platform). Errors carry the path (`inventory_io`). Writes refuse such targets before taking the lock, so nothing is created; front ends check the path as given (`io::check_path`) before making it absolute.
- **Lock wait:** a busy lock is retried for at most 10 s (`io::LOCK_WAIT`), then the write fails with `inventory_locked` and nothing is written (revision 11).
- **No lost edits (revisions 12, 13):** callers that keep an inventory in memory between load and save use `io::load_tracked` + `io::save_if_unchanged`: under the lock the file must still match the fingerprint of the bytes loaded (or still be missing), else `inventory_changed` and nothing is written (the file is also compared before the lock, so a refusal usually creates nothing). `io::save` (blind overwrite) is only for a deliberate "force". `io::save`, `io::save_if_unchanged` and `io::update_tracked` return the fingerprint of the file afterwards; a caller never reloads just to get one.

---

## 8. Architecture

```
apvm-config   apvm-storage        apvm-sites (NEW: inventory · remote · wp · package · ops · hints) ──► apvm-process (rev 15)
       \         /                  ▲      ▲
        apvm-core ──────────────────┘      │   core uses: PluginHints (from builders), build → package → install
         ▲     ▲                           │
     apvm(cli)  apvm-napi ─────────────────┘   front ends: args, prompts, rendering, default locations
```

```
crates/sites/src/
  lib.rs, error.rs (Error + stable code()), problem.rs (located problems), allowlist/{mod, net, paths}.rs (value rules),
  hints.rs, layout.rs (file/dir *names* only)
  inventory/{mod, model, validate, io, resolve}.rs
  package/{mod, headers, rules, archive, layout}.rs
  remote/{mod, quote, script, statement, identity, transport, capture, local, fake, ssh, classify, mux}.rs, remote/ssh/{config, files}.rs
  wp/{mod, command, parse, state, snippets, probe, classify}.rs
  ops/{model, plan, order, apply, verify, ownership, variants, staging, install, list, status, lock, run, runner, report}.rs
  progress.rs (SiteEvent, SiteReporter, Null/Closure/LogFileReporter) · session.rs · manager.rs
crates/process/src/lib.rs (rev 15: core's former process.rs; command/spawn/output, tree kill, ProcessScope)  ·  crates/core/src/deploy.rs  ·  crates/core/src/build/plugins/*.rs (slugs, markers)  ·  projects/{registry, hints}.rs (plugin_hints)
crates/cli/src/commands/{server, site, group}.rs · commands/wp/{mod, args, render, list, status, install, activate,
  deactivate, uninstall, reset, delete, ownership, run}.rs · prompt.rs
crates/napi/src/{sites_inventory, sites, sites_types, sites_progress}.rs
tests/e2e/sites/…  ·  tests/support/fake-remote/{fake-ssh, fake-sudo, fake-wp}
```

### 8.1 Library contract

1. **Every capability is an `apvm-sites` API.** It covers `Inventory` (+ io), `SiteManager` (`probe`, `list`, `status`, `plan`/`apply`, `run_wp`, `unlock`), `PluginPackage::inspect`, `wp::classify`, `PluginHints`, and the reporters (including `LogFileReporter`).
2. **Front ends add only** argument parsing, confirmation, rendering, and default locations (`~/.apvm` + `layout` names). The env override helper `inventory::io::env_override()` (`APVM_INVENTORY`) lives in the library, mirroring `config_io::cache_dir_env_override`.
   Inventory edits go through `io::update` (CLI), `io::update_tracked` (`SiteInventory.update`) or `io::load_tracked` + `io::save_if_unchanged` (`SiteInventory.load` … `save`); a front end never writes back a whole document blindly unless the user forces it.
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
    pub fn plugin_hints(&self) -> Result<PluginHints>;   // fallible: custom builders can declare invalid hints
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
| `wp run`: argv-only, every global parameter refused in every form and position except the allowed overrides (no `--exec`/`--require`), refused commands, read-only allowlist, confirmation / `allowWrites`, identity-run, lock | T1, T7 |
| Hermetic WP-CLI: no user config, no packages, code-loading environment variables removed with `env -u` (§6.1); the identity is derived from the plugins dir's `stat`, never from WordPress's own report (§6.2) | T1, T4 |
| Zip rules (§6.10) | T2 |
| `sudo -n` only; no shell parsing data under sudo or runuser (`env` + the WP-CLI argv; the probe's walk is constant code with positional data); root commands are a closed enum (`stat`; `chown -R -P -h --from=…`, opt-in per site, with the §6.8 preconditions); root never deletes; `chgrp` without root first; `mirror` and `check` → zero root | T3 |
| Nonce framing, output caps, strict parsers, `escape_debug`; `ssh_*` classes only when ssh exits 255 (remote stderr shares the stream) | T4 |
| No stored secrets; inventory and logs `0600`; no secrets in argv | T5 |
| `BatchMode yes`; apvm's known_hosts (the inventory's file, else the user's `~/.ssh/known_hosts`, never one an ssh_config names; a file that keeps nothing refused); `accept-new` only per run; `no` never; all of it in a generated `-F` file, so jump hosts get it too (rev 16) | T6 |
| Plan + one confirmation; non-TTY needs `-y`; `--dry-run`; empty selection = error; site lock | T7 |

**Known limits:**
- The identity comes from the plugins directory's owner as one process sees it at the end of a walk that follows only links the site cannot plant (owned, with their directory, by root or the login user, the directory writable by neither group nor others): the site cannot point the identity at another account's directory, even by swapping links during the probe. For a directory only root's `stat` can read, the identity's own walk must find the same owner and group immediately before WordPress loads. **Residual window:** between that final check and WP-CLI's own load of the site, the site can change the files it owns (wp-config.php, the plugins); that cannot change which identity was chosen, only what the site's own code does as that identity, which it controls anyway.
- The walk follows a link's target component by component (`readlink`, each component checked by the same rules, at most 40 links; audit 3: a trusted link into a directory the site owns could otherwise reach through a link the site planted there).
- Permissions are read from mode bits only: NFSv4 or macOS ACLs that let others write a directory holding a trusted link, or grant access the mode does not show, are not seen.
- A sudoers `env_reset`/`env_keep` that drops or sets `LC_*` differently can localise sudo's and coreutils' messages: classification then fails closed (an unrecognised refusal is `wp_cli_missing` or `probe_failed`, never a wrong success).
- apvm verifies against WordPress's own state, so a site's must-use plugin or drop-in can falsify what it reads. Reads skip regular plugins, nothing more.
- Refused sudo probes are logged on the server; operators should expect those entries (§6.2).
- `wp_cli` names the program that runs as the site's identity, so it is trusted configuration, like the host list. Its shape keeps WP-CLI from being wrapped in another program (§7), but a single program can be anything installed on the server.

---

## 11. NAPI surface

```ts
class SiteInventory {                                   // sync, file-backed
  static load(path?: string): SiteInventory              // default ~/.apvm/inventory.json | APVM_INVENTORY
  static empty(): SiteInventory
  readonly path: string | null;  readonly readOnly: boolean
  defaults(): JsDefaults;  setDefaults(d: JsDefaults): void
  servers(): JsServer[];   upsertServer(s: JsServer): void;  removeServer(id: string, cascade?: boolean): boolean
  sites(): JsSite[];       upsertSite(s: JsSite): void;      removeSite(id: string): boolean
  groups(): JsGroup[];     upsertGroup(g: JsGroup): void;    removeGroup(id: string): boolean
  save(path?: string, opts?: { force?: boolean }): string
      // refuses with `InventoryChanged` when the file changed since load (or, for a path not
      // loaded from, when it exists); the object keeps its changes; `force` overwrites
      // (discarding others' edits), never an inventory written by a newer apvm
  static update(fn: (inv: SiteInventory) => void, path?: string): SiteInventory
      // load → fn → save under the lock: never conflicts; a throw in fn writes nothing;
      // fn must be synchronous: a returned promise/thenable is refused (InvalidArg), its rejection handled
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

### P1 — Foundations (lib) ✅ (2026-10-12)

#### P1.1 — Crate, inventory model, validation, hint types ✅ (implemented 2026-10-11, audited 2026-10-12)
- **Goal:** the pure data layer.
- **Changes:** root `Cargo.toml`; `crates/sites/{Cargo.toml, README.md, src/{lib,error,problem,hints,layout}.rs, src/allowlist/{mod,net,paths}.rs, src/inventory/{mod,model,validate,tests}.rs, tests/inventory.rs}`.
- **Tests:** accept/reject tables per rule; CRUD integrity; serde round-trips; unknown field rejected; newer `version` read-only; `PluginHints` validation (a family needs ≥ 2 slugs; a slug belongs to one family; variant names unique; markers are allowlisted relative paths).
- **Done when:** the crate builds with no `apvm-*` deps (since rev 15: only the leaf `apvm-process`) and every validator is covered.
- **Result:** `crates/sites` with `Inventory` (strict parse, located validation, version handling, CRUD that keeps it valid), `PluginHints`, `layout` names, `Error` + `Code`, and the value rules in `allowlist/` and located problems in `problem.rs`. The refinements are in revisions 7 and 8 (audit). Tests: 105 unit tests, including a randomized property test of the always-valid invariant; 4 integration tests on the §7 example; 2 doctests. Unit tests also guard against any `apvm-*` dependency. Line coverage is 99.7%; the rest is an unreachable serialization error.
- **Depends:** — · **Parallel:** P0

#### P1.2 — Inventory persistence ✅ (implemented and audited 2026-10-12)
- **Goal:** safe load/save/update.
- **Changes:** `inventory/io.rs` (lock, atomic `0600` write, `update`, symlink refusal, `env_override`).
- **Tests:** missing file = empty; located errors; `0600`; no partial file; two concurrent `update`s both land; symlink refused; env override.
- **Result:** `io::{load, save, update, env_override}` with std only (`File::lock`), new errors `InventoryIo`/`InventoryNotAFile`/`InventoryFile` and the `inventory_io` code; details in §7 and revisions 9 and 10. Tests: 51 unit, 4 integration (concurrent updates from 4 processes, `APVM_INVENTORY` from a child environment), 1 doctest; the 3 concurrency tests fail with the lock disabled. Windows: clippy-clean for `x86_64-pc-windows-msvc`. Known limits (low): a FIFO swapped in between the check and the open can block a `load` (only the same user can, in `~/.apvm`; fixing needs `libc`); a crash mid-save can leave a hidden `0600` temp file. Revision 11 bounds the lock wait (10 s, `inventory_locked`).
- **Depends:** P1.1 · **Parallel:** P1.3–P1.5, P3.1

#### P1.3 — Settings merge and targets ✅ (implemented and audited 2026-10-12)
- **Goal:** `ResolvedSite` + `Targets` resolution.
- **Changes:** `inventory/resolve.rs`.
- **Tests:** precedence table; union/order/dedupe; empty selection error; same-WordPress warning.
- **Result:** `Inventory::{resolve_server, resolve_site, resolve_targets}`, `Targets`, `ResolvedTargets` (+ `TargetWarning::{NoSites, SameWordPress}`), `Setting<T>` + `SettingSource`, `ResolvedServer`/`ResolvedSsh`/`ResolvedSite` (`Serialize`, `#[non_exhaustive]`, built only in the crate) and the `BUILTIN_*` constants; rules in §4.2 and revisions 9 and 10. Errors reuse `invalid_input`, located like `groups[1]`. Tests: 27 unit (every field at every level; all 8192 selections of a sample inventory against an independent model, warnings and first error included) and 3 integration tests on the §7 example. Open: whether a site's `command_timeout_secs` overrides the 900 s install timeout (decide in P8.2); warnings have no stable code yet (add with P5's `--json`).
- **Depends:** P1.1 · **Parallel:** P1.2, P1.4, P1.5, P3.1

#### P1.4 — Core: slugs, variant markers, families → `PluginHints` ✅ (implemented and audited 2026-10-12)
- **Goal:** core describes its plugins to `apvm-sites`.
- **Changes:**
  - `crates/core/Cargo.toml` (`apvm-sites`);
  - `Builder::plugin_slug(variant)`, `Builder::variant_marker(variant)`, `Builder::family_traits()` (all with defaults returning "none", so custom builders don't break), implemented for BackWPup/WP Rocket/Imagify per §3.1;
  - `ProjectRegistry::{plugin_hints, project_for_slug}` and `Apvm::plugin_hints`;
  - `apvm info` shows slug + marker per variant; `SKILL.md`.
- **Tests:** declared values equal §3.1; hints validate; `info` output.
- **Result:** the three `Builder` methods (default `None`), declared for BackWPup/WP Rocket/Imagify and checked against all 43 cached artifacts (pro-de/pro-en markers match exactly one variant in each of the 19 pro zips); `ProjectRegistry::{plugin_hints, project_for_slug}`, `Apvm::plugin_hints` (both `Result`, §8.1), `Project::plugin_slugs`, new `projects/hints.rs`, `Error::Sites` + napi `GenericFailure` mapping; `apvm info` shows slugs, markers and a `Family:` section; SKILL.md and READMEs match the real output. Tests: §3.1 table as independent literals, hint validation, invalid custom declarations, `info` helpers and 2 end-to-end `info` tests. Adding `Error::Sites` to core's (not `#[non_exhaustive]`) `Error` breaks exhaustive matches downstream, as `Skill` did in a minor release.
- **Depends:** P1.1 · **Parallel:** P1.2, P1.3, P1.5, P2.x, P3.x

#### P1.5 — Package inspection ✅ (implemented and audited 2026-10-12)
- **Goal:** `PluginPackage::inspect(path, &hints)` per §6.10.
- **Changes:** `package/{mod,headers,rules,archive,layout}.rs`.
- **Tests:**
  - generated zips covering every rule (single root ✓, files at root ✗, two roots ✗, `__MACOSX`, `..`/absolute/backslash ✗, symlink ⚠);
  - headers: missing `Plugin Name` ✗, several headered ⚠, beyond 8 KiB ignored, CRLF, multibyte at the boundary (no panic);
  - bomb caps; unsupported compression → clear error; SHA-256;
  - variant detection from markers.
- **Result:** `PluginPackage` (`path`, `size`, `sha256`, `uncompressed_size`, `slug`, `main_file`, `headers`, `variant`, `warnings`), `PluginHeaders`, `VariantDetection`, `PackageWarning`, `PackageDefect`, the limits as constants, and `Error::PackageInvalid` (`package_invalid`); rules in §6.10 and revisions 9 and 10. Uses the workspace's `zip` 8.6.0 (deflate only) and `sha2` 0.11.0. Tests: every rule and defect on generated zips, the libzip/PclZip differential shapes, ZIP64, headers, marker boundaries, a 9 000-zip fuzz test, a public-API test; all 43 cached artifacts inspect as §3.1 says (~80 ms each, release). `inspect` is synchronous CPU + I/O: async callers use `spawn_blocking`. Residual (low): a non-UTF-8 name without the UTF-8 flag is written differently by libzip and PclZip; it never affects the folder structure or the slug.
- **Depends:** P1.1 · **Parallel:** P1.2–P1.4, P2.x, P3.x, P4.x

#### P1.6 — Inventory conflict detection (lib) ✅ (implemented and audited 2026-10-12)
- **Goal:** a save that cannot lose another process's edit, for callers that keep an inventory in memory (revision 12).
- **Changes:** `inventory/io.rs` (+ tests): `Fingerprint` (opaque: SHA-256 of the exact bytes read, or missing; `sha2` is already a dependency), `load_tracked(path) -> Result<(Inventory, Fingerprint)>` (one read: the inventory and the fingerprint come from the same bytes), `save_if_unchanged(path, &Inventory, &Fingerprint) -> Result<Fingerprint>` (under the lock: re-read safely, compare, write, return the new fingerprint); `Error::InventoryChanged { path }` + code `inventory_changed`; README.
- **Tests:** unchanged → saved, new fingerprint returned and usable for the next save; changed by another writer → `inventory_changed`, file untouched; deleted since load, or created when it was missing → refused; a byte-identical rewrite is not a conflict; read-only (newer) inventory refused; busy lock → `inventory_locked`; the conflict reproduced across two processes.
- **Result:** `io::{Fingerprint, load_tracked, save_if_unchanged, update_tracked, TrackedUpdate}`; `io::save` returns a `Fingerprint`; `Error::InventoryChanged` (`inventory_changed`); README. Audit changes in revision 13. Tests: 25 new unit tests (every case above, plus a deleted directory, a file grown past the cap, a no-op `update` in between, forced-save and `update_tracked` chaining, a failed write retried with the same fingerprint, a symlinked path, and 6 threads that reload on conflict until every edit lands, with conflicts asserted to happen), 2 cross-process tests (a child's edits between load and save, `Fingerprint::missing()` over a file a child created), 1 doctest. Each guard (the comparison, the comparison under the lock, the read-only check, the pre-lock look, the size-cap mapping, the returned fingerprints) was removed in turn and a test failed every time. The threaded test passed 40 of 40 runs. Windows: clippy-clean for `x86_64-pc-windows-msvc`. Workspace: 1 533 tests green. Rev 14 (P2.4 audit): `update_tracked_with` (the change gets the loaded bytes' fingerprint), `check_path`, a pre-lock target check in `save`/`update*`, and a forced save refuses a newer target.
- **Depends:** P1.2 · **Parallel:** P1.3–P1.5, P2.1–P2.3, P3.x

### P2 — Inventory front ends

#### P2.1 — cli: `apvm server` + `server defaults` ✅ (2026-10-12)
- **Goal:** manage servers offline.
- **Changes:**
  - `crates/cli/Cargo.toml`; `paths.rs` (`inventory_file`, `ssh_control_dir`, `sites_logs_dir`); `main.rs` (dispatch, env override);
  - `commands/{mod,server,inventory}.rs`, `commands/server/{fields,render,tests}.rs`; `prompt.rs` (shared `confirm`; `uninstall.rs` and `cache.rs` move onto it, wording and tests unchanged); `status.rs` (`warn_line`);
  - `.claude/skills/apvm-cli/{SKILL.md, references/sites.md}`; `skill/embedded.rs` (`SKILL_FILES`); `crates/cli/README.md`; `tests/cli_inventory.rs`; `tests/cli_lifecycle.rs` (`SKILL_FILES`); `tests/common/mod.rs`.
- **Tests:** add/edit/show/list/remove; defaults set/unset; invalid input → exit 1; `--json`; `--cascade`; `APVM_INVENTORY`; `0600`.
- **Audit notes (rev 10):** every edit goes through `io::update` (lock → load → change → save), never `load` then `save`, which can lose a concurrent edit. Map error locations like `sites[1]` / `all_sites` to the flags the user typed (`--site`, `--all-sites`).
- **Result (rev 14):** every field is declared once (`server/fields.rs`: one exhaustive `Field::spec()` table; clap structs destructure every flag under `#[deny(unused_variables)]`), so a dropped flag cannot silently do nothing. `commands/inventory.rs` holds the shared plumbing for P2.2/P2.3/P4.3/P5 (`Failure` with codes and remedies, `name_flags`, an injectable `Console`, JSON documents). `--json` failures print `{"schema":1,"error":{code,message,problems}}` on stdout and `Error:` on stderr. Relative `--key`/`--known-hosts`/`--ssh-config`/`--ssh-binary` values warn only when they change. The audit's 17 surviving mutants (of 27) led to the table tests (every flag read from clap saved and reported, every set + unset refused, every relative path warned once); 35/35 mutants are now killed. Not done here, carried to P2.2/P5: mapping target locations (`sites[1]`, `all_sites`) to `--site`/`--all-sites`, since no `server` command takes targets. A refused change on a missing inventory still leaves the `0700` directory and an empty `0600` lock file (library behavior, documented).
- **Depends:** P1.2, P1.3 · **Parallel:** P1.4–P1.6, P2.4, P3.x

#### P2.2 — cli: `apvm site` ✅ (2026-10-12)
- **Goal:** manage sites offline (the probe arrives in P4.3).
- **Changes:** `commands/site.rs`; `SKILL.md`.
- **Tests:** as P2.1, plus `--wp-cli` tokens, `--run-as`/`--owner`/`--ownership`, filters.
- **Audit notes (rev 14):** reuse `commands/inventory.rs` (`Failure`, `name_flags`, `Console`, `confirm_change`) and the one-table field pattern of `server/fields.rs`; `remove` re-checks the confirmed record under the lock like `server remove`.
- **Result (rev 15):** `apvm site add|edit|list|show|remove`, no SSH. Each field is declared once (`site/fields.rs`, clap structs destructured under `#[deny(unused_variables)]`); the one-table pattern and the confirm-then-recheck removal became shared modules (`commands/inventory/{field_table,removal}.rs`), and `server` moved onto them with byte-identical output (35 commands compared against the old binary). `--wp-cli` is split at ASCII whitespace, refused tokens named `--wp-cli[i]`. `--group` on `add`/`edit` joins existing groups in the same locked change and never leaves one (leaving is P2.3). `list --server/--group`: union within a flag, intersection across, an unknown id is an error. `show` prints effective settings with their source; `--json` embeds `ResolvedSite`. `remove` re-checks the site and its groups under the lock. Remedies name flags: root refusals add `--allow-root` advice only when root is the actual cause (re-validated with root allowed), with wording that depends on whether the edit revoked `allow_root`; an unknown group says groups have no CLI yet and gives the inventory path and the minimal `{"id":…,"sites":[]}` shape (switch to `apvm group list` in P2.3). Library: `auto`/`login` are keywords in any letter case for `run_as` (and `auto` for `owner`), like `root`; an inventory holding such a value is now `inventory_invalid` on load. 3 audit rounds; 14/14 mutants killed. Carried to P4.3/P5: mapping `sites[i]`/`all_sites` locations to `--site`/`--all-sites`.
- **Depends:** P2.1 · **Parallel:** P2.4, P3.x

#### P2.3 — cli: `apvm group` ✅ (2026-10-13)
- **Goal:** manage groups.
- **Changes:** `commands/group.rs`; `SKILL.md`.
- **Tests:** add/edit/remove; unknown site rejected; site removal updates groups.
- **Result:** `apvm group add|edit|list|show|remove`, no SSH (`commands/group.rs`, `group/{fields,render,tests}.rs`), on the shared `field_table` (notes only) and `removal` modules. Sites (`add <ID> [<SITE>…]`, `edit --add/--remove`) are repeatable and comma-separated; each must exist (`--remove` too), checked under the lock after the group's own checks (read-only inventory, taken or invalid id, notes: as `site add/edit` report their record first), so the first unknown one is `site '…' does not exist` + `'apvm site list' shows the sites`, never `sites[i]`. A repeat counts once; adding a member or removing a non-member is a no-op (`Group '…' is unchanged.`, nothing written); members keep their order, new ones go last; one site in `--add` and `--remove` is refused. `edit` also takes `--unset notes` (not in §5.2 before: an empty `--notes` is refused, so notes could not be cleared). `list`: `ID SITES MEMBERS`; `--json` documents hold the records as stored (`{"groups": […]}`, `{"group": {…}}`). `remove` keeps the sites and re-checks the whole group (members and notes) under the lock (`inventory_changed`). The unknown-group remedy is now `'apvm group list' shows the groups` for every command (the P2.2 hand-edit hint and inventory-path line are gone). Site removal leaving groups was already in the library and `site remove`; now covered end to end with `group list/show` (also `server remove --cascade`). Tests: 58 unit (47 command, 6 field-table, 5 render: parsing, every flag clap defines, membership, reports, whole commands on temp files, confirm-then-recheck races, read-only/invalid files, `0600`) and 10 integration tests (round trip, missing file, usage, unknown ids, contradictions, piped `y`, site/server removal, read-only, `APVM_INVENTORY`, 8 concurrent `edit --add` processes). Audit: the group's own refusals now come before an unknown site; the skill's `description` + `when_to_use` (1 545 characters) were cut under Claude Code's 1 536-character listing cap, now guarded by a test. Workspace: 2 076 tests green.
- **Depends:** P2.2 · **Parallel:** P2.4, P3.x

#### P2.4 — napi: `SiteInventory` ✅ (2026-10-12)
- **Goal:** the inventory from Node.
- **Changes:** `crates/napi/Cargo.toml`; `sites_inventory.rs`, `sites_types.rs`, `sites_input.rs` (strict reading of JS records, like `cache_input.rs`); `error.rs` (shared code mapping); `lib.rs` docs; `__tests__/sites-inventory.test.ts`, `__tests__/setup.ts` (per-file temp `APVM_INVENTORY`); `crates/napi/README.md`; library `io::{update_tracked_with, check_path}`.
- **Behavior (revision 12):**
  - `load(path?)` uses `io::load_tracked` and remembers the path and fingerprint; `empty()` remembers none.
  - `save(path?, { force? })`: the target is `path`, else the loaded path, else the default (`APVM_INVENTORY`, else `~/.apvm/inventory.json`). The expected state is the remembered fingerprint when the target is the loaded path (compared as absolute paths), else "missing", so `empty().save()` never wipes an existing inventory. Without `force` it calls `io::save_if_unchanged`; with `force`, `io::save` (which refuses a newer target, readable or not). On success the object tracks the target and the fingerprint the call returned (never one from a reload, which could hold another process's later edit) and returns the path. On `InventoryChanged` nothing is written and the object keeps its changes. A loaded read-only (newer) inventory always rejects `save` with `InventoryReadOnly`. Comparing absolute (not canonical) paths can report a false `InventoryChanged` (`/tmp/x` vs `/private/tmp/x` on macOS): safe, documented.
  - Paths are checked as given with `io::check_path` before `absolute()` (Unix keeps `..`; Windows normalizes and adds the drive).
  - `static update(fn, path?)`: `io::update_tracked_with` (the callback's object tracks the fingerprint loaded under the lock; no re-read); `fn` gets a `SiteInventory` over the locked copy and must be synchronous. A throw in `fn` writes nothing and is rethrown as is (beware napi's pending-exception pitfalls). It returns the saved inventory, tracked with the `TrackedUpdate` fingerprint. Calling `save`/`update` on the same path inside `fn` fails with `InventoryLocked` after 10 s (documented, tested), never deadlocks; only then does the `InventoryLocked` message name the `update()` callback. A promise or any thenable returned by `fn` is refused (`InvalidArg`, nothing written) and its rejection is handled through the `Promise.prototype.then` captured when the addon loads (per JS environment), so a subclass overriding `then`/`catch` cannot crash Node either (only a throwing species constructor can). After a throw or refusal, the object `fn` received keeps its edits, and saving it is guarded like any save.
  - Records are read strictly: unknown, non-enumerable or wrongly typed keys, list fields that are not plain arrays, and non-whole or out-of-range numbers are `InvalidArg`; a lone surrogate becomes U+FFFD.
  - Rejection codes: `InventoryChanged`, `InventoryLocked`, `InventoryIo`, plus the existing inventory codes.
- **Tests:** round-trip; validation errors with `err.code`; `APVM_INVENTORY` from `process.env`; `save` → `0600`; a second process edits between `load` and `save` → `InventoryChanged`, the file keeps that edit, the object keeps its own, reload + reapply + save succeeds; `force` overwrites; `empty().save()` over an existing file refused; chained saves on one object; `update` from parallel workers all land; a throwing `fn` writes nothing.
- **Result (rev 14):** sync API (`update` must call `fn` on the JS thread under the lock; a writer blocks the event loop for at most the 10 s lock wait plus `fn`). Added `readOnly`. Codes: library `invalid_input` → `InvalidArg`, the rest PascalCase. Audit tests added: async throw in a child process (no unhandled rejection), thenables, every `force`-less spelling (`{}`, `{force: undefined}`, `null`, …) stays guarded, forced save over newer files (both kinds), trailing `.`/`..`, no lock file for refused targets, non-enumerable keys, lone surrogates, a lock held by another process (no callback hint), escaped objects, Windows path semantics (`#[cfg(windows)]`, not compiled locally: the napi crate's C dependencies cannot cross-build from macOS; CI covers it). 68 JS tests; every audit and re-audit mutant killed. New `crates/napi/src/intrinsics.rs` (`module_exports` hook).
- **Depends:** P1.3, P1.6 · **Parallel:** P2.1–P2.3, P3.x

### P3 — Remote execution (lib)

#### P3.1 — Script builder ✅ (2026-10-12)
- **Goal:** typed, framed scripts for both delivery modes (§6.1).
- **Changes:** `remote/{mod,quote,script,identity,statement}.rs` (prefixes incl. `-g`, closed `RootCommand`, `UploadScript`, crate-internal `Statement`/`Body`), `remote/tests.rs` (real shells); `allowlist/paths.rs` (`wp_cli` shape rule moved here, `wp_cli_arg`, `upload_value`).
- **Tests:**
  - render goldens; adversarial round-trips through real `sh -s` and `sh -c` (Unix); outer layer through the locally available shells (skipped if absent);
  - `UploadScript` accepts only `^[A-Za-z0-9._/+-]+$` values (so `'` and `\` are rejected at construction); its templates are single-line and contain neither `\\` nor `\'`;
  - every script is one brace group whose first command is `LC_ALL=C`, and a cut at any byte runs nothing (real shells); identity commands never contain a shell (`sh -c`) under sudo or runuser;
  - the hermetic WP-CLI argv: `env -u … WP_CLI_CONFIG_PATH=/dev/null PAGER=cat <wp_cli> --skip-packages --exec='umask(umask() & ~0<g>);'`, with `<g>` from the plugins dir mode (golden per mode); the argument-refusal table (§6.12); `+id` chown; `sudo -n -u root`; a 30 s per-child timeout.
- **Audit notes (rev 8):** `path` and `plugins_dir` may hold `'`, `$`, `*` and spaces, so they are only ever POSIX-quoted in stdin scripts, never put in an `UploadScript`. Variant needles may hold `'` and `\`.
- **Result (rev 14):** public `remote::{quote, Nonce, FileMode, AccountName, Identity, RootAccess, OwnerIds, PluginEntry, RootCommand, WpCli, Script, UploadScript}`; refused values are `invalid_input` with a located problem; no new dependencies (the nonce uses std's randomly keyed `RandomState`). Audit and re-audit changes in revision 14. Verified in Debian 12 with sh, dash, bash, zsh, ksh, mksh, yash (ASCII only: it cannot read non-ASCII input under `LC_ALL=C`), busybox ash, and fish, tcsh, csh as login shells, against real WP-CLI 2.12.0.
- **Depends:** P1.1 · **Parallel:** P1.2–P1.5, P2.x, P0

#### P3.2 — Transport trait, capture, local shell, fake ✅ (2026-10-12)
- **Goal:** run scripts and capture results safely.
- **Changes:** `remote/{transport,capture,local}.rs`, `#[cfg(test)] fake.rs`.
- **Tests:** caps; timeout kills the child; 20 MB stdin hash round-trip; nonce behind noise; 255 remap.
- **Audit notes (rev 14):** unframe by the line exactly equal to the nonce (P3.1's test helper `after_nonce` shows the rule); a missing nonce line is a failure, never "no output" (startup files that eat the start of stdin are caught that way).
- **Result (rev 15):** `remote::{Transport, BoxFuture, Request, Delivery, Payload, Exit, Execution, Captured, PayloadError, unframe, STDOUT_LIMIT, STDERR_LIMIT}`; crate-internal `local::run` (P3.3's runner) and `#[cfg(test)]` `LocalShell` and `fake::{FakeTransport, Reply}`. `Exit::Status` is never 255 (`TransportFailed`; 254 = a command's 255); `Execution::new`/`Captured::new` are public so other crates can implement `Transport` (P8.5). `payload()` is `Ok` only for an exit status and an uncut stdout: `NonceMissing { stdout_truncated }`, `Truncated`, `Interrupted { exit }`. `stdin_written()` means handed to the transport, not received (staging verifies by SHA-256). Upload files are opened `O_NONBLOCK` (a FIFO swapped in cannot hang the call), type-checked on the opened handle, then streamed; a missing/forbidden/wrong-kind path is `invalid_input`, a read failure `process_failed`. Output gets a 2 s settle after exit. **Process handling moved to the new leaf crate `apvm-process`** (user decision): core's `process.rs` moved there unchanged in behavior (core re-exports `ProcessScope`); sites spawns through `apvm_process::spawn`, so every child is recorded in the caller's `ProcessScope` and killed by napi's teardown at `process.exit()`; a failed `wait` forgets the tokio handle (no signal to a reused PID); on Linux the held child is pinned by a pidfd; the stop-then-kill window when the killer itself dies is documented. Tests check that every recorded PID disappears (a merely stopped tree fails); verified on macOS, Linux (slim container) and Windows clippy. 3 audit rounds. Obligations: P3.3 decides which environment variables `ssh` gets (`GITHUB_TOKEN`/`GH_TOKEN` are inherited today) and maps a spawn `NotFound` to `ssh_missing`; P5.1 runs each `tokio::spawn`ed site task inside `ProcessScope::current()`; P5.4 runs `SiteManager` calls under napi's `CallGuard`; P6.2 checks `exit()` and treats `NonceMissing` after a mutation as unknown → re-read.
- **Depends:** P3.1 · **Parallel:** P4.1, P1.x, P2.x

#### P3.3 — SSH transport ✅ (2026-10-13)
- **Goal:** the real transport with actionable errors.
- **Changes:** `remote/{ssh,classify}.rs`; `tests/support/fake-remote/fake-ssh`.
- **Tests:** argv goldens across precedence; Windows argv; classification table from P0's stderr samples (§3.5, both OpenSSH versions; noise lines ignored; `ssh_*` only when ssh exits 255, so remote stderr with the same phrases never matches); hermetic fake-ssh; E2E (connect, unknown host key, `accept-new`, wrong key).
- **Audit notes (rev 8):** jump hosts only get `-l`, `-p`, `-F` and `-v` from the main connection (verified). `BatchMode`, `StrictHostKeyChecking`, `UserKnownHostsFile`, `IdentityFile` and `ConnectTimeout` must reach them through a generated `-F` config, or apvm chains the hops itself; otherwise a hop can prompt, or use `~/.ssh/known_hosts`. The ProxyCommand that ssh builds for a ProxyJump runs through the user's `$SHELL`; the §7 allowlists keep the `-F` path, the ssh program and the hops literal there. Never combine `accept-new` with a known_hosts file that does not persist: at use, refuse a `known_hosts_file` that exists but is not a regular file (a symlink to `/dev/null` passes the §7 rules).
- **Result (rev 16):** `remote::{SshTransport, SshOptions, HostKeyPolicy, classify, SshFailure, SshFailureKind, host_key_notices, HostKeyNotices}` (`HostKeyNotices { recorded, not_saved, refused_certificate }`, audit 5), `Execution::script_started`, codes `ssh_missing`, `ssh_auth_failed`, `ssh_host_key_unknown`, `ssh_host_key_changed`, `ssh_unreachable`, `ssh_timeout`, `ssh_connection_lost`, `ssh_failed`, errors `SshMissing`, `SshUnusable`, `SshConfigFile`, `Ssh`; files `remote/{ssh,classify}.rs`, `remote/ssh/{config,files,tests}.rs`, `remote/classify/tests.rs`, `tests/ssh_e2e.rs`, `tests/support/fake-remote/fake-ssh`, the harness checks `ssh.jump-host-options`, `ssh.jump-host-shell-startup`, `ssh.jump-host-isolation`, `ssh.known-hosts-persistence`, `ssh.generated-config-rules`, `ssh.agent-isolation`, `ssh.system-config-include` (`checks/ssh.sh`, `lib/client_{jump,known_hosts,system_config}.sh`) and a `ssh_e2e` step in CI's `e2e-sites`. `tempfile` moved from dev-dependencies to dependencies (already in the workspace); no new crates.io dependency. Design and verified facts: revision 16 and §3.5 "Verified in P3.3". `local::run` takes a `Launch` (program, args, environment changes, directory).
  - **Tests:** argv goldens with every SSH field at the defaults and server levels (and nothing a hop needs on the command line); the generated file for Unix and Windows paths; `%`, quoting and refused paths with their field; `~`, relative paths and no home; the checks at use (missing key or config, a group-writable config, a known_hosts link to `/dev/null` or a directory, read-only file or directory with `accept-new`); `-G` output parsing; ssh's environment; through `fake-ssh` (hermetic, Unix): the argv and file ssh gets (`0600`, its absolute path when shell-literal and its bare name otherwise, alone in a private `0700` directory where ssh runs), the pinned options for a client that knows them and none for an older one (a `-old` link to `fake-ssh`), the file gone after the call, a timeout and a dropped call (ssh's process tree gone too), uploads, a missing or unusable ssh, `accept-new`, every failure class, a lost connection after the script started; classification of 28 real samples from five OpenSSH versions with noise around them (10.1's post-quantum warning included), remote stderr with ssh's phrases never classified. E2E (`ssh_e2e`, 9 tests): a script, a 1 MB upload read back, an unknown host key refused then recorded with `accept-new`, a changed key (strict and accept-new), a wrong key, a closed port, a jump host (both keys known; hop key missing → the strict-checking message, proving the hop read apvm's file; `accept-new` recording both), an ssh_config with `SessionType none`, `StdinNull yes`, `ForkAfterAuthentication yes` (from 8.7) and a 1 s `ChannelTimeout` (from 9.6) (a script, a silent 3 s command, an upload and a jump host still work), and a timed-out call through a jump host (the hop's ssh seen while it runs, gone after). Ran on macOS (OpenSSH 10.3p1), and in containers on the harness network with Debian 13 (10.0p2; the whole crate's tests as a non-root user) and Ubuntu 24.04 (9.6p1); audit 2 ran it (a static musl build, as a non-root user) with 7.6p1, 8.2p1, 8.7p1, 8.9p1, 9.2p1, 9.6p1, 10.0p2 and 10.6p1. `verify.sh`: 49 pass, 2 INFO, 1 SKIP (the optional real-BackWPup check), `ssh.jump-host-shell-startup` and `ssh.agent-isolation` added by audit 2. Audit 3 added: OpenSSH's rule for included files on mode and owner; default includes kept when missing, left out when unreadable, a system file others may write refused at `ssh_config_file`; the private directory's base (configured, a shell-literal temporary directory, else a sticky root `/tmp`); through `fake-ssh` (a `-sysconf` link names its own directory as ssh's), the system file included and read, refused when group-writable (nothing runs, nothing left behind), left out when unreadable, the defaults read again for another program, eight concurrent calls in eight directories, and no directory left after a failed `-G`; the classifier samples of a key file others can read (10.3p1, 7.6p1), which explain only a refused login; E2E (now 11): that key file (`ssh_auth_failed` with the `are too open` line), and a transport without `ssh_config_file` that reads the sandbox's `~/.ssh/config` and includes this machine's real system file (the one E2E test that reads it); `verify.sh` check `ssh.system-config-include` (now 50 pass, 2 INFO, 1 SKIP). Audit 4 added: the vendor system file under `/usr` (its path; chosen only when the system file is missing or a dangling link and it exists, an unreadable system file still wins); the two `fake-ssh` tests that read this machine's system file now use the `-sysconf` link; classifier samples (now 34) of a key that differs from the one known for the address (7.6p1, 9.6p1) and of a revoked key (`@revoked`, `RevokedHostKeys`; 10.3p1); E2E (now 12): a key marked `@revoked` (strict and accept-new: `ssh_host_key_changed`). The whole E2E suite also ran in an openSUSE Tumbleweed container (10.5p1, as a non-root user, the system file `/usr/etc/ssh/ssh_config`), and a probe there reached the target through a `Host` alias that only an `/etc/ssh/ssh_config.d` drop-in defines; the crate's tests pass there too (590, as a non-root user, `/bin/sh` bash 5.3). A container needs an init process (`docker run --init`) for the process-tree tests: without one, PID 1 never reaps the killed processes, whose zombies `kill -0` still finds. Audit 5 added: classifier samples (now 62) of every class whose phrases it added or changed, on the versions whose wording differs (7.6p1, 8.2p1, 9.6p1, 10.0p2 glibc and musl, 10.3p1: connect errors, resolver failures, SSH-1 and unreadable banners, a reset handshake, a banner that never comes, a hop that forwards nothing, a server that opens no session, host certificates refused, an unreadable `RevokedHostKeys` file, a key file that is no key, a missing ssh_config key, a bad option, a ProxyCommand's ssh asking nobody), the host key classes needing ssh's closing host key line, line-start phrases, the trailing banner line after a hop, summaries skipped, the `refused_certificate` notice; `-T` on the `-G` run and a failed run's first non-noise line; `UpdateHostKeys yes` for ssh's own known_hosts files when the client defaults to it (the `-old` link like 8.2p1 does not); the vendor copy only for `/etc/ssh/ssh_config`; E2E (now 13): ssh's own known_hosts files learn the target's other host keys as plain ssh does (10.3p1: all three; a client before 8.5 nothing); `ssh.system-config-include` also asserts Debian's exception for a file its owner's private group may write. Audit 6 added: the cause read from ssh's last `\r\n` message (real banner samples from six clients: a refused login, a close during authentication, a lost session, a remote shell that printed and exited 255; constructed orders ssh cannot print; a line ending with `\n` after ssh's last message, a stderr cut mid-line, an explanation only from ssh's own lines), the whole-line host key refusal, a hop's channel timeout as `ssh_timeout`, `host_key_notices` ignoring a remote copy of ssh's notices; the 62 samples unchanged.
  - **Known limits:** a `ProxyCommand` in the user's ssh_config (not a ProxyJump) is the user's own command and gets none of apvm's options; ssh and the commands ssh_config makes it run (ProxyCommand, `Match exec`) run in the call's private directory, so relative paths there resolve against it (and find nothing); a `UserKnownHostsFile` in ssh_config is ignored (set `known_hosts_file`); the system ssh_config is included as a user file, so a relative `Include` inside it would be read from `~/.ssh` (the systems tested, macOS included, use absolute ones), and one owned by another user or writable by its group or others, which ssh alone reads unchecked, is refused (`invalid_input` at `ssh_config_file`: set one of your own); a default ssh_config this user cannot read is left out, as ssh skips it; when the system ssh_config does not exist, its vendor copy under `/usr` (`/usr/etc/ssh/ssh_config`) is included if it exists, as openSUSE's ssh reads it (verified on Tumbleweed only; a stock OpenSSH never reads that file, and no other system is known to ship one); Windows: argv and file rendering are unit-tested and clippy-clean, the runtime is not verified (Win32-OpenSSH's `-G` output, its reading of `/` paths, and its ACL check of included files, which apvm does not read); a Windows temporary directory whose path is not shell-literal (a user name with a space) gives ssh the file's bare name, so a jump host's shell startup that changes directory would break it (no shared literal directory to fall back to); so does a configured directory (`SshOptions::with_config_dir`) on any system, and a Unix temporary directory that is not shell-literal when `/tmp` is not root's and sticky. The temporary directory (or the configured one) is trusted, as by every program that writes there: one that others may write without the sticky bit would let them swap the private directory for theirs. A bare `binary` (`ssh`) is resolved through `PATH` at every call, but its defaults are read once per transport: a `PATH` that later finds another ssh keeps the first one's pinned options. An ssh_config's other settings stay the user's: `GlobalKnownHostsFile`, `KnownHostsCommand` and `VerifyHostKeyDNS` can add trusted host keys, `IdentityFile` adds keys after the inventory's (a FIDO key of the user's offered after apvm's may ask for its PIN on the terminal), `SendEnv`/`SetEnv` send what they name (apvm's environment, as the user's ssh would; a top-level `SendEnv -*` cannot help, later `SendEnv` lines add up), `UpdateHostKeys yes` adds the server's other host keys, proven by the known one, to apvm's known_hosts file (`ask` is skipped under `BatchMode`), `PKCS11Provider`/`SecurityKeyProvider` load the user's libraries (no PIN prompt under `BatchMode`). With `accept-new` and no `known_hosts_file`, a missing `~/.ssh` is refused, naming the directory and the fix (ssh 8.4+ would create it; older ones run without keeping the key). Only a `ProxyJump` hop is covered: a `ProxyCommand` of the user's that runs ssh can still prompt on the terminal apvm runs in. ssh stays in apvm's session so that Ctrl+C reaches it (`apvm-process`); apvm forwards no signals, so a new session (`setsid`) would leave ssh running after Ctrl+C, and dropping only the controlling terminal (`TIOCNOTTY` in the child) loses Ctrl+C on macOS though not on Linux (audit 3, §3.5): a Linux-only fix would need `unsafe` `pre_exec` code and make a prompting ProxyCommand fail on Linux while it asks on macOS (revisit when the front ends handle signals). It opens no ssh-askpass dialog (`SSH_ASKPASS_REQUIRE=never`, 8.4+). A key file of this user's that others can read is ignored by ssh (one owned by another user is not checked) and reads as `ssh_auth_failed` whose line names the file and its mode (an agent holding the key still logs in, so apvm does not refuse such a file). A remote shell killed by a signal looks like a lost connection (exit 255, empty stderr). The classifier reads ssh's last message, the last line ending with `\r\n` (audit 6): a server's login banner or a remote startup file's stderr (`\n`) decides nothing, but a server that sends `\r\n` itself, after its host key passed, can pass its text for ssh's; a remote shell that prints and exits 255 before the nonce line is `ssh_failed` with that text. A ProxyCommand of the user's whose ssh has `StrictHostKeyChecking yes` refuses an unknown key with the same lines as a ProxyJump hop: `ssh_host_key_unknown`, though `--trust-new-host-key` cannot reach that ssh. A host certificate an `@cert-authority` line rejects is `ssh_failed` under strict checking, but with `accept-new` ssh trusts its plain key as new and the call runs (OpenSSH's own fallback; `host_key_notices().refused_certificate` says so). Debian's and Ubuntu's ssh include a file of the user's that only its private group may write; apvm refuses such an `ssh_config_file` (upstream's rule: `chmod go-w`). `UpdateHostKeys yes` ends the file whenever the inventory names no `known_hosts_file` and the client defaults to it (8.5+), also when an ssh_config sets `VerifyHostKeyDNS yes`, which turns plain ssh's default off (with a key verified by secure DNS alone, ssh then records the server's proven keys in apvm's known_hosts file; plain ssh would not), and over an ssh_config's `UpdateHostKeys` that applies only in ssh's final pass (`Match final`, or a `Host` block for a name `CanonicalizeHostname` produced): apvm's line is read in the first pass, and a `Match final` around it would force a final pass, which changes which `Host` blocks apply (audit 6, verified with `-G`). `Connection closed by <ip> port <n>` during authentication (a server's `LoginGraceTime`) reads as `ssh_unreachable` (ssh prints the same before the banner exchange).
  - **Obligations:** P3.4 puts `ControlMaster`/`ControlPath`/`ControlPersist` into the generated file in place of `ControlMaster no`/`ControlPath none` (`config.rs` `FIXED_OPTIONS`); top-level options reach hops, so it decides whether hops multiplex (scope them with `Match`/`Host` if not); the file is already double-quoted with `%` doubled, which the rev 8 `ControlPath` note asks for; a ControlPersist master outlives the call and its file (it does not reread it); `UpdateHostKeys yes` stays the file's last line, after the includes (audit 5). P4.2 maps `classify()` to the probe's codes, `Exit::TimedOut` to `timeout`, and uses `script_started()` after mutations. P4.3 maps `--trust-new-host-key` to `HostKeyPolicy::AcceptNew`, reports `host_key_notices` (a `not_saved` key as a warning; a `refused_certificate` as a warning that the recorded key bypassed the certificate authority), and adds remedies per code and kept line (§3.5 "Classification table", audit 5; never `--trust-new-host-key` for `ssh_failed`, whose host key lines it cannot fix or would bypass; for `ssh_host_key_unknown`, add that a ProxyCommand's own ssh is not reached by it, audit 6) (unknown key: `--trust-new-host-key`; changed: `ssh-keygen -R` for the name or, when the line says `differs from the key for the IP address`, the address it names; a line that says `revoked`: no `ssh-keygen -R`, which would drop the revocation, only investigate; a group-writable `ssh_config_file`: `chmod go-w`; a `%` in a known_hosts path: set `known_hosts_file` elsewhere; `ssh_auth_failed` whose line ends in `are too open.`: `chmod 600` the key; the system ssh_config refused at `ssh_config_file`: set one of your own).
- **Depends:** P3.2, P1.3 · **Parallel:** P4.1

#### P3.4 — Multiplexing and per-server limits ✅ (2026-10-13)
- **Goal:** fast repeated execs within `MaxSessions`.
- **Changes:** `remote/mux.rs`, per-server semaphore.
- **Tests:** option goldens; long-path/Windows fallback; E2E `ssh -O check` live after the first exec.
- **Audit notes (rev 8):** ssh splits `ControlPath` on whitespace and expands `%` and `${}` in it. Double-quote it and double its `%`: the front end's home directory may contain spaces.
- **Result (rev 17):** `remote::{Multiplexing, NoMultiplexing, ServerLimits, CONTROL_PERSIST_SECS}`, `SshOptions::{with_control_dir, with_server_limits, control_dir, server_limits}`, `SshTransport::multiplexing()`; files `remote/{mux,limits}.rs`, `remote/mux/tests.rs`; `config.rs` takes the sharing lines in place of P3.3's `ControlMaster no`/`ControlPath none` (same position, so `UpdateHostKeys yes` stays last) and quotes the control directory (`%%`, `${` refused); `config::destination` (the argv's `-l -p -J -- host`). No CLI or napi surface change (front ends pass `~/.apvm/ssh-mux` from P4.3/P5), no new dependency. Design: §6.1 items 9 and 10.
  - **Decisions:** multiplexing needs a caller's control directory (§8.1: no default locations); calls never become masters: a starter of their own (`-N`) does, so no call's process tree holds the shared connection (audit round 2); only direct connections are shared: through a jump host or a ProxyCommand ssh hides the proxy's messages once `ControlPersist` is set (audit round 3); the tag keeps calls with another key, known_hosts file, host key policy, ssh_config, program or jump list off a master authenticated under other options (a strict and an `accept-new` call get two masters); an unusable directory, a too-long path and Windows turn it off rather than fail (ssh would fail every call: a bind error other than "exists" is fatal, verified); the semaphore is per inventory server id, waits in order, and is taken before the call (not counted in its timeout); the classifier treats the verified fallback lines as noise: `mux_client_request_session: session request failed: …`, `mux_client_request_session: read from master failed: …`, `muxclient: master hello exchange failed`, `ControlSocket … already exists, disabling multiplexing` (any other `mux_client_request_session:` line keeps its diagnosis).
  - **Verified** (macOS 10.3p1; Debian 10.0p2 in the target; Ubuntu 7.6p1, 8.2p1 and 9.6p1 in containers on the harness network): the master forks into its own session with stdio on `/dev/null`, so the first call returns at once (also through a jump host, whose `ssh -W` lives on with `/dev/null` stderr); `%%` in `ControlPath` is a `%` and quotes keep spaces, `${HOME}` is expanded (so refused); 86 bytes bind on macOS and 90 on Linux, one more exits 255 (`unix_listener: path "…" too long for Unix domain socket`); later calls share the connection (same `SSH_CONNECTION`); eleven calls at once through one master all run, the refused one printing the two fallback lines; a jump host gives two masters; a stale socket (master killed) is replaced silently; a master killed during a call ends it with exit 255 and an empty stderr (`ssh_connection_lost` after the nonce).
  - **Tests:** 22 new unit tests (the crate's lib 599 → 621; 629 after the audit, 636 after round 2, 644 after round 3, 652 after round 4, 656 after round 5): the mux goldens (lines, `%` quoting, every off reason and its message, the limit at and past its edge, Windows), tag stability and separation, RFC 4648 base32, the directory rule (mode, owner, link, file) and its creation, probe and refusals; `ServerLimits` (limit held, per server, clones share, default 4); the classifier's fallback lines around every real sample; through `fake-ssh`: the file with the sharing lines, its tag stable and different for `accept-new`, the directory made `0700`, the off cases (no directory, `multiplex: false`, too long, others may write) with the call still running, and at most 2 calls at once with a limit of 2. E2E (`ssh_e2e`, now 16; 17 after the audit, with the relay test; 19 after round 2, with the killed-call cascade and the stopped master; still 19 after round 3, the jump-host master test replaced by a jump host's unknown key with multiplexing requested; 20 after round 4, with `Match command`; 21 after round 5, with a slow `Match exec`): a master `ssh -O check` finds live after the first call (none before), later calls and a 1 MB upload over it, a non-multiplexing server on its own connection, a jump host and its server with one master each, a master killed mid-call (`ssh_connection_lost`, then a new master). `verify.sh` check `ssh.multiplexing` (`lib/client_mux.sh`, 10.0p2: the 90/91-byte limit, a live master, a shared connection, MaxSessions fallback, a jump host's two masters; the `ssh` group ran: 11 pass, 1 INFO). Windows: clippy-clean for `x86_64-pc-windows-msvc`; the runtime is off there by design.
  - **Audit (rev 17, 2026-10-13; no high, one medium):** **a master whose TCP died silently** (medium; a Wi-Fi/VPN switch, a NAT reset, sleep; reproduced with a relay that swallows a live connection) stalled every call about 75 s (`ServerAliveInterval 15` × `CountMax 4`) before `mux_client_request_session: read from master failed: Broken pipe` and a fallback, and a call with a shorter timeout ended `TimedOut`: now shared connections use keep-alives 5 s × 3 (measured stall 19.6 s), `ControlPersist 15s`, and a call that timed out before its script started runs `ssh -O stop` on its master (running calls go on, the next call starts a new master; E2E through the relay: the next call takes under 4 s, 13.6 s without the stop); **a stopped or wedged master** (low) delays a call by the connect timeout, then `muxclient: master hello exchange failed` and a fallback (verified): that line and `read from master failed` are noise now (samples); **the 37-byte directory budget on macOS** (low): an opt-in `SshOptions::with_tmp_control_fallback()` uses `/tmp/apvm-mux-<uid>` (sticky root `/tmp` only, same checks) when the control directory is too long (a library still picks no location on its own); **ancestors** (low): every directory above the control directory (resolved) must be this user's or root's and not group- or other-writable unless sticky; **`config_text()` side effects** documented (it creates the directory and binds a test socket); **equality** (low): `SshOptions` no longer derives `PartialEq` (two `new()` had different limits), `ServerLimits::shares_slots_with` replaces its pointer `==`; tests added for a slot freed by a timed-out and a dropped call, FIFO order, the `-O stop` (fake-ssh `-O`, `stall`) and its absence after a started call or without sharing; docs say a queued call waits without a bound. `verify.sh` `ssh.multiplexing` also asserts the stopped master's line and `-O stop`.
  - **Audit round 2 (rev 17, 2026-10-13; one high, two medium):** **a killed call cut every call riding its master** (high; reproduced on 10.3p1): a call that became the master kept the jump host's `ssh -W` (or a ProxyCommand: nc, cloudflared, SSM) as its own child, so apvm's kill of a timed-out or dropped first call killed the connection the others rode (exit 255 mid-script, outcome unknown, even mid-mutation): now calls are clients only (`ControlMaster no`), and a starter of their own (`ssh … true`, `ControlMaster auto`, `ControlPersist 15s`) makes the master, whose carrier is left to init (verified: the `ssh -W` re-parented to init; E2E: a first call through a jump host timed out and killed, a second call on the same master finishes; with calls as masters the second call exits 255, so the test reproduces the bug); **5 s × 3 keep-alives** (medium) would cut a 900 s install on a 15–20 s blip: now 10 s × 3 (item 7); **a stopped or wedged master never recovered** (medium): it answers neither calls nor `-O stop` and runs no keep-alives; now `-O check` (3 s cap) or a call's `muxclient: master hello exchange failed` removes its socket (path from `ssh -G`, only this configuration's socket in the checked directory) and the next starter replaces it (E2E: a `SIGSTOP`ped master replaced, the next call within 12 s; without the removal it fails); lows: the noise rule narrowed to the verified tails; `-O stop` and a dead hop documented; plan drift fixed (generated-file row, decisions, `Cargo.toml` comment, the harness's `ControlPersist`); flaky thresholds widened (8 s, a lower bound on time for the limit test, 3 s for the started-call test); `fake-ssh` logs a control command's argv as received (`-J` checked) and answers `-O check` (no master unless `master live`); the opt-in fallback's base is injectable in tests (never the real `/tmp`); the `0775` home case documented.
  - **Audit round 3 (rev 17, 2026-10-13; one high, two medium):** **a jump host's failure lost its reason** (high; verified on 10.3p1): with `ControlPersist` ssh sends the ProxyCommand's (`ssh -W`'s) stderr to `/dev/null`, so the starter of a master through a jump host with an unknown key failed with only `Connection closed by UNKNOWN port 65535` (noise: `ssh_failed` instead of `ssh_host_key_unknown`), likewise a refused login or an unknown name, and `accept-new` hop notices vanished; `fake-ssh` printed them anyway: now multiplexing is off when `ssh -G` shows a `proxyjump` or `proxycommand` (`NoMultiplexing::Proxied`, `ProxyUnknown` when `-G` fails; checked once per configuration), `fake-ssh` answers `-G` (`controlpath`, `proxyjump`, `proxycommand`), the E2E checks a jump host's unknown key with a control directory (`ssh_host_key_unknown`) and keeps the killed-call regression (B untouched, nothing shared), and `verify.sh` shows the lost reason; **the starter's `true`** (medium) kept the master's carrier in the starter's tree until `true` ended and ran a ForceCommand: now `-N` (verified: exit 0 in 0.1 s, the master live, the server's host key notice on stderr); **serialized starters** (medium): N calls to an unreachable server failed one after another, up to N connect timeouts: now calls that waited take a failed or unanswered start's result (`Gate`; fake-ssh: 4 calls fail in one 2 s start, not 8 s; one master for 4 concurrent calls), and the start's time is documented as outside the call's timeout; lows: a socket is removed only on the hello-failed line (a check that gets no answer starts nothing; `-O check` never times out on a stopped master, verified, so the call's own hello failure decides), a timed-out starter's partial stderr is kept, `SshTransport::call` split, the `%C` assumption and the races documented, fake-ssh models a stopped master (`-O check` hangs, a call prints the hello line) and `-N`/`-G`.
  - **Audit round 4 (rev 17, 2026-10-13; three medium):** **a stuck master never recovered before OpenSSH 9.6** (medium; 8.9p1, 9.2p1: no hello-failure line, the call hangs, `-O stop` too): a check with no answer in 3 s now means a stuck master, whose socket is removed (same guards), and the call (and its waiters) runs with `ControlPath none` (fake-ssh `master hang`: the call returns at once and the socket is gone); **a starter that lost a race** (medium; 8.9p1) waited as a plain `ssh -N` until killed at connect + 15 s: now its stderr is watched and it is stopped on `already exists, disabling multiplexing`, then checked again (fake-ssh `race`: done in well under 30 s; without the stop, 30.5 s), and the gate is process-wide per control directory and tag; **`Match command`/`Match sessiontype`** (medium; 10.3p1) could send calls through a proxy that `-G` and `-N` never showed: `-G` now runs for a call (`sh -s`) and for a master (`-N`), and a proxy in either, or a different host, user, port or socket, turns multiplexing off (`SessionDependent`, `Unresolved` when `-G` fails; E2E with `Match command "sh -s"` + `ProxyJump`: off; with the plain `-G`, on, so the test catches it; fake-ssh `command-host`); lows: the hello-failure line counts only as ssh's own line (`\r\n`) before the script started; the `-G` verdict expires after 30 s (documented); waiters get a failed start without its host key notices; the socket probe also makes a hard link (OpenSSH links its socket into place), and a starter's 255 that classifies as `ssh_failed` lets the call run on its own; timing margins widened.
  - **Audit round 5 (rev 17, 2026-10-13; five low):** `ssh -O check` reads the whole ssh_config first, so a `Match exec "sleep 4"` made every call `Stuck` and removed a healthy master's socket (10.3p1, 8.9p1): apvm now connects to the socket itself and waits up to 3 s for the master's hello (verified on 10.3p1: 25 bytes at once from a live master, nothing from a stopped one, a refused connection on a stale socket; E2E with `Match exec "sleep 4"`: the second call rides the first's master); the socket path is the one `ssh -G` expands for a call (it must be one of apvm's, else multiplexing is off, `Unresolved`), and the `-G` result is cached for 30 s, failures too; a re-look after a lost race that finds a stuck master takes the stuck path; the hello failure line counts when it is ssh's own (`\r\n`), whether or not the call's script then ran on its own connection (it does); fake-ssh starts a stand-in master (python3, hello to every connection, gone after 5 s idle) and prints the hello failure only when `ControlPath` is not `none`; new tests: a call that meets a stuck master itself, a remote copy of the line removing nothing; the orphan-unlink churn of a resumed stuck master documented; `run_until`, `mux::control_socket` and `shared_call` split.
  - **Known limits:** a master outlives apvm (up to 15 s idle) and is outside every `ProcessScope` (napi's `kill_all` does not reach it); a master keeps the options it started with (an ssh_config or known_hosts edit applies to the next master); calls already waiting on a master whose connection died stall until its keep-alives give up (30 to 40 s) or their own timeout; a stopped or wedged master costs the next call 3 s (no hello) before it runs on its own connection, and if it resumes and exits later it removes its successor's socket (that master runs on unreachable until idle, and one more is started); while calls already on it wait the connect timeout (9.6+) or hang until their own timeout (before 9.6); a shared connection that a blip of over about 30 s cuts ends its calls as `ssh_connection_lost` (unknown outcome after a mutation; a call's own connection tolerates about a minute); a start's time (up to the connect timeout + 15 s) and the wait for the configuration's process-wide gate come before a call's own timeout; a starter that races another process's and finds its master live runs as that master's client, and its `-N` opens a session (a login shell with no stdin, which ends at once; a ForceCommand runs then); looking for a master costs one connection to its socket per call; the control directory check is stricter than OpenSSH's (a `0775` home or `~/.apvm` with a user-private group turns multiplexing off; `chmod g-w` fixes it); the ancestor check resolves links once (a link swapped afterwards is not seen); the limit counts one process's calls only (another apvm or the user's own sessions on the same master can still meet `MaxSessions`, which then costs a connection, not the call); a master's socket left by a killed apvm is replaced (a stale one when `-O check` fails, a stuck one after a call's hello failure); apvm recognizes its sockets by `%C` being 40 hex digits (a SHA-1 on every client checked, 7.6p1–10.3p1; a client that changed it would only leave a stuck master's socket in place); servers behind a jump host or a ProxyCommand, or resolved differently for a call and a master, are never multiplexed; an upload's remote command (`sh -c '…'`) is not resolved, so a `Match command` that tells uploads apart is missed; the `-G` result (also a failure to resolve) is reused for 30 s (an included file edited, or a `Match exec`/`localnetwork` answer that changed, within that time is not seen), and `Match exec` commands run twice per check; host key notices of a failed start are reported by the call whose starter met them, not by the calls that waited for it.
- **Depends:** P3.3 · **Parallel:** P4.x

### P4 — WordPress driver and probe

#### P4.1 — lib: WP-CLI commands and parsers ✅ (2026-10-12)
- **Goal:** typed `wp` argv and robust parsing.
- **Changes:** `wp/{mod,command,parse,state,snippets}.rs`.
- **Tests:** argv goldens per command, **asserting the banned flags never appear**; parser fixtures (clean, notice-prefixed incl. the PHP 8.5 deprecation line, notice-suffixed (shutdown), HTML error, truncated, empty, unknown status, single-file plugin, must-use and drop-in rows).
- **Result (rev 15):** `wp::{WpCommand, CommandKind, WpSite, Slug, StagedZip, Resolved, PluginList, PluginRow, PluginStatus, Lookup, CommandOutput, Parsed, Notices, OutputDefect, SiteFacts, WpCliVersion, BootCheck, RequiresPlugins}`, `wp::parse::{plugin_list, regular_plugins, cli_version, site_facts, requires_plugins, boot_check}`, `wp::snippets::{SITE_FACTS, REGULAR_PLUGINS}` (constant `eval` snippets, §10), code `wp_command_failed`. Pure; every argv goes through P3.1's `WpCli`.
  - **Commands:** typed values only; one plugin per process, never `--all/--exclude/--deactivate/--uninstall/--activate` (goldens). Lifecycle commands and `requires_plugins` take a `Resolved` plugin (`PluginList::resolve`: a unique regular `Found` only), so no command can name a slug WP-CLI's fetcher would take for another plugin; `--no-load` skips by WP-CLI's own plugin name (`get_plugin_name`), not the slug (verified: `--skip-plugins=hello.php` still loads Hello Dolly). `StagedZip` is built from `(remote_tmp_dir, mktemp name, n)`. `CommandKind`: `is_read` (plugins skipped: retried, no lock), `loads_plugins` (boot check and mutations: locked, never retried), `changes_files` (install, uninstall, **reset**, delete).
  - **Lookups follow WP-CLI's fetcher** (`$name.php === $file || $file === $name || dirname($file) === $name`), never display names (WP-CLI renames regular plugins whose names repeat: `twofiles/a.php` → `twofiles/a`). `Lookup::{Absent, Found, Ambiguous, Shadowed, Unreadable}`: `plugin list` hides a regular plugin behind a must-use plugin or drop-in at `<slug>.php` or `<slug>` (verified: an active Hello Dolly vanished), and prints `null` for non-UTF-8 file names, which WP-CLI can still act on; both read as undetermined until `REGULAR_PLUGINS` (raw `get_plugins()`, with lossless `file_hex`) resolves them. apvm never acts on a non-UTF-8 file.
  - **Parsers:** a strict line grammar (a state machine, bounded memory and lookahead) skips PHP diagnostics, multi-line messages, PHP and Xdebug traces and BOMs around exactly one payload and returns them as notices; it refuses a non-zero exit, empty or notice-only output, HTML, any other line (remedy names wp-config.php, must-use plugins, drop-ins or php.ini), two payloads, cut or invalid JSON, cut line payloads (no trailing newline), stdout flagged truncated, missing/unknown/duplicate/mistyped fields, and duplicate files. Site facts are observations (absolute paths and URLs without control characters, canonical or not; `content_dir_realpath` added): a value must pass its own argument rule before it becomes one. The boot check passes on exit 0 plus exactly one `http(s)://` line with a host; a changed siteurl is a warning; it catches fatals, `die()`, HTML and empty output, not a plugin faking a boot. `requires_plugins` mirrors WordPress 7.1.3's `sanitize_dependency_slugs`; `PluginRow::dependency_slug` mirrors `convert_to_slug` (`hello.php` → `hello-dolly`).
  - **Fixtures:** 38, captured from the P0 harness (WP-CLI 2.12.0, PHP 8.4.26; PHP 8.5.11 and Xdebug 3 for their lines) except the synthetic ones listed in `wp/parse/tests.rs`. Four audit rounds (two HIGH "Found X while WP-CLI acts on Y" cases fixed by making them unbuildable).
  - **Obligations:** P4.2 sets `with_stdout_truncated` from the transport and maps `OutputDefect::Failed` to `not_wordpress`/`wp_not_installed`; P5/P6 treat `Ambiguous`, `Shadowed` and `Unreadable` (after `REGULAR_PLUGINS` fails) as undetermined, never absent, and block them for mutations; P6.1 orders by `requires_plugins` mapped through `dependency_slug`; `--all` reports rows it cannot resolve instead of skipping them silently (conservative: `slug_of` gives none when the WP-CLI name is ambiguous, e.g. `a.php` next to `a/b.php`). A `Resolved` is unique in *a* parsed list, not tied to a site: P4.2's session binds the list to its `WpSite`, and P6.2 re-reads under the site lock before each mutation. `--no-load` skips every plugin sharing the target's WP-CLI name (a WP-CLI limit): P6.2/P7.2 warn when another regular row shares it. Filters (`all_plugins`) apply to lifecycle commands but not to the reads: only an entry a site deliberately adds could redirect `uninstall`/`delete` (site code, §10 known limits).
- **Depends:** P3.1 · **Parallel:** P3.2–P3.4

#### P4.2 — lib: probe, identity (owner + group), session ✅ (2026-10-13)
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
- **Audit notes (rev 8):** decide root by the resolved uid/gid 0, never by name (`toor`, numeric ids). Names read from the server (`stat %U/%G`) must pass the §7 user rule before they reach argv or an `UploadScript`: remote names such as `a b` or `x'y` exist. Check that `cli version` really is WP-CLI (`WP_CLI_VERSION`), since `wp_cli` is chosen by the operator.
- **Audit notes (rev 14):** parse the six-line `stat` (`%u %g %a %F %U %G`, one per line, `RootCommand::stat_ownership`'s contract): decide by ids, refuse anything but `directory`, `UNKNOWN` = no name; names must pass `AccountName`. The login user's stat uses the same format; verify the BSD `stat -f` equivalent. Set `WpCli::allow_root` exactly when the identity's uid is 0 (test it). Independent probe steps need per-statement status capture added to `remote/statement.rs`'s `Body`, or one `Script` per step; build the exact-shape probe with `WpCli`.
- **Result (with the audit's fixes):** `wp::probe::{probe, SiteProbe, HostFacts, PluginsDir, ProbedIdentity, Capabilities, UpgradeDir, ProbeFailure, ProbeWarning, site_timeout, DEFAULT_TIMEOUT}`, `remote::{plan_identity, IdentityFacts, IdentityPlan, Candidate, Creation, Reference, Account, LoginAccount, DirOwnership, SwitchTools, IdentityRefusal, RootRole, NamedRole, SudoRefusal, SudoRefusalKind}`, `Session` (+ `CommandRun`, `SitePlugins`, `SiteTarget`, `TargetOp`), `CommandKind::targets_plugin`; crate-internal `remote/{steps,inspect,call}.rs`; codes `sudo_*`, `owner_is_root`, `plugins_dir_mismatch`, `wp_cli_missing`, `not_wordpress`, `wp_not_installed`, `plugins_dir_not_writable` (a probe warning), `timeout`, new `probe_failed`; errors `Probe`, `Timeout { started }`, `Killed`, `ScriptNotRun` (no nonce line: `ssh_failed`). No new dependencies.
  - **Steps:** one round trip each. Independent statements run in one script (`remote/steps.rs`: each statement's status, stdout and stderr framed by `<nonce> <i> <status>` marker lines; a missing marker is `probe_failed`, never a guess). H runs as the login user: `id -u/-g/-un/-gn/-G`, `printenv SHELL`, `command -v` of sudo, runuser, sha256sum, shasum, openssl and `wp_cli[0]`, the walk of the plugins dir plus a diagnostic `stat -L -c '%u⏎%g⏎%a⏎%F⏎%U⏎%G' --`, `id -u` of the `run_as`/`owner` users; a BSD retry (walk with `stat -f '%u%n%g%n%Mp%Lp%n%HT%n%Su%n%Sg'`, verified on macOS: `0755`, `Directory`, `Symbolic Link`, ids printed for names) only when `-c` is refused as an option (audit 2), other failures are `probe_failed`; the directory is read by the walk (step H row of §6.2: one `sh` process, `cd -P` per component, `pwd -P` + `stat .`; links only when root or the login user own them and their directory, which neither group nor others may write; audit 1: site code could replace `plugins` with a link to the login user's home; audit 2: a separate `realpath` and `stat` left a swap window); `Permission denied` → root's `stat` (`sudo -n -u root`, GNU/busybox only, refuses anything but a directory), then, immediately before W, the identity's own walk must find the owner and group root read (`PluginsDirChanged` otherwise), and its physical path becomes the reference path. I tries the table's candidates in order with the exact `WpCli` shape (`cli version`; `--allow-root` exactly when the candidate's uid is 0); a sudo refusal moves to the next candidate (warning `IdentityFallback`), and when all are refused the first is classified (§6.2 table; `sudo -n -l -u U [-g G] true`, or `… -u root stat` for root's `stat`), the message naming the sudoers line to add; sudo-rs's messages are recognized too (§3.5); a sudo failure whose stderr was cut by login noise is `probe_failed`; phrases count only on lines sudo prints (`sudo:`, `sudo-rs:`, `Sorry, user`). W reads `SITE_FACTS`; WP-CLI's own `Error: This does not seem to be a WordPress installation.` → `not_wordpress`, `'wp-config.php' not found.` / `The site you have requested is not installed.` → `wp_not_installed` (captured on the harness), other failures stay `wp_command_failed`; `WP_CLI_VERSION` must equal `cli version`'s, else `wp_cli_missing`. O compares `realpath(WP_PLUGIN_DIR)` with H's path (`plugins_dir_mismatch`, naming the unchanged identity). C (`test -w`/`-d`, `df -Pk`) only warns, also when its output cannot be read (`ChecksUnreadable`, nothing assumed writable).
  - **Identity table** (`remote/identity/decision.rs`, pure): reference = the dir's owner/group or the explicit `owner` (its user must own the dir by uid, its group must be the dir's group by name, else `plugins_dir_mismatch`); root decided by ids (owner uid or identity uid 0) unless `allow_root` (`owner_is_root`); a gid-0 group drops `-g` instead (audit 2); `U == L`: no prefix, or `sudo -u L -g G` first when `G ≠ Lg` (not on a setgid directory whose group `L` is in: audit, a refused sudo was logged and mailed every probe); `U ≠ L`: `runuser` for a root login that has it, else `sudo -u U -g G` then `sudo -u U` (`CheckAfter`), else `sudo_missing`; accounts without a usable name (`UNKNOWN`, a BSD id, a name failing `AccountName`) cannot be switched to (`invalid_input`); `run_as: login` never switches; a `run_as` user switches the prefix only, the reference stays the directory's (or `owner`'s), so `AtCreation` only when the identity's uid is the reference owner's and it runs with `G`. `Creation::{AtCreation, CheckAfter}` tells P6.3 whether the post-install check must expect repairs.
  - **Session** (`session.rs`): `Session::open` probes, then `run` executes `WpCommand`s as the identity; `CommandRun::output` sets `with_stdout_truncated` from the transport (P4.1 obligation); transport failures go through `remote/call.rs` (exit 255 → `classify()`, `Exit::TimedOut` → `timeout` with `started` from `script_started()`, P3.3 obligation). `plugin_list`/`regular_plugins` return `SitePlugins` bound to the session; commands that name a plugin (`targets_plugin`) run only through `Session::run_on(&SiteTarget, TargetOp)`, which accepts a target only from its own session; `Session::run` refuses them (P4.1 obligation; `Script`/`Transport` stay public, outside the session's guard); `Session::new` is crate-only, so a public caller cannot pair a probe with another server's transport.
  - **Deviation:** step H runs no generic `sudo -n -l`. The identity's sudo is tried directly in step I (a refusal is logged by sudo, as any refused probe), and `sudo -n -l -u U [-g G] <command>` runs only after a refusal, to classify it; the table needs no listing to decide.
  - **Audit 3:** the walk resolves a trusted link's target itself (`readlink`; absolute targets restart from `/`, relative ones continue, `..` moves to the physical parent; every component checked; more than 40 links: exit 75), uses every component as `./<name>`; a walk that cannot enter a directory the diagnostic `stat -L` sees (a 0750 plugins dir; dash's `cd` gives no reason) is denied, so root's `stat` reads it; a login or sudo target whose primary group is gid 0 is refused without `allow_root` (`RootRole::PrimaryGroup`; `id -g` as a `-g`-less sudo identity); E2E tests restore the harness with a drop guard.
  - **Audit 4:** a link target keeps its trailing newlines (`readlink … && echo /`, verified on dash and macOS sh); a `-g`-less sudo identity's `id -g` runs before WP-CLI runs as it (only those candidates pay the round trip), a sudo refusal of it is classified, a non-numeric answer is `probe_failed`; the confirming walk's exit 70 (no diagnostic there) is `probe_failed`; the hermetic tests set their directories' modes, so they pass under umask 002 (the whole crate's tests ran under umask 002).
  - **Tests:** 97 unit tests: the identity table row by row (every group case, root by id, explicit owners, `run_as` users), the sudo classification table, step markers, step H/C readers (GNU and BSD `stat`, `df -Pk`), the probe over `FakeTransport` (new `Reply::Steps`) for every §6.2 failure code, the fallbacks, timeouts and warnings, sessions; hermetic through `fake-ssh` (new `path` directive) with `fake-sudo` and `fake-wp` (`tests/support/fake-remote/`) and this machine's `sh`/`id`/`stat`/`readlink` (the BSD retry on macOS; links through `..`, loops, a component named `-`). E2E `tests/probe_e2e.rs` (15 tests, CI step `Probe E2E`): every case listed above, plus missing tables, a path without WordPress, a missing `wp_cli` and `zshy`/`tcshy`/`dashy`, a `plugins` dir replaced by a link to deploy's home and a linked `wp-content` behind root's `stat` (both refused before WordPress loads, with control runs proving the wp-config.php mark works), and a root-owned release link (`/var/www/<current> -> single`) followed; a root-owned link into ftpuser's home, whose `public_html` the site swapped for a link to `single`, refused before WordPress loads; a 0750 plugins dir read through root's `stat`. Unit tests only: the session's refusal of unbound targets, sudo-rs's messages (captured samples), the owner change between root's `stat` and the identity's walk (the swap window is not raced in E2E); hermetic: a release link of the login user followed and the same link in a world-writable directory refused; the hostile config test has a control run proving the config and package run outside the hermetic shape. Ran on macOS (OpenSSH 10.3p1) against the harness.
  - **Known limits:** root's `stat` fallback is GNU/busybox only (a BSD server whose login user cannot reach the site fails as `probe_failed`);  reads are not retried yet (P5.1); sudo-rs is verified from captured messages only (no harness target); a sudo refusal hidden by more than 1 MiB of login stderr noise is `probe_failed`, not classified; "`chgrp` only when the post-install check finds offenders" is P6.3's to verify (the probe reports `CheckAfter` for `ftpuser`).
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
- **Audit notes (rev 14):** map target locations (`sites[1]`, `groups[0]`, `all_sites`) to the flags typed (`--site`, `--group`, `--all-sites`) with `commands/inventory.rs`'s `name_flags` (deferred from P2.1, which takes no targets).
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
- **Audit notes (rev 8):** pass only numeric ids to `chown`, `chgrp` and `find`, and decide root by id (see P4.2).
- **Audit notes (rev 14):** use `chgrp +<gid>` and `find -uid/-gid` (numeric, name-first lookup otherwise); `find … | head -n 20` with `find`'s own status needs a pipeline statement kind added inside `statement.rs`/`frame`, never hand-written text; root `chown` is per (F, FG) pair; busybox root `chown` has no `--from` → `ownership_unfixable`.
- **Depends:** P6.2 · **Parallel:** P6.4, P7.1, P8.1

#### P6.4 — Site lock
- **Goal:** §6.13.
- **Changes:** `ops/lock.rs`; acquire/release wired into `apply` (and later `run_wp`).
- **Tests:** goldens; path validation; fake: busy → wait → `site_locked`, stale → broken with warning, release on failure paths; E2E: two concurrent applies on one site serialize.
- **Audit notes (rev 8):** key the lock, and the same-WordPress warning, on the probe's `realpath` of `path`: through a symlink, two canonical spellings can name one WordPress.
- **Audit notes (rev 14):** the holder file needs an output-redirect statement kind added in `statement.rs`; `rm -rf` only on a validated path.
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
- **Rev 15 note:** `wp plugin uninstall --skip-delete` deletes the plugin's translation files (`languages/plugins/<slug>-*.{po,mo,l10n.php,json}`) before its `--skip-delete` branch (verified, WP-CLI 2.12.0). So `reset` changes files (`CommandKind::changes_files`): it is blocked under `DISALLOW_FILE_MODS` like the other file changes, and its confirmation says the translations go.
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
- **Audit notes (rev 8):** find staging dirs by a literal prefix of `remote_tmp_dir` (it may hold `.` and `+`), never by a regex built from it.
- **Audit notes (rev 10):** `PluginPackage::main_file()` and header values may hold any character, control characters included: treat them as data (quote, escape before printing), never splice them into a command. Call `inspect` through `spawn_blocking`.
- **Audit notes (rev 14):** upload through `UploadScript::tee`, restricted to a typed staging path (`<tmp>/apvm.XXXXXXXXXX/package-<n>.zip`) rather than any safe absolute path; `mktemp -d`, `df` and the checksum run in stdin `Script` bodies.
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
  - ~~`Error::Sites(#[from] apvm_sites::Error)` + `crates/napi/src/error.rs` mapping~~: done in P1.4 (napi maps it to `GenericFailure`; refine per code here).
- **Audit notes (rev 10):** a builder with one variant records no variant in its builds (`variants_built = []`); map such builds through that sole variant, as `plugin_hints` does.
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
- **Audit notes (rev 14):** `WpCli::arg` already enforces §6.12's global-parameter rule (`allowlist::wp_cli_arg`); the classifier adds the command rules. Treat `post edit`, `config edit` and `--edit` as refused or `may-write` (they run `$EDITOR`, which apvm now unsets). `help` is safe (`PAGER=cat`).
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
- **Audit notes (rev 10):** subsites added as separate sites with the same `path` raise the same-WordPress warning (§4.2). That is correct (one plugins directory); document it in the multisite docs.
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
     │      └ P1.6 ┐
     ├ P1.3 ───────┴ P2.4 ────────────────────┼──────────────┐
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
