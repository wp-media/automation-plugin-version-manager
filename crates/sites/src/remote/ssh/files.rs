//! The local files one ssh call names: where they are ([`LocalFiles::resolve`],
//! pure) and whether they are usable ([`check`], at use).
//!
//! **Where:** `~/` (or `~\`) is the home directory apvm was given, else
//! the `HOME` it runs with; a relative path is taken from the current
//! directory. apvm writes absolute paths, so ssh opens exactly what apvm
//! checked: ssh itself would expand `~` from the password database and
//! ignore `HOME`, and would read a relative `Include` from `~/.ssh`
//! (verified on OpenSSH 9.6p1, 10.0p2 and 10.3p1). Unset fields fall back
//! to what ssh would use: `~/.ssh/known_hosts` and `~/.ssh/known_hosts2`,
//! and `~/.ssh/config` (plus the system file, found by the transport).
//!
//! **Usable** (plan §7 "existence checked at use"):
//! - the key and the inventory's ssh_config file must be regular files:
//!   ssh would go on without a missing key, and `Include` skips a missing
//!   file silently, so another host could be reached than the one the file
//!   names;
//! - the inventory's ssh_config file must not be writable by group or
//!   others, and must belong to this user or root (Unix): ssh refuses to
//!   `Include` such a file ("Bad owner or permissions", verified), though
//!   `ssh -F` would read it, so apvm says so before it runs ssh;
//! - without one, the system-wide ssh_config must pass the same check
//!   ([`default_includes`]): ssh reads it unchecked on its own, but apvm
//!   includes it, and an `Include` is checked (verified with one owned by
//!   another user on 7.6p1, 8.2p1 and 10.0p2, and a root-owned `0664` one
//!   on 10.0p2). A default file that exists but this user cannot read is
//!   left out: ssh skips it on its own, while an `Include` of it fails the
//!   call (`Can't open user config file`; verified for both files on
//!   10.0p2, for the system one on 7.6p1 and 8.2p1). When the system file
//!   does not exist, its vendor copy under `/usr` is the one included, if
//!   it exists, as openSUSE's ssh reads it ([`default_includes`]);
//! - the known_hosts file new keys go to must be a regular file, or not
//!   exist yet. A symbolic link to `/dev/null` passes plan §7's text rules,
//!   and with it ssh "permanently adds" every key and runs the command
//!   (verified). With [`HostKeyPolicy::AcceptNew`] it must also be
//!   writable (or its directory must be, when it does not exist yet): ssh
//!   otherwise trusts the new key for this call only and runs the command
//!   anyway (verified with a known_hosts file in a missing directory).

use std::path::{Path, PathBuf};

use super::HostKeyPolicy;
use crate::inventory::ResolvedSsh;
use crate::problem::{Problem, quoted};

/// The `known_hosts_file` field.
const KNOWN_HOSTS: &str = "known_hosts_file";
/// The `ssh_config_file` field.
const SSH_CONFIG: &str = "ssh_config_file";

/// A local path and the inventory field it comes from (for messages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalPath {
    /// The field: `identity_file`, `known_hosts_file`, `ssh_config_file`
    /// or `binary`.
    pub(crate) field: &'static str,
    /// The absolute path (or, for `binary`, a bare command name).
    pub(crate) path: PathBuf,
}

/// The ssh_config files a call includes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigSource {
    /// The inventory's `ssh_config_file`, read alone (as `ssh -F` does).
    Inventory(LocalPath),
    /// What ssh reads by default: the user's `~/.ssh/config`, then the
    /// system-wide file (found by the transport).
    Default {
        /// `~/.ssh/config` (it may not exist).
        user: PathBuf,
    },
}

/// Every local file one call names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalFiles {
    /// The ssh program.
    pub(crate) program: LocalPath,
    /// The private key, when set.
    pub(crate) identity: Option<LocalPath>,
    /// The known_hosts files; new keys go to the first.
    pub(crate) known_hosts: Vec<LocalPath>,
    /// The ssh_config files to include.
    pub(crate) config: ConfigSource,
}

impl LocalFiles {
    /// Where the files of `ssh` are.
    ///
    /// # Arguments
    ///
    /// * `ssh` - The server's effective SSH settings
    /// * `home` - The home directory, if known
    /// * `cwd` - The current directory, for relative paths
    ///
    /// # Errors
    ///
    /// The problems (located by field) when a value starts with `~` or
    /// needs a default under it while no home directory is known.
    pub(crate) fn resolve(
        ssh: &ResolvedSsh,
        home: Option<&Path>,
        cwd: &Path,
    ) -> Result<Self, Vec<Problem>> {
        let program = program(&ssh.binary.value, home, cwd);
        let identity = ssh
            .identity_file
            .as_ref()
            .map(|key| file("identity_file", &key.value, home, cwd))
            .transpose();
        let known_hosts = known_hosts_files(ssh, home, cwd);
        let config = config_source(ssh, home, cwd);
        match (program, identity, known_hosts, config) {
            (Ok(program), Ok(identity), Ok(known_hosts), Ok(config)) => Ok(Self {
                program,
                identity,
                known_hosts,
                config,
            }),
            (program, identity, known_hosts, config) => Err([
                program.err(),
                identity.err(),
                known_hosts.err(),
                config.err(),
            ]
            .into_iter()
            .flatten()
            .collect()),
        }
    }

