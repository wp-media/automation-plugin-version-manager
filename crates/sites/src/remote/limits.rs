//! How many SSH calls run at once per server (plan §6.1 item 10, P3.4).
//!
//! sshd refuses a connection's session past `MaxSessions` (10 by default),
//! and a multiplexed master is one connection for every call it serves.
//! [`ServerLimits`] lets at most [`ServerLimits::DEFAULT_PER_SERVER`] (4)
//! calls to one inventory server run at once; the others wait for a slot,
//! in the order they asked. The wait comes before the call starts, so it
//! does not count against the request's timeout, and it has no bound of
//! its own: a caller that must finish by a deadline sets one around the
//! whole run (a slot is freed when its call ends, however it ends:
//! finished, timed out, or its future dropped).
//!
//! Every [`SshTransport`](super::SshTransport) built with the same
//! [`SshOptions`](super::SshOptions) (or clones of it) shares one set of
//! limits, so the calls of every site on a server count together. Other
//! processes (another apvm, the user's own ssh) are not counted.

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The calls each server may run at once, shared by clones.
#[derive(Clone)]
pub struct ServerLimits {
    /// Calls per server.
    per_server: NonZeroUsize,
    /// One semaphore per server id, made on first use. Never shrinks: it
    /// holds one entry per server an apvm process talks to.
    slots: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}

impl ServerLimits {
    /// The built-in calls per server: below sshd's default `MaxSessions` of
    /// 10, with room for the user's own sessions on a shared master.
    pub const DEFAULT_PER_SERVER: NonZeroUsize = NonZeroUsize::MIN.saturating_add(3);

    /// Limits of `per_server` calls at once per server.
    ///
    /// # Arguments
    ///
    /// * `per_server` - The calls one server may run at once
    pub fn new(per_server: NonZeroUsize) -> Self {
        Self {
            per_server,
            slots: Arc::default(),
        }
    }

    /// The calls one server may run at once.
    pub fn per_server(&self) -> NonZeroUsize {
        self.per_server
    }

    /// Whether `other` counts calls together with these (one is a clone of
    /// the other); limits made apart never do, whatever their size.
    ///
    /// # Arguments
    ///
    /// * `other` - Other limits
    pub fn shares_slots_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.slots, &other.slots)
    }

    /// Wait for a slot of `server`'s; it is free again when the returned
    /// permit is dropped. `None` only if the semaphore was closed, which
    /// nothing does: the call then runs unlimited rather than failing.
    ///
    /// # Arguments
    ///
    /// * `server` - The inventory server's id
    pub(crate) async fn acquire(&self, server: &str) -> Option<OwnedSemaphorePermit> {
        self.semaphore(server).acquire_owned().await.ok()
    }

    /// `server`'s semaphore, made on first use.
    ///
    /// # Arguments
    ///
    /// * `server` - The inventory server's id
    fn semaphore(&self, server: &str) -> Arc<Semaphore> {
        // A panic elsewhere while the lock was held leaves the map intact:
        // entries are only ever added whole.
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let semaphore = slots
            .entry(server.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_server.get())));
        Arc::clone(semaphore)
    }
}

impl Default for ServerLimits {
    /// [`ServerLimits::DEFAULT_PER_SERVER`] calls per server.
    fn default() -> Self {
        Self::new(Self::DEFAULT_PER_SERVER)
    }
}

impl fmt::Debug for ServerLimits {
    /// The calls per server (the semaphores tell nothing more).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerLimits")
            .field("per_server", &self.per_server)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    /// Run `calls` tasks on `server` that each hold a slot for a moment,
    /// and return the most that ran at once.
    ///
    /// # Arguments
    ///
    /// * `limits` - The limits
    /// * `servers` - The server of each task
    async fn most_at_once(limits: &ServerLimits, servers: &[&str]) -> usize {
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = servers
            .iter()
            .map(|server| {
                let (limits, server) = (limits.clone(), server.to_string());
                let (running, most) = (Arc::clone(&running), Arc::clone(&most));
                tokio::spawn(async move {
                    let _slot = limits.acquire(&server).await;
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        most.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn one_server_runs_at_most_its_limit_at_once() {
        let limits = ServerLimits::new(NonZeroUsize::new(2).unwrap());
        assert_eq!(most_at_once(&limits, &["eu-1"; 6]).await, 2);
        // The default: 4.
        assert_eq!(
            most_at_once(&ServerLimits::default(), &["eu-1"; 9]).await,
            4
        );
    }

    #[tokio::test]
    async fn servers_have_slots_of_their_own() {
        let limits = ServerLimits::new(NonZeroUsize::new(1).unwrap());
        let servers = ["a", "b", "c", "a", "b", "c"];
        assert_eq!(most_at_once(&limits, &servers).await, 3);
    }

    #[tokio::test]
    async fn clones_share_their_slots_and_new_limits_do_not() {
        let limits = ServerLimits::new(NonZeroUsize::new(1).unwrap());
        let held = limits.acquire("eu-1").await;
        assert!(held.is_some());
        let clone = limits.clone();
        assert!(clone.shares_slots_with(&limits));
        let waiting = tokio::time::timeout(Duration::from_millis(50), clone.acquire("eu-1"));
        assert!(waiting.await.is_err(), "a clone got a slot that is taken");
        // Another server, and other limits, are free.
        assert!(clone.acquire("eu-2").await.is_some());
        let other = ServerLimits::new(NonZeroUsize::new(1).unwrap());
        assert!(!other.shares_slots_with(&limits));
        assert!(other.acquire("eu-1").await.is_some());
        // A slot given back is taken by the next call.
        drop(held);
        assert!(clone.acquire("eu-1").await.is_some());
    }

    #[tokio::test]
    async fn waiting_calls_get_slots_in_the_order_they_asked() {
        let limits = ServerLimits::new(NonZeroUsize::new(1).unwrap());
        let held = limits.acquire("eu-1").await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiting = Vec::new();
        for i in 0..5 {
            let (limits, order) = (limits.clone(), Arc::clone(&order));
            waiting.push(tokio::spawn(async move {
                let _slot = limits.acquire("eu-1").await;
                order.lock().unwrap().push(i);
            }));
            // Let task `i` queue before the next one is spawned.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(held);
        for task in waiting {
            task.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn the_default_is_four_calls_per_server() {
        assert_eq!(ServerLimits::DEFAULT_PER_SERVER.get(), 4);
        assert_eq!(ServerLimits::default().per_server().get(), 4);
        let shown = format!("{:?}", ServerLimits::default());
        assert_eq!(shown, "ServerLimits { per_server: 4, .. }");
    }
}
