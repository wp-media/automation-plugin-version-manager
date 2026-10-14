//! A transport that remembers what ssh said about host keys on every call
//! it carries (plan P3.3's obligation for P4.3): the probe and the server
//! check consume each call's stderr, so the notices are read on the way.

use std::sync::Mutex;

use apvm_sites::Result;
use apvm_sites::remote::{
    BoxFuture, Execution, HostKeyNotices, Request, Transport, host_key_notices,
};

/// Wraps a transport, recording the host key notices of each call that
/// ran (failed calls included: a key recorded before a refused login
/// stays recorded).
pub struct Noticing<'t> {
    /// The transport that runs the calls.
    inner: &'t dyn Transport,
    /// What ssh said so far, in call order.
    seen: Mutex<HostKeyNotices>,
}

impl<'t> Noticing<'t> {
    /// A recorder around `inner`.
    ///
    /// # Arguments
    ///
    /// * `inner` - The transport that runs the calls
    pub fn new(inner: &'t dyn Transport) -> Self {
        Self {
            inner,
            seen: Mutex::new(HostKeyNotices::default()),
        }
    }

    /// Everything ssh said about host keys on the calls so far.
    pub fn notices(&self) -> HostKeyNotices {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Add `execution`'s notices to what was seen.
    ///
    /// # Arguments
    ///
    /// * `execution` - A call that ran
    fn record(&self, execution: &Execution) {
        let found = host_key_notices(execution.stderr());
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        merge(&mut seen, found);
    }
}

/// Add `found` to `seen`: hosts once each, in order; `not_saved` if any
/// call said so; the first refused certificate.
///
/// # Arguments
///
/// * `seen` - The notices so far
/// * `found` - One call's notices
fn merge(seen: &mut HostKeyNotices, found: HostKeyNotices) {
    for host in found.recorded {
        if !seen.recorded.contains(&host) {
            seen.recorded.push(host);
        }
    }
    seen.not_saved |= found.not_saved;
    if seen.refused_certificate.is_none() {
        seen.refused_certificate = found.refused_certificate;
    }
}

impl Transport for Noticing<'_> {
    fn execute<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Execution>> {
        Box::pin(async move {
            let execution = self.inner.execute(request).await?;
            self.record(&execution);
            Ok(execution)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use apvm_sites::remote::{Captured, Exit, Nonce, Script};

    use super::*;

    /// A transport that answers every call with `stderr` and `exit`.
    struct Fixed {
        /// What each call's stderr holds.
        stderr: &'static [u8],
        /// How each call ends.
        exit: Exit,
    }

    impl Transport for Fixed {
        fn execute<'a>(&'a self, _request: &'a Request) -> BoxFuture<'a, Result<Execution>> {
            Box::pin(async move {
                Ok(Execution::new(
                    Nonce::random(),
                    self.exit,
                    Captured::whole(Vec::new()),
                    Captured::whole(self.stderr.to_vec()),
                    true,
                    Duration::ZERO,
                ))
            })
        }
    }

    /// A request for any script.
    fn request() -> Request {
        let script = Script::wp(
            Nonce::random(),
            &apvm_sites::remote::Identity::Login,
            &apvm_sites::remote::WpCli::new(
                &["wp".to_string()],
                apvm_sites::remote::FileMode::new(0o755).expect("mode"),
                "/srv/wp",
            )
            .expect("wp"),
        );
        Request::script(script, Duration::from_secs(1))
    }

    /// Run two calls through a recorder around `inner`.
    fn twice(inner: &dyn Transport) -> HostKeyNotices {
        let noticing = Noticing::new(inner);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let request = request();
        for _ in 0..2 {
            runtime
                .block_on(noticing.execute(&request))
                .expect("the call ran");
        }
        noticing.notices()
    }

    #[test]
    fn recorded_keys_are_kept_once_each_even_from_failed_calls() {
        let inner = Fixed {
            stderr: b"Warning: Permanently added '[127.0.0.1]:2722' (ED25519) to the list of \
                      known hosts.\r\nuser@h: Permission denied (publickey).\r\n",
            exit: Exit::TransportFailed,
        };
        let notices = twice(&inner);
        assert_eq!(notices.recorded, ["[127.0.0.1]:2722"]);
        assert!(!notices.not_saved);
        assert_eq!(notices.refused_certificate, None);
    }

    #[test]
    fn unsaved_keys_and_refused_certificates_are_kept() {
        let inner = Fixed {
            stderr: b"Certificate invalid: expired\r\nFailed to add the host to the list of \
                      known hosts (/k).\r\n",
            exit: Exit::Status(0),
        };
        let notices = twice(&inner);
        assert!(notices.not_saved);
        assert_eq!(
            notices.refused_certificate.as_deref(),
            Some("Certificate invalid: expired")
        );
    }

    #[test]
    fn a_quiet_call_reports_nothing() {
        let inner = Fixed {
            stderr: b"",
            exit: Exit::Status(0),
        };
        assert_eq!(twice(&inner), HostKeyNotices::default());
    }

    #[test]
    fn merge_keeps_the_first_certificate_and_new_hosts_in_order() {
        let mut seen = HostKeyNotices::default();
        seen.recorded.push("a".into());
        seen.refused_certificate = Some("first".into());
        let mut found = HostKeyNotices::default();
        found.recorded.extend(["a".to_string(), "b".to_string()]);
        found.refused_certificate = Some("second".into());
        merge(&mut seen, found);
        assert_eq!(seen.recorded, ["a", "b"]);
        assert_eq!(seen.refused_certificate.as_deref(), Some("first"));
    }
}