    /// The known_hosts file new keys go to.
    pub(crate) fn recording_known_hosts(&self) -> Option<&LocalPath> {
        self.known_hosts.first()
    }
}

/// The known_hosts files: the inventory's, else `~/.ssh/known_hosts` and
/// `~/.ssh/known_hosts2` (ssh's defaults).
///
/// # Arguments
///
/// * `ssh` - The server's effective SSH settings
/// * `home` - The home directory, if known
/// * `cwd` - The current directory
///
/// # Errors
///
/// When a home directory is needed and none is known.
fn known_hosts_files(
    ssh: &ResolvedSsh,
    home: Option<&Path>,
    cwd: &Path,
) -> Result<Vec<LocalPath>, Problem> {
    if let Some(known_hosts) = &ssh.known_hosts_file {
        return Ok(vec![file(KNOWN_HOSTS, &known_hosts.value, home, cwd)?]);
    }
    let first = under_home(home, KNOWN_HOSTS, ".ssh/known_hosts")?;
    let second = LocalPath {
        field: KNOWN_HOSTS,
        path: first.path.with_file_name("known_hosts2"),
    };
    Ok(vec![first, second])
}

/// The ssh_config files: the inventory's alone, else `~/.ssh/config` (and
/// the system file).
///
/// # Arguments
///
/// * `ssh` - The server's effective SSH settings
/// * `home` - The home directory, if known
/// * `cwd` - The current directory
///
/// # Errors
///
/// When a home directory is needed and none is known.
fn config_source(
    ssh: &ResolvedSsh,
    home: Option<&Path>,
    cwd: &Path,
) -> Result<ConfigSource, Problem> {
    match &ssh.ssh_config_file {
        Some(config) => Ok(ConfigSource::Inventory(file(
            SSH_CONFIG,
            &config.value,
            home,
            cwd,
        )?)),
        None => Ok(ConfigSource::Default {
            user: under_home(home, SSH_CONFIG, ".ssh/config")?.path,
        }),
    }
}

/// `relative` under the home directory: an unset field's default.
///
/// # Arguments
///
/// * `home` - The home directory, if known
/// * `field` - The unset field, for messages
/// * `relative` - The path under it (`.ssh/known_hosts`)
///
/// # Errors
///
/// When no home directory is known.
fn under_home(
    home: Option<&Path>,
    field: &'static str,
    relative: &str,
) -> Result<LocalPath, Problem> {
    let home = home.ok_or_else(|| {
        Problem::new(
            field,
            format!("is not set, and no home directory is known to find ~/{relative}"),
        )
    })?;
    Ok(LocalPath {
        field,
        path: home.join(relative),
    })
}

/// A file field's absolute path: `~/` expanded, a relative path taken
/// from `cwd`.
///
/// # Arguments
///
/// * `field` - The field, for messages
/// * `value` - Its value (plan §7's rules already passed)
/// * `home` - The home directory, if known
/// * `cwd` - The current directory
///
/// # Errors
///
/// When `value` starts with `~` and no home directory is known.
fn file(
    field: &'static str,
    value: &str,
    home: Option<&Path>,
    cwd: &Path,
) -> Result<LocalPath, Problem> {
    let path = match tilde_rest(value) {
        Some(rest) => home.ok_or_else(|| no_home(field, value))?.join(rest),
        None => cwd.join(value),
    };
    Ok(LocalPath { field, path })
}

/// The ssh program: a bare command name stays one (found on `PATH`);
/// anything with a separator is a file, made absolute like [`file`].
///
/// # Arguments
///
/// * `value` - The `binary` setting
/// * `home` - The home directory, if known
/// * `cwd` - The current directory
///
/// # Errors
///
/// When `value` starts with `~` and no home directory is known.
fn program(value: &str, home: Option<&Path>, cwd: &Path) -> Result<LocalPath, Problem> {
    if !value.contains(['/', '\\']) {
        return Ok(LocalPath {
            field: "binary",
            path: PathBuf::from(value),
        });
    }
    file("binary", value, home, cwd)
}

/// What follows a leading `~/` or `~\`, if `value` has one.
///
/// # Arguments
///
/// * `value` - A path setting
fn tilde_rest(value: &str) -> Option<&str> {
    value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
}

