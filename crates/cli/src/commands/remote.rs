//! Connecting to the inventory's servers and sites (plan P4.3): `apvm
//! server test`, `apvm site test`, and the test `server add` / `site add`
//! run before saving.
//!
//! - [`Remote`]: what a command asks of a server or site (a server's
//!   login check, a site's full read-only probe), injected so whole
//!   commands run in unit tests without SSH. [`SshRemote`] is the real
//!   one: system OpenSSH through `apvm-sites`, one transport per server
//!   (shared connections in `~/.apvm/ssh-mux`, at most
//!   [`ServerLimits::DEFAULT_PER_SERVER`] calls per server at once).
//! - [`check`]: what a test found, as the CLI shows it ([`ServerCheck`],
//!   [`SiteCheck`]), with the host keys ssh trusted on the way.
//! - [`remedy`]: what the user can do about a failure, by code and by the
//!   line ssh printed (plan §3.5 "Classification table").
//! - [`render`]: the reports, as text and as `--json` documents.
//! - [`bounded`]: runs tests concurrently, at most `jobs` at a time, results
//!   in the order asked.
//!
//! Nothing here changes a server: the server check runs `id` and
//! `command -v` as the login user, the site probe is read-only (plan
//! §6.2).

pub mod check;
mod notices;
pub mod remedy;
pub mod render;
#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use apvm_sites::remote::{HostKeyPolicy, ServerLimits, SshOptions, SshTransport};
use apvm_sites::wp::probe::probe_server;
use apvm_sites::{EntityKind, INVENTORY_VERSION, Inventory, ResolvedServer, ResolvedSite, Session};

use super::inventory::{Console, Failure, count};

pub use check::{ServerCheck, SiteCheck};
use notices::Noticing;

/// How long a server's test may run once connected: it runs `id` and
/// `command -v` only. The connection's own time is added on top
/// ([`server_test_timeout`]).
pub const SERVER_TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How many tests run at once unless `-j` says otherwise (plan §5.1).
pub const DEFAULT_JOBS: usize = 4;

/// A future a [`Remote`] returns: it may borrow the remote and its
/// arguments, and need not be `Send` (the CLI runs on one thread).
pub type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// What the commands that connect ask of the servers.
pub trait Remote {
    /// Connect to `server` and read its login user and tools.
    ///
    /// # Arguments
    ///
    /// * `server` - The server, with its effective SSH settings
    /// * `keys` - What ssh does with an unknown host key
    fn test_server<'a>(
        &'a self,
        server: &'a ResolvedServer,
        keys: HostKeyPolicy,
    ) -> LocalFuture<'a, ServerCheck>;

    /// Probe `site` (read-only, plan §6.2).
    ///
    /// # Arguments
    ///
    /// * `site` - The site, with its effective settings
    /// * `keys` - What ssh does with an unknown host key
    fn test_site<'a>(
        &'a self,
        site: &'a ResolvedSite,
        keys: HostKeyPolicy,
    ) -> LocalFuture<'a, SiteCheck>;
}

/// The real [`Remote`]: system OpenSSH, through `apvm-sites`.
pub struct SshRemote {
    /// Where shared connections' sockets go (`~/.apvm/ssh-mux`).
    control_dir: PathBuf,
    /// The calls each server may run at once, shared by every transport.
    limits: ServerLimits,
    /// One transport per server and host key policy, so the sites of a
    /// server share its connection and its `ssh -G` answers.
    transports: Mutex<HashMap<(String, bool), Arc<SshTransport>>>,
}

impl SshRemote {
    /// A remote whose shared connections live in `control_dir` (or, when
    /// that path is too long for a socket, `/tmp/apvm-mux-<uid>`).
    ///
    /// # Arguments
    ///
    /// * `control_dir` - The control directory (`~/.apvm/ssh-mux`)
    pub fn new(control_dir: PathBuf) -> Self {
        Self {
            control_dir,
            limits: ServerLimits::default(),
            transports: Mutex::new(HashMap::new()),
        }
    }

    /// The transport to `server` for `keys`, made once.
    ///
    /// # Arguments
    ///
    /// * `server` - The server
    /// * `keys` - What ssh does with an unknown host key
    fn transport(&self, server: &ResolvedServer, keys: HostKeyPolicy) -> Arc<SshTransport> {
        let key = (server.id.clone(), keys == HostKeyPolicy::AcceptNew);
        // A poisoned lock only means another test panicked while holding
        // it; the map itself is still whole.
        let mut transports = self
            .transports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let transport = transports.entry(key).or_insert_with(|| {
            let options = SshOptions::new()
                .with_host_keys(keys)
                .with_control_dir(&self.control_dir)
                .with_tmp_control_fallback()
                .with_server_limits(self.limits.clone());
            Arc::new(SshTransport::new(server.clone(), options))
        });
        Arc::clone(transport)
    }
}

impl Remote for SshRemote {
    fn test_server<'a>(
        &'a self,
        server: &'a ResolvedServer,
        keys: HostKeyPolicy,
    ) -> LocalFuture<'a, ServerCheck> {
        Box::pin(async move {
            let transport = self.transport(server, keys);
            let noticing = Noticing::new(transport.as_ref());
            let outcome = probe_server(&noticing, server_test_timeout(server)).await;
            ServerCheck {
                host_keys: noticing.notices(),
                outcome: outcome.map(|facts| check::Login::of(&facts)),
            }
        })
    }

    fn test_site<'a>(
        &'a self,
        site: &'a ResolvedSite,
        keys: HostKeyPolicy,
    ) -> LocalFuture<'a, SiteCheck> {
        Box::pin(async move {
            let transport = self.transport(&site.server, keys);
            let noticing = Noticing::new(transport.as_ref());
            let outcome = Session::open(&noticing, site)
                .await
                .map(|session| check::SiteSummary::of(session.probe()));
            SiteCheck {
                host_keys: noticing.notices(),
                outcome,
            }
        })
    }
}

