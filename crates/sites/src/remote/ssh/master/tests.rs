//! The master's helpers: what waiting calls share, the process-wide gates.

use super::*;

#[test]
fn only_a_failed_or_unanswered_start_is_shared_with_waiting_calls() {
    let unreachable = Ready::Unreachable {
        stderr: b"ssh: connect to host h port 22: Connection refused\r\n".to_vec(),
        elapsed: Duration::from_secs(1),
    };
    assert!(unreachable.shared_with_waiters());
    assert!(Ready::Unknown(Vec::new()).shared_with_waiters());
    assert!(Ready::Stuck.shared_with_waiters());
    assert!(!Ready::Live.shared_with_waiters());
    assert!(!Ready::Started(Vec::new()).shared_with_waiters());
}

#[test]
fn only_calls_that_waited_share_a_failed_start() {
    let refused = Ready::Unreachable {
        stderr: b"refused\r\n".to_vec(),
        elapsed: Duration::from_secs(2),
    };
    let mut gate = Gate::default();
    let ticket = gate.generation();
    assert_eq!(gate.shared_since(ticket), None);
    gate.record(&refused);
    // A call that waited while it ran shares it; a later call does not.
    assert_eq!(gate.shared_since(ticket), Some(refused.clone()));
    assert_eq!(gate.shared_since(gate.generation()), None);
    // A started master is not shared (the next call checks it).
    let ticket = gate.generation();
    gate.record(&Ready::Started(Vec::new()));
    assert_eq!(gate.shared_since(ticket), None);
    // A live master starts nothing.
    let ticket = gate.generation();
    gate.record(&Ready::Live);
    assert_eq!(gate.generation(), ticket);
    // An unanswered start is shared, so waiters run on their own.
    gate.record(&Ready::Unknown(b"Warning: Permanently added\r\n".to_vec()));
    assert!(matches!(gate.shared_since(ticket), Some(Ready::Unknown(_))));
}

#[test]
fn a_waiter_gets_the_failure_without_the_host_key_notices() {
    let stderr = b"Warning: Permanently added '[h]:22' (ED25519) to the list of known hosts.\r\n\
                   h: Permission denied (publickey).\r\n"
        .to_vec();
    let failed = Ready::Unreachable {
        stderr,
        elapsed: Duration::from_secs(1),
    };
    let mut gate = Gate::default();
    let ticket = gate.generation();
    gate.record(&failed);
    let Some(Ready::Unreachable { stderr, .. }) = gate.shared_since(ticket) else {
        panic!("not shared");
    };
    assert_eq!(stderr, b"h: Permission denied (publickey).\r\n");
    let unknown =
        Ready::Unknown(b"Failed to add the host to the list of known hosts (/k).\r\n".to_vec());
    assert_eq!(unknown.for_waiter(), Ready::Unknown(Vec::new()));
    assert_eq!(Ready::Stuck.for_waiter(), Ready::Stuck);
}

#[test]
fn one_gate_per_configuration_in_the_process() {
    let shared = |tag: &str| Shared {
        master_config: String::new(),
        own_config: String::new(),
        control_dir: PathBuf::from("/m"),
        tag: tag.to_string(),
        socket: PathBuf::from("/m/s"),
    };
    let a = gate_for(&shared("gatetest").gate_key());
    let again = gate_for(&shared("gatetest").gate_key());
    let other = gate_for(&shared("gatetes2").gate_key());
    assert!(Arc::ptr_eq(&a, &again));
    assert!(!Arc::ptr_eq(&a, &other));
}