/// The problem for a `~` path without a home directory.
///
/// # Arguments
///
/// * `field` - The field
/// * `value` - Its value
fn no_home(field: &'static str, value: &str) -> Problem {
    Problem::new(
        field,
        format!(
            "{} starts with '~', but no home directory is known",
            quoted(value)
        ),
    )
}

/// Check that the files `files` names are usable for a call with
/// `host_keys` (see the module docs).
///
/// # Arguments
///
/// * `files` - The resolved files
/// * `host_keys` - Whether new host keys are recorded
///
/// # Errors
///
/// The problems, located by field.
pub(crate) fn check(files: &LocalFiles, host_keys: HostKeyPolicy) -> Result<(), Vec<Problem>> {
    let mut problems = Vec::new();
    if let Some(key) = &files.identity {
        problems.extend(regular_file(key).err());
    }
    if let ConfigSource::Inventory(config) = &files.config {
        problems.extend(regular_file(config).and_then(|()| includable(config)).err());
    }
    if let Some(known_hosts) = files.recording_known_hosts() {
        problems.extend(known_hosts_file(known_hosts, host_keys).err());
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// `file` must exist as a regular file (links followed).
///
/// # Arguments
///
/// * `file` - The file
///
/// # Errors
///
/// The problem otherwise.
fn regular_file(file: &LocalPath) -> Result<(), Problem> {
    match std::fs::metadata(&file.path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(problem(file, "is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(problem(file, "does not exist"))
        }
        Err(error) => Err(problem(file, &format!("cannot be read: {error}"))),
    }
}

/// An ssh_config file ssh will `Include`: see [`include_refusal`].
///
/// # Arguments
///
/// * `file` - The file (a regular file)
///
/// # Errors
///
/// The problem otherwise.
fn includable(file: &LocalPath) -> Result<(), Problem> {
    match include_refusal(&file.path) {
        Ok(None) => Ok(()),
        Ok(Some(IncludeRefusal::Writable)) => Err(problem(
            file,
            "is writable by its group or others, and OpenSSH refuses to include such a file \
             (chmod go-w)",
        )),
        Ok(Some(IncludeRefusal::Owner)) => Err(problem(
            file,
            "belongs to another user, and OpenSSH refuses to include such a file",
        )),
        Err(error) => Err(problem(file, &format!("cannot be read: {error}"))),
    }
}

/// Why ssh refuses to `Include` a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(unix),
    expect(dead_code, reason = "apvm reads Unix modes only, not Windows ACLs")
)]
enum IncludeRefusal {
    /// Its group or others may write it.
    Writable,
    /// It belongs to neither this user nor root.
    Owner,
}

/// Why ssh would refuse to `Include` the file at `path`, if it would:
/// OpenSSH's rule for included files (`readconf.c`), not writable by group
/// or others and owned by this (real) user or root.
///
/// Debian's and Ubuntu's ssh (their `user-group-modes` patch) also include
/// a file its group may write when that group holds only the file's owner
/// (a user's private group, under their usual umask `002`; verified on
/// 9.6p1 Ubuntu and 10.0p2 Debian: `0664` included, `0666` refused). apvm
/// keeps upstream's rule: it cannot tell a patched ssh, and the stricter
/// rule only asks for `chmod go-w`.
///
/// # Arguments
///
/// * `path` - An existing file
///
/// # Errors
///
/// When its metadata cannot be read.
#[cfg(unix)]
fn include_refusal(path: &Path) -> std::io::Result<Option<IncludeRefusal>> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    let me = rustix::process::getuid().as_raw();
    Ok(refusal_of(metadata.mode(), metadata.uid(), me))
}

/// OpenSSH's rule for included files, on a file's `mode` and `owner` for
/// user `me`.
///
/// # Arguments
///
/// * `mode` - The file's mode bits
/// * `owner` - The file's owner
/// * `me` - This process's real user
#[cfg(unix)]
fn refusal_of(mode: u32, owner: u32, me: u32) -> Option<IncludeRefusal> {
    if mode & 0o022 != 0 {
        return Some(IncludeRefusal::Writable);
    }
    if owner != 0 && owner != me {
        return Some(IncludeRefusal::Owner);
    }
    None
}

/// Why ssh would refuse to `Include` a file: not checked outside Unix
/// (Win32-OpenSSH checks ACLs, which apvm does not read).
///
/// # Arguments
///
/// * `_path` - The file
///
/// # Errors
///
/// Never.
#[cfg(not(unix))]
fn include_refusal(_path: &Path) -> std::io::Result<Option<IncludeRefusal>> {
    Ok(None)
}

