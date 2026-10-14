//! What `ssh -G` resolves for a shared call's configuration (P3.4, audit
//! rounds 3 and 4): whether a proxy is in the way, whether the user's
//! ssh_config resolves a call and a master alike, and a master's expanded
//! control path.
//!
//! `-G` runs twice, as ssh would read the configuration for a call (with
//! its remote command, `sh -s`) and for a master's starter (`-N`): OpenSSH
//! 10.0 added `Match sessiontype` and `Match command`, so an ssh_config can
//! send a call through a jump host, or as another user or to another port,
//! that a plain `-G` and the starter never see (verified on 10.3p1). An
//! upload's remote command (`sh -c '…'`) is not resolved: a `Match command`
//! that tells uploads apart is not detected (documented).

use std::path::PathBuf;
use std::time::Duration;

use super::command::{Commands, run};
use crate::problem::printable;

/// How long `ssh -G` may take: it reads files and opens no connection
/// (though `Match exec` may run a command of the user's).
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// The remote command calls run (stdin delivery), as `-G` is given it.
const CALL_COMMAND: &str = "sh -s";

/// What decides where ssh connects, as `-G` resolved it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Resolved {
    /// `hostname`, `user`, `port` and `controlpath`, as printed.
    destination: Vec<String>,
    /// Whether `proxyjump` or `proxycommand` is set.
    proxied: bool,
    /// The expanded `controlpath`, unless `none`.
    control_path: Option<PathBuf>,
}

/// The verdict and, when connections can be shared, the control socket a
/// call uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Resolution {
    /// Whether connections can be shared.
    pub(super) verdict: Verdict,
    /// The expanded control path a call resolves to.
    pub(super) control_path: Option<PathBuf>,
}

/// Why connections cannot be shared, as the two resolutions tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Nothing in the way.
    Shareable,
    /// A jump host or a ProxyCommand.
    Proxied,
    /// A call and a master resolve to different destinations.
    SessionDependent,
}

/// Resolve the configuration for a call and for a master, and say whether
/// connections can be shared.
///
/// # Arguments
///
/// * `commands` - The commands, with the file to resolve
///
/// # Errors
///
/// Why `ssh -G` could not tell, escaped.
pub(super) async fn verdict(commands: &Commands<'_>) -> Result<Resolution, String> {
    let call = resolve(commands, &[], &[CALL_COMMAND]).await?;
    let master = resolve(commands, &["-N"], &[]).await?;
    Ok(Resolution {
        verdict: judge(&call, &master),
        control_path: call.control_path,
    })
}

/// The verdict for a call's and a master's resolutions.
///
/// # Arguments
///
/// * `call` - As ssh resolves a call
/// * `master` - As ssh resolves a master's starter
fn judge(call: &Resolved, master: &Resolved) -> Verdict {
    if call.proxied || master.proxied {
        return Verdict::Proxied;
    }
    if call.destination != master.destination {
        return Verdict::SessionDependent;
    }
    Verdict::Shareable
}

/// `ssh -G` with `before` and `after` around the destination.
///
/// # Arguments
///
/// * `commands` - The commands, with the file to resolve
/// * `before` - Options before the destination
/// * `after` - A remote command
///
/// # Errors
///
/// Why `ssh -G` failed, escaped.
async fn resolve(
    commands: &Commands<'_>,
    before: &[&str],
    after: &[&str],
) -> Result<Resolved, String> {
    let options: Vec<&str> = before.iter().copied().chain(["-G"]).collect();
    let query = run(
        commands.command(commands.file, &options, after),
        QUERY_TIMEOUT,
    )
    .await;
    if !query.succeeded() {
        let said = String::from_utf8_lossy(&query.stderr);
        let line = said
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("Pseudo-terminal"));
        return Err(line.map_or_else(
            || "'ssh -G' failed".to_string(),
            |line| format!("'ssh -G' failed: {}", printable(line)),
        ));
    }
    Ok(parse(&String::from_utf8_lossy(&query.stdout)))
}

/// What `ssh -G` output says (it prints `proxyjump` and `proxycommand`
/// only when set, verified on 10.3p1).
///
/// # Arguments
///
/// * `output` - `ssh -G` stdout
fn parse(output: &str) -> Resolved {
    let mut resolved = Resolved::default();
    for line in output.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "proxyjump" | "proxycommand" if !value.is_empty() && value != "none" => {
                resolved.proxied = true;
            }
            "hostname" | "user" | "port" => resolved.destination.push(line.to_string()),
            "controlpath" => {
                resolved.destination.push(line.to_string());
                resolved.control_path = (value != "none").then(|| PathBuf::from(value));
            }
            _ => {}
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-G` output for a destination.
    fn output(host: &str, extra: &str) -> String {
        format!("user deploy\nhostname {host}\nport 22\ncontrolpath /m/t0123\n{extra}")
    }

    #[test]
    fn a_proxy_in_either_resolution_turns_sharing_off() {
        let plain = parse(&output("h", ""));
        let jumped = parse(&output("h", "proxyjump deploy@hop\n"));
        let commanded = parse(&output("h", "proxycommand nc %h %p\n"));
        assert_eq!(judge(&plain, &plain), Verdict::Shareable);
        assert_eq!(judge(&jumped, &plain), Verdict::Proxied);
        assert_eq!(judge(&plain, &commanded), Verdict::Proxied);
        // `proxyusefdpass` is another key; `none` is no proxy.
        let other = parse(&output("h", "proxyusefdpass no\nproxycommand none\n"));
        assert_eq!(judge(&other, &plain), Verdict::Shareable);
    }

    #[test]
    fn a_call_resolved_elsewhere_than_its_master_turns_sharing_off() {
        // `Match command "sh -s"` sending calls to another host, user,
        // port or socket than the starter.
        let master = parse(&output("h", ""));
        for call in [
            output("other", ""),
            output("h", "").replace("user deploy", "user root"),
            output("h", "").replace("port 22", "port 2222"),
            output("h", "").replace("/m/t0123", "/m/t4567"),
        ] {
            assert_eq!(
                judge(&parse(&call), &master),
                Verdict::SessionDependent,
                "{call}"
            );
        }
    }

    #[test]
    fn the_control_path_is_read_from_ssh_g() {
        let read = |output: &str| parse(output).control_path;
        assert_eq!(
            read("user deploy\ncontrolpath /home/u/.apvm/ssh-mux/abc\ncontrolmaster false\n"),
            Some(PathBuf::from("/home/u/.apvm/ssh-mux/abc"))
        );
        // A `%` in the directory stays (ssh expanded `%%`).
        assert_eq!(read("controlpath /m%x/t\n"), Some(PathBuf::from("/m%x/t")));
        assert_eq!(read("controlpath none\n"), None);
        assert_eq!(read("user u\n"), None);
    }
}