/// The whole time a server's test may take: [`SERVER_TEST_TIMEOUT`] plus
/// the connect timeout of the server and of each jump host. A call that
/// does not share a connection (jump hosts, `--no-multiplex`, Windows)
/// connects inside its own timeout, so a slow but allowed connection must
/// end as ssh's `ssh_timeout`, not as the test's `timeout`.
///
/// # Arguments
///
/// * `server` - The server, with its effective SSH settings
pub fn server_test_timeout(server: &ResolvedServer) -> Duration {
    let hops = server
        .ssh
        .proxy_jump
        .as_ref()
        .map_or(0, |jump| jump.value.split(',').count());
    let connect = u64::from(server.ssh.connect_timeout_secs.value);
    let connects = u64::try_from(hops + 1).unwrap_or(u64::MAX);
    SERVER_TEST_TIMEOUT.saturating_add(Duration::from_secs(connect.saturating_mul(connects)))
}

/// The host key policy `--trust-new-host-key` asks for.
///
/// # Arguments
///
/// * `trust_new` - Whether the flag was passed
pub fn host_keys(trust_new: bool) -> HostKeyPolicy {
    if trust_new {
        HostKeyPolicy::AcceptNew
    } else {
        HostKeyPolicy::Strict
    }
}

/// `inventory`, when this apvm may change it: an `add` refuses a
/// read-only (newer) inventory before it connects anywhere, as saving
/// would.
///
/// # Arguments
///
/// * `inventory` - The inventory as loaded
///
/// # Errors
///
/// The library's `inventory_read_only` refusal.
pub fn writable(inventory: Inventory) -> Result<Inventory, Failure> {
    if !inventory.is_read_only() {
        return Ok(inventory);
    }
    Err(Failure::Sites(apvm_sites::Error::InventoryReadOnly {
        found: inventory.version(),
        supported: INVENTORY_VERSION,
    }))
}

/// After `add` tested a new record: go on when the test passed; when it
/// failed (`failed`), ask whether to save the record anyway.
///
/// # Arguments
///
/// * `console` - Where to ask
/// * `failed` - Why the test failed, if it did
/// * `kind` - What is being added
/// * `id` - Its id
///
/// # Errors
///
/// [`Failure::TestFailed`] (with the test's code) when the answer is no,
/// or when stdin is not a terminal to ask on; [`Failure::Prompt`] when
/// the prompt fails.
pub fn save_anyway(
    console: &mut Console<'_>,
    failed: Option<&apvm_sites::Error>,
    kind: EntityKind,
    id: &str,
) -> Result<(), Failure> {
    let Some(error) = failed else {
        return Ok(());
    };
    let code = error.code().as_str();
    let question = format!("The test failed ({code}). Save {kind} '{id}' anyway? [y/N]");
    let why = match (console.ask)(&question).map_err(Failure::Prompt)? {
        Some(true) => {
            console.warn(&format!(
                "saving {kind} '{id}' although its test failed ({code})"
            ));
            return Ok(());
        }
        Some(false) => "",
        None => ", and stdin is not a terminal to ask whether to save it anyway",
    };
    Err(Failure::TestFailed {
        message: format!(
            "{kind} '{id}' was not added: its test failed ({code}){why}\n  fix the problem and \
             add it again, or pass --no-test to save it without testing"
        ),
        code,
    })
}

/// [`Failure::TestFailed`] when any test failed: `<kind> '<id>' failed its
/// test (<code>)` for a single test, `N of M <kind>s failed the test` for
/// several, with the first failure's code (plan §4.5).
///
/// # Arguments
///
/// * `kind` - What was tested
/// * `failures` - The id and error of each failed test, in report order
/// * `total` - How many tests ran
///
/// # Errors
///
/// [`Failure::TestFailed`] when `failures` is not empty.
pub fn failed_tests(
    kind: EntityKind,
    failures: &[(&str, &apvm_sites::Error)],
    total: usize,
) -> Result<(), Failure> {
    let Some((id, error)) = failures.first() else {
        return Ok(());
    };
    let code = error.code().as_str();
    let message = if total == 1 {
        format!("{kind} '{id}' failed its test ({code})")
    } else {
        format!(
            "{} of {} failed the test",
            failures.len(),
            count(total, kind.as_str())
        )
    };
    Err(Failure::TestFailed { message, code })
}

/// Run `tasks`, at most `jobs` at a time, and return their results in the
/// order of `tasks` (whatever order they finish in).
///
/// # Arguments
///
/// * `tasks` - The futures to run
/// * `jobs` - How many may run at once
pub async fn bounded<'a, T>(tasks: Vec<LocalFuture<'a, T>>, jobs: NonZeroUsize) -> Vec<T> {
    let mut done: Vec<Option<T>> = tasks.iter().map(|_| None).collect();
    let mut queue = tasks.into_iter().enumerate();
    let mut running: Vec<(usize, LocalFuture<'a, T>)> = Vec::new();
    loop {
        while running.len() < jobs.get() {
            match queue.next() {
                Some(task) => running.push(task),
                None => break,
            }
        }
        if running.is_empty() {
            break;
        }
        std::future::poll_fn(|cx| {
            let mut finished = false;
            let mut index = 0;
            while index < running.len() {
                match running[index].1.as_mut().poll(cx) {
                    Poll::Ready(value) => {
                        let (position, _) = running.swap_remove(index);
                        done[position] = Some(value);
                        finished = true;
                    }
                    Poll::Pending => index += 1,
                }
            }
            if finished {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
    // Every task ran to its end, so every slot is filled.
    done.into_iter().flatten().collect()
}