/// The files a call without `ssh_config_file` includes after apvm's
/// options: what ssh reads without `-F`, the user's `user`
/// (`~/.ssh/config`) and then the system-wide file ([`system_config`] of
/// `system`; see the module docs).
///
/// A file that exists but this user cannot read is left out, as ssh skips
/// it; a missing one stays (an `Include` of it is silent). The system file
/// must pass ssh's check for included files, which ssh alone never applies
/// to it; the user's file gets that check from ssh either way.
///
/// # Arguments
///
/// * `user` - `~/.ssh/config`
/// * `system` - The system-wide ssh_config in ssh's configuration
///   directory (`/etc/ssh/ssh_config`)
///
/// # Errors
///
/// The problem (located at `ssh_config_file`, the setting that avoids it)
/// when ssh would refuse to include the system file.
pub(crate) fn default_includes(user: &Path, system: &Path) -> Result<Vec<PathBuf>, Problem> {
    let system = system_config(system, vendor_config(system).as_deref());
    let mut includes = Vec::with_capacity(2);
    if !unreadable(user) {
        includes.push(user.to_path_buf());
    }
    if !unreadable(&system) {
        system_includable(&system)?;
        includes.push(system);
    }
    Ok(includes)
}

/// The system-wide ssh_config ssh reads: `system`, unless it does not
/// exist (a dangling link counts as missing) while `vendor` does. openSUSE
/// Tumbleweed ships it as `/usr/etc/ssh/ssh_config`, with none in
/// `/etc/ssh`, and its ssh reads that copy only then: an
/// existing `/etc/ssh/ssh_config`, even an unreadable one, is read instead
/// (verified with OpenSSH 10.5p1). Without it, apvm's `Include` of the
/// missing `/etc` file would be silent and leave out the whole system
/// configuration, `/etc/ssh/ssh_config.d/*.conf` included.
///
/// # Arguments
///
/// * `system` - The file in ssh's configuration directory
/// * `vendor` - Its vendor copy ([`vendor_config`]), if it has one
fn system_config(system: &Path, vendor: Option<&Path>) -> PathBuf {
    let exists = |path: &Path| std::fs::metadata(path).is_ok();
    match vendor {
        Some(vendor) if !exists(system) && exists(vendor) => vendor.to_path_buf(),
        _ => system.to_path_buf(),
    }
}

/// The vendor copy of the system file `system`, for the one system file a
/// distribution's ssh is known to fall back from: openSUSE's
/// `/usr/etc/ssh/ssh_config` for `/etc/ssh/ssh_config` (the only vendor
/// path in the ssh binaries of Debian, Ubuntu, Fedora, AlmaLinux 8 and 9,
/// Alpine, Arch, openSUSE, nixpkgs and macOS, audit 5); none for another
/// build's directory (Homebrew's, FreeBSD's ports), which no ssh reads a
/// copy of.
///
/// # Arguments
///
/// * `system` - The file in ssh's configuration directory
fn vendor_config(system: &Path) -> Option<PathBuf> {
    (system == Path::new("/etc/ssh/ssh_config")).then(|| PathBuf::from("/usr/etc/ssh/ssh_config"))
}

/// The system ssh_config must be one ssh agrees to include.
///
/// # Arguments
///
/// * `system` - The system-wide ssh_config (it may not exist)
///
/// # Errors
///
/// The problem when ssh would refuse it.
fn system_includable(system: &Path) -> Result<(), Problem> {
    let why = match include_refusal(system) {
        Ok(Some(IncludeRefusal::Writable)) => "is writable by its group or others",
        Ok(Some(IncludeRefusal::Owner)) => "belongs to another user",
        // Missing (or gone): the `Include` is silent.
        Ok(None) | Err(_) => return Ok(()),
    };
    Err(Problem::new(
        SSH_CONFIG,
        format!(
            "is not set, and the system ssh_config {} {why}: ssh reads that file unchecked on \
             its own, but refuses to include it, as apvm must (set ssh_config_file to a file of \
             your own, or have that one belong to root and not be writable by its group or \
             others)",
            quoted(&system.to_string_lossy())
        ),
    ))
}

/// Whether `path` exists but this process may not read it (Unix
/// `access(R_OK)`, which fails as ssh's `fopen` would); never outside Unix.
///
/// # Arguments
///
/// * `path` - A default ssh_config file
#[cfg(unix)]
fn unreadable(path: &Path) -> bool {
    match rustix::fs::access(path, rustix::fs::Access::READ_OK) {
        Ok(()) => false,
        Err(error) => error != rustix::io::Errno::NOENT && error != rustix::io::Errno::NOTDIR,
    }
}

/// Whether `path` exists but may not be read: not checked outside Unix.
///
/// # Arguments
///
/// * `_path` - A default ssh_config file
#[cfg(not(unix))]
fn unreadable(_path: &Path) -> bool {
    false
}

/// The known_hosts file new keys go to: a regular file, or none yet; and
/// writable for `accept-new`.
///
/// # Arguments
///
/// * `file` - The file
/// * `host_keys` - Whether new host keys are recorded
///
/// # Errors
///
/// The problem otherwise.
fn known_hosts_file(file: &LocalPath, host_keys: HostKeyPolicy) -> Result<(), Problem> {
    let recording = host_keys == HostKeyPolicy::AcceptNew;
    match std::fs::metadata(&file.path) {
        Ok(metadata) if !metadata.is_file() => Err(problem(
            file,
            "is not a regular file, so host keys would not be kept and any key would pass",
        )),
        Ok(_) if recording => writable(file, &file.path, "is not writable"),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !recording {
                return Ok(());
            }
            new_file_directory(file)
        }
        Err(error) => Err(problem(file, &format!("cannot be read: {error}"))),
    }
}

/// For `accept-new` with a known_hosts file that does not exist yet: its
/// directory must exist and be writable, or ssh could not create it.
///
/// # Arguments
///
/// * `file` - The file
///
/// # Errors
///
/// The problem otherwise.
fn new_file_directory(file: &LocalPath) -> Result<(), Problem> {
    let Some(directory) = file.path.parent() else {
        return Err(problem(file, "has no directory"));
    };
    match std::fs::metadata(directory) {
        Ok(metadata) if metadata.is_dir() => writable(
            file,
            directory,
            "does not exist yet, and its directory is not writable",
        ),
        // ssh 8.4+ would create `~/.ssh` (not another directory) itself;
        // older versions fail to add the key and run the command anyway.
        _ => Err(problem(
            file,
            &format!(
                "does not exist yet, and neither does its directory {}, so a new host key \
                 could not be kept (create the directory, mode 0700, or set known_hosts_file \
                 to a file in an existing one)",
                quoted(&directory.to_string_lossy())
            ),
        )),
    }
}

/// `path` must be writable by this process (Unix `access(W_OK)`, what ssh
/// will try); not checked elsewhere.
///
/// # Arguments
///
/// * `file` - The field's file, for messages
/// * `path` - The file or directory to check
/// * `reason` - The message when it is not writable
///
/// # Errors
///
/// The problem when it is not.
#[cfg(unix)]
fn writable(file: &LocalPath, path: &Path, reason: &str) -> Result<(), Problem> {
    rustix::fs::access(path, rustix::fs::Access::WRITE_OK).map_err(|_| {
        problem(
            file,
            &format!("{reason}, so a new host key could not be kept"),
        )
    })
}

/// `path` must be writable: not checked outside Unix.
///
/// # Arguments
///
/// * `_file` - The field's file
/// * `_path` - The file or directory
/// * `_reason` - The message
#[cfg(not(unix))]
fn writable(_file: &LocalPath, _path: &Path, _reason: &str) -> Result<(), Problem> {
    Ok(())
}

/// A problem about `file`: `<field>: '<path>' <reason>`.
///
/// # Arguments
///
/// * `file` - The file
/// * `reason` - What is wrong
fn problem(file: &LocalPath, reason: &str) -> Problem {
    Problem::new(
        file.field,
        format!("{} {reason}", quoted(&file.path.to_string_lossy())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{Inventory, Server, SshSettings};
    use crate::{Defaults, ResolvedServer};

    /// The effective settings of a server with `ssh`, under `defaults`.
    fn resolved(defaults: SshSettings, ssh: SshSettings) -> ResolvedServer {
        let mut inventory = Inventory::new();
        inventory.set_defaults(Defaults { ssh: defaults }).unwrap();
        let mut server = Server::new("s", "example.com");
        server.ssh = ssh;
        inventory.add_server(server).unwrap();
        inventory.resolve_server("s").unwrap()
    }

    /// Settings with only `known_hosts_file`, `ssh_config_file` and
    /// `identity_file`.
    fn files(known: Option<&str>, config: Option<&str>, key: Option<&str>) -> SshSettings {
        SshSettings {
            known_hosts_file: known.map(Into::into),
            ssh_config_file: config.map(Into::into),
            identity_file: key.map(Into::into),
            ..SshSettings::default()
        }
    }

    #[test]
    fn unset_fields_fall_back_to_what_ssh_reads_under_home() {
        let server = resolved(SshSettings::default(), SshSettings::default());
        let local =
            LocalFiles::resolve(&server.ssh, Some(Path::new("/h")), Path::new("/cwd")).unwrap();
        assert_eq!(local.program.path, PathBuf::from("ssh"));
        assert_eq!(local.identity, None);
        let known: Vec<_> = local
            .known_hosts
            .iter()
            .map(|f| (f.field, f.path.clone()))
            .collect();
        assert_eq!(
            known,
            [
                ("known_hosts_file", PathBuf::from("/h/.ssh/known_hosts")),
                ("known_hosts_file", PathBuf::from("/h/.ssh/known_hosts2")),
            ]
        );
        assert_eq!(
            local.config,
            ConfigSource::Default {
                user: PathBuf::from("/h/.ssh/config")
            }
        );
    }

    #[test]
    fn tilde_is_the_given_home_and_relative_paths_the_current_directory() {
        let defaults = SshSettings {
            binary: Some("~/bin/ssh".into()),
            ..files(Some("~/kh"), Some("conf/ssh_config"), Some("keys/id"))
        };
        let server = resolved(defaults, files(None, None, Some("~\\win\\id")));
        let local =
            LocalFiles::resolve(&server.ssh, Some(Path::new("/h")), Path::new("/cwd")).unwrap();
        assert_eq!(local.program.path, PathBuf::from("/h/bin/ssh"));
        // The server's key wins over the default one.
        assert_eq!(
            local.identity.unwrap().path,
            PathBuf::from("/h").join("win\\id")
        );
        assert_eq!(local.known_hosts.len(), 1);
        assert_eq!(local.known_hosts[0].path, PathBuf::from("/h/kh"));
        assert_eq!(
            local.config,
            ConfigSource::Inventory(LocalPath {
                field: "ssh_config_file",
                path: PathBuf::from("/cwd/conf/ssh_config")
            })
        );
        let relative = resolved(
            SshSettings {
                binary: Some("bin/ssh".into()),
                ..SshSettings::default()
            },
            SshSettings::default(),
        );
        let local =
            LocalFiles::resolve(&relative.ssh, Some(Path::new("/h")), Path::new("/cwd")).unwrap();
        assert_eq!(local.program.path, PathBuf::from("/cwd/bin/ssh"));
    }

    #[test]
    fn without_a_home_every_path_that_needs_one_is_a_located_problem() {
        let server = resolved(
            SshSettings {
                binary: Some("~/ssh".into()),
                ..files(None, None, Some("~/id"))
            },
            SshSettings::default(),
        );
        let problems = LocalFiles::resolve(&server.ssh, None, Path::new("/cwd")).unwrap_err();
        let fields: Vec<_> = problems.iter().map(|p| p.location.as_str()).collect();
        assert_eq!(
            fields,
            [
                "binary",
                "identity_file",
                "known_hosts_file",
                "ssh_config_file"
            ]
        );
        assert!(
            problems[0]
                .to_string()
                .contains("starts with '~', but no home directory")
        );
        assert!(
            problems[2]
                .to_string()
                .contains("no home directory is known to find ~/.ssh/known_hosts")
        );
        // Absolute settings need no home.
        let server = resolved(
            files(Some("/k"), Some("/c"), Some("/i")),
            SshSettings::default(),
        );
        assert!(LocalFiles::resolve(&server.ssh, None, Path::new("/cwd")).is_ok());
    }

    /// A `LocalPath` for `field` at `path`.
    fn at(field: &'static str, path: PathBuf) -> LocalPath {
        LocalPath { field, path }
    }

    /// Files that name only `known_hosts` (and a regular key and config).
    fn with_known_hosts(dir: &Path, known_hosts: PathBuf) -> LocalFiles {
        std::fs::write(dir.join("id"), "key").unwrap();
        std::fs::write(dir.join("config"), "").unwrap();
        // ssh's rule for included files, whatever the umask.
        #[cfg(unix)]
        std::fs::set_permissions(
            dir.join("config"),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        LocalFiles {
            program: at("binary", "ssh".into()),
            identity: Some(at("identity_file", dir.join("id"))),
            known_hosts: vec![at("known_hosts_file", known_hosts)],
            config: ConfigSource::Inventory(at("ssh_config_file", dir.join("config"))),
        }
    }

    /// The single problem `check` reports, as text.
    fn refusal(files: &LocalFiles, host_keys: HostKeyPolicy) -> String {
        let problems = check(files, host_keys).unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        problems[0].to_string()
    }

    #[test]
    fn a_missing_key_or_config_file_is_refused_a_missing_known_hosts_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut local = with_known_hosts(dir.path(), dir.path().join("kh"));
        assert_eq!(check(&local, HostKeyPolicy::Strict), Ok(()));
        local.identity = Some(at("identity_file", dir.path().join("nope")));
        local.config = ConfigSource::Inventory(at("ssh_config_file", dir.path().to_path_buf()));
        let problems = check(&local, HostKeyPolicy::Strict).unwrap_err();
        let text: Vec<_> = problems.iter().map(ToString::to_string).collect();
        assert_eq!(text.len(), 2);
        assert!(text[0].starts_with("identity_file: '") && text[0].ends_with("' does not exist"));
        assert!(
            text[1].starts_with("ssh_config_file: '")
                && text[1].ends_with("' is not a regular file")
        );
        // The default user config may be missing, as for ssh.
        local = with_known_hosts(dir.path(), dir.path().join("kh"));
        local.config = ConfigSource::Default {
            user: dir.path().join("missing"),
        };
        assert_eq!(check(&local, HostKeyPolicy::AcceptNew), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn an_ssh_config_file_others_can_write_is_refused_as_ssh_would() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let local = with_known_hosts(dir.path(), dir.path().join("kh"));
        let config = dir.path().join("config");
        for (mode, refused) in [(0o600, false), (0o644, false), (0o664, true), (0o646, true)] {
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(mode)).unwrap();
            let result = check(&local, HostKeyPolicy::Strict);
            if refused {
                let message = refusal(&local, HostKeyPolicy::Strict);
                assert!(message.starts_with("ssh_config_file: '"), "{message}");
                assert!(
                    message.ends_with(
                        "is writable by its group or others, and OpenSSH refuses to include such \
                         a file (chmod go-w)"
                    ),
                    "{message}"
                );
            } else {
                assert_eq!(result, Ok(()), "{mode:o}");
            }
        }
        // The user's default config is ssh's own business, as without apvm.
        std::fs::write(dir.path().join("user_config"), "").unwrap();
        std::fs::set_permissions(
            dir.path().join("user_config"),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        let local = LocalFiles {
            config: ConfigSource::Default {
                user: dir.path().join("user_config"),
            },
            ..local
        };
        assert_eq!(check(&local, HostKeyPolicy::Strict), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn ssh_includes_only_files_of_this_user_or_root_that_others_cannot_write() {
        // OpenSSH's `readconf.c` rule: `(uid != 0 && uid != getuid()) ||
        // (mode & 022) != 0`.
        let me = 501;
        for (mode, owner, refusal) in [
            (0o100_644, me, None),
            (0o100_600, 0, None),
            (0o100_444, me, None),
            (0o100_664, me, Some(IncludeRefusal::Writable)),
            (0o100_646, 0, Some(IncludeRefusal::Writable)),
            (0o100_620, me, Some(IncludeRefusal::Writable)),
            (0o100_644, 502, Some(IncludeRefusal::Owner)),
            // Writable wins, as in ssh's message.
            (0o100_666, 502, Some(IncludeRefusal::Writable)),
        ] {
            assert_eq!(refusal_of(mode, owner, me), refusal, "{mode:o} {owner}");
        }
    }

    /// A user and a system ssh_config in a new directory, mode `0600`.
    #[cfg(unix)]
    fn default_files() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let (user, system) = (dir.path().join("config"), dir.path().join("ssh_config"));
        for file in [&user, &system] {
            std::fs::write(file, "").unwrap();
            std::fs::set_permissions(file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .unwrap();
        }
        (dir, user, system)
    }

    #[cfg(unix)]
    #[test]
    fn the_default_files_are_included_when_ssh_would_read_them() {
        let (dir, user, system) = default_files();
        assert_eq!(
            default_includes(&user, &system),
            Ok(vec![user.clone(), system.clone()])
        );
        // Missing files stay: an `Include` of one is silent, as ssh is.
        let (no_user, no_system) = (dir.path().join("no_config"), dir.path().join("no_sys"));
        assert_eq!(
            default_includes(&no_user, &no_system),
            Ok(vec![no_user, no_system])
        );
        // A component that is a file: ssh finds nothing there either.
        let under_file = user.join("x");
        assert_eq!(
            default_includes(&under_file, &system),
            Ok(vec![under_file, system])
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_default_file_this_user_cannot_read_is_left_out_as_ssh_skips_it() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return; // root reads anyway, as ssh would
        }
        let (_dir, user, system) = default_files();
        std::fs::set_permissions(&user, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(default_includes(&user, &system), Ok(vec![system.clone()]));
        std::fs::set_permissions(&system, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(default_includes(&user, &system), Ok(vec![]));
        std::fs::set_permissions(&user, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(default_includes(&user, &system), Ok(vec![user]));
    }

    #[cfg(unix)]
    #[test]
    fn a_system_file_ssh_would_refuse_to_include_is_refused_first() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, user, system) = default_files();
        std::fs::set_permissions(&system, std::fs::Permissions::from_mode(0o664)).unwrap();
        let problem = default_includes(&user, &system).unwrap_err();
        assert_eq!(problem.location, "ssh_config_file");
        assert_eq!(
            problem.to_string(),
            format!(
                "ssh_config_file: is not set, and the system ssh_config '{}' is writable by its \
                 group or others: ssh reads that file unchecked on its own, but refuses to \
                 include it, as apvm must (set ssh_config_file to a file of your own, or have \
                 that one belong to root and not be writable by its group or others)",
                system.display()
            )
        );
        // The user's own file is ssh's business: it checks it either way.
        std::fs::set_permissions(&system, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&user, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(default_includes(&user, &system), Ok(vec![user, system]));
    }

    #[test]
    fn only_etc_ssh_ssh_config_has_a_vendor_copy() {
        assert_eq!(
            vendor_config(Path::new("/etc/ssh/ssh_config")),
            Some(PathBuf::from("/usr/etc/ssh/ssh_config"))
        );
        // Until audit 5, other builds' directories got a path under /usr
        // too (`/usr/opt/homebrew/etc/ssh/ssh_config`) that no ssh reads.
        for none in [
            "/opt/homebrew/etc/ssh/ssh_config",
            "/usr/local/etc/ssh/ssh_config",
            "/usr/etc/ssh/ssh_config",
            "/usr",
            "etc/ssh/ssh_config",
            "",
        ] {
            assert_eq!(vendor_config(Path::new(none)), None, "{none:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_vendor_system_file_is_read_only_when_the_etc_one_does_not_exist() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _user, system) = default_files();
        let vendor = dir.path().join("vendor_ssh_config");
        let missing = dir.path().join("missing_ssh_config");
        // openSUSE: no /etc/ssh/ssh_config, so ssh reads /usr/etc's.
        std::fs::write(&vendor, "").unwrap();
        assert_eq!(system_config(&missing, Some(&vendor)), vendor);
        // A dangling link is missing too (verified).
        let dangling = dir.path().join("dangling_ssh_config");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &dangling).unwrap();
        assert_eq!(system_config(&dangling, Some(&vendor)), vendor);
        // An /etc file wins, even one this user cannot read (verified: ssh
        // then reads neither).
        assert_eq!(system_config(&system, Some(&vendor)), system);
        std::fs::set_permissions(&system, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(system_config(&system, Some(&vendor)), system);
        // Neither exists, or no vendor copy: the /etc path (a silent Include).
        let no_vendor = dir.path().join("no_vendor");
        assert_eq!(system_config(&missing, Some(&no_vendor)), missing);
        assert_eq!(system_config(&missing, None), missing);
    }

    #[cfg(unix)]
    #[test]
    fn a_known_hosts_that_keeps_nothing_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("null_kh");
        std::os::unix::fs::symlink("/dev/null", &link).unwrap();
        for host_keys in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew] {
            let local = with_known_hosts(dir.path(), link.clone());
            let message = refusal(&local, host_keys);
            assert!(message.starts_with("known_hosts_file: '"), "{message}");
            assert!(
                message.contains("is not a regular file, so host keys would not be kept"),
                "{message}"
            );
            // A directory too.
            let local = with_known_hosts(dir.path(), dir.path().to_path_buf());
            assert!(refusal(&local, host_keys).contains("is not a regular file"));
        }
        // A link to a regular file is fine.
        std::fs::write(dir.path().join("real"), "").unwrap();
        let link = dir.path().join("link_kh");
        std::os::unix::fs::symlink(dir.path().join("real"), &link).unwrap();
        let local = with_known_hosts(dir.path(), link);
        assert_eq!(check(&local, HostKeyPolicy::AcceptNew), Ok(()));
    }

    #[test]
    fn accept_new_needs_somewhere_to_keep_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let missing_dir = dir.path().join("nodir").join("kh");
        let local = with_known_hosts(dir.path(), missing_dir);
        assert_eq!(check(&local, HostKeyPolicy::Strict), Ok(()));
        let message = refusal(&local, HostKeyPolicy::AcceptNew);
        assert!(
            message.contains(&format!(
                "neither does its directory '{}', so a new host key could not be kept (create \
                 the directory, mode 0700, or set known_hosts_file",
                dir.path().join("nodir").display()
            )),
            "{message}"
        );
        // A new file in an existing directory, or an existing file: fine.
        let local = with_known_hosts(dir.path(), dir.path().join("new_kh"));
        assert_eq!(check(&local, HostKeyPolicy::AcceptNew), Ok(()));
        std::fs::write(dir.path().join("new_kh"), "").unwrap();
        assert_eq!(check(&local, HostKeyPolicy::AcceptNew), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn accept_new_refuses_a_read_only_known_hosts_or_directory() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return; // root writes anyway, as ssh would
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ro_kh");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
        let local = with_known_hosts(dir.path(), file);
        assert_eq!(check(&local, HostKeyPolicy::Strict), Ok(()));
        assert!(refusal(&local, HostKeyPolicy::AcceptNew).contains("is not writable"));

        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let local = with_known_hosts(dir.path(), locked.join("kh"));
        let message = refusal(&local, HostKeyPolicy::AcceptNew);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            message.contains("its directory is not writable"),
            "{message}"
        );
    }
}
