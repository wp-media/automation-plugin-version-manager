//! Classification of real OpenSSH stderr (plan §3.5, P3.3).
//!
//! Every sample was captured through the E2E harness with the transport's
//! own options (a generated `-F` file), byte for byte: ssh ends its own
//! lines with `\r\n`, a remote command's lines end with `\n`. `MAC` is
//! OpenSSH 10.3p1 (macOS, target `127.0.0.1:2722`), `UBUNTU` 9.6p1 (Ubuntu
//! 24.04) and `DEBIAN` 10.0p2 (Debian 13), both on the harness network
//! (target `target:22`); jump hosts go through the target to `localhost`.
//! The fifth audit's samples (`a5 …`) came from 7.6p1 (Ubuntu 18.04),
//! 8.2p1 (Ubuntu 20.04), 9.6p1, 10.0p2 (Debian 13, and Alpine 3.22's musl
//! build) and 10.3p1 (macOS, through ports published on `127.0.0.1`)
//! against an sshd 10.0p2 lab container at `172.19.0.50` (ports 2200–2204:
//! plain, `DisableForwarding yes`, `MaxSessions 0`, a host certificate with
//! another principal, an expired one) and small fake servers (ports
//! 2301–2306).

use super::*;
use std::time::Duration;

use crate::remote::{Captured, Nonce};

/// The banner OpenSSH 10.3p1 prints for a changed host key.
const MAC_CHANGED: &str = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\r\n\
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\r\n\
Someone could be eavesdropping on you right now (man-in-the-middle attack)!\r\n\
It is also possible that a host key has just been changed.\r\n\
The fingerprint for the ED25519 key sent by the remote host is\n\
SHA256:i7k7PtHHEfl4mfLYe4tONxzY6b1NwOLk8gaPVxhfGUc.\r\n\
Please contact your system administrator.\r\n\
Add correct host key in /tmp/exp/kh_changed to get rid of this message.\r\n\
Offending ED25519 key in /tmp/exp/kh_changed:1\r\n\
Host key for [127.0.0.1]:2722 has changed and you have requested strict checking.\r\n\
Host key verification failed.\r\n";

/// The banner OpenSSH 9.6p1 and 10.0p2 (Ubuntu, Debian) print for a
/// changed host key: Debian's patch adds the `remove with:` lines.
const LINUX_CHANGED: &str = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\r\n\
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\r\n\
Someone could be eavesdropping on you right now (man-in-the-middle attack)!\r\n\
It is also possible that a host key has just been changed.\r\n\
The fingerprint for the ED25519 key sent by the remote host is\n\
SHA256:i7k7PtHHEfl4mfLYe4tONxzY6b1NwOLk8gaPVxhfGUc.\r\n\
Please contact your system administrator.\r\n\
Add correct host key in /work/kh_changed to get rid of this message.\r\n\
Offending ED25519 key in /work/kh_changed:1\r\n\
  remove with:\r\n\
  ssh-keygen -f '/work/kh_changed' -R 'target'\r\n\
Host key for target has changed and you have requested strict checking.\r\n\
Host key verification failed.\r\n";

/// Every exit-255 sample: (name, stderr, class, the line kept).
fn samples() -> Vec<(&'static str, &'static str, SshFailureKind, &'static str)> {
    use SshFailureKind::*;
    let mut samples = vec![
        (
            "mac auth-wrong-key",
            "deploy@127.0.0.1: Permission denied (publickey).\r\n",
            AuthFailed,
            "deploy@127.0.0.1: Permission denied (publickey).",
        ),
        (
            "mac auth-unknown-user",
            "nosuchuser@127.0.0.1: Permission denied (publickey).\r\n",
            AuthFailed,
            "nosuchuser@127.0.0.1: Permission denied (publickey).",
        ),
        (
            "linux auth-wrong-key",
            "deploy@target: Permission denied (publickey).\r\n",
            AuthFailed,
            "deploy@target: Permission denied (publickey).",
        ),
        (
            "ubuntu too-many-keys",
            "Received disconnect from 172.19.0.3 port 22:2: Too many authentication failures\r\n\
             Disconnected from 172.19.0.3 port 22\r\n",
            AuthFailed,
            "Received disconnect from 172.19.0.3 port 22:2: Too many authentication failures",
        ),
        (
            "mac connection-refused",
            "ssh: connect to host 127.0.0.1 port 1: Connection refused\r\n",
            Unreachable,
            "ssh: connect to host 127.0.0.1 port 1: Connection refused",
        ),
        (
            "mac dns-failure",
            "ssh: Could not resolve hostname apvm-e2e-nonexistent.invalid: nodename nor servname \
             provided, or not known\r\n",
            Unreachable,
            "ssh: Could not resolve hostname apvm-e2e-nonexistent.invalid: nodename nor servname \
             provided, or not known",
        ),
        (
            "linux dns-failure",
            "ssh: Could not resolve hostname apvm-e2e-nonexistent.invalid: Name or service not \
             known\r\n",
            Unreachable,
            "ssh: Could not resolve hostname apvm-e2e-nonexistent.invalid: Name or service not \
             known",
        ),
        (
            "ubuntu ipv6 no route",
            "ssh: connect to host 2001:db8::1 port 22: Network is unreachable\r\n",
            Unreachable,
            "ssh: connect to host 2001:db8::1 port 22: Network is unreachable",
        ),
        (
            "mac not-ssh-server",
            "Connection closed by 127.0.0.1 port 8722\r\n",
            Unreachable,
            "Connection closed by 127.0.0.1 port 8722",
        ),
        (
            "linux not-ssh-server",
            "Connection closed by 172.19.0.3 port 80\r\n",
            Unreachable,
            "Connection closed by 172.19.0.3 port 80",
        ),
        (
            "mac host-key-unknown",
            "No ED25519 host key is known for [127.0.0.1]:2722 and you have requested strict \
             checking.\r\nHost key verification failed.\r\n",
            HostKeyUnknown,
            "No ED25519 host key is known for [127.0.0.1]:2722 and you have requested strict \
             checking.",
        ),
        (
            "linux host-key-unknown",
            "No ED25519 host key is known for target and you have requested strict checking.\r\n\
             Host key verification failed.\r\n",
            HostKeyUnknown,
            "No ED25519 host key is known for target and you have requested strict checking.",
        ),
        (
            "mac host-key-changed (strict and accept-new alike)",
            MAC_CHANGED,
            HostKeyChanged,
            "Host key for [127.0.0.1]:2722 has changed and you have requested strict checking.",
        ),
        (
            "linux host-key-changed (strict and accept-new alike)",
            LINUX_CHANGED,
            HostKeyChanged,
            "Host key for target has changed and you have requested strict checking.",
        ),
        (
            "mac connect-timeout",
            "ssh: connect to host 192.0.2.1 port 22: Operation timed out\r\n",
            Timeout,
            "ssh: connect to host 192.0.2.1 port 22: Operation timed out",
        ),
        (
            "linux connect-timeout",
            "ssh: connect to host 192.0.2.1 port 22: Connection timed out\r\n",
            Timeout,
            "ssh: connect to host 192.0.2.1 port 22: Connection timed out",
        ),
        (
            // verify.sh's banner-timeout sample (OpenSSH 10.3p1; P0 saw the
            // same on 9.6p1).
            "banner-timeout",
            "Connection timed out during banner exchange\r\n\
             Connection to 127.0.0.1 port 49624 timed out\r\n",
            Timeout,
            "Connection timed out during banner exchange",
        ),
        (
            "mac jump-host-key-unknown",
            "No ED25519 host key is known for [127.0.0.1]:2722 and you have requested strict \
             checking.\r\nHost key verification failed.\r\nConnection closed by UNKNOWN port \
             65535\r\n",
            HostKeyUnknown,
            "No ED25519 host key is known for [127.0.0.1]:2722 and you have requested strict \
             checking.",
        ),
        (
            // A jump host reached without apvm's options (a ProxyCommand of
            // the user's running plain ssh), asking nobody (a5: 9.6p1,
            // 10.0p2, 10.3p1): `--trust-new-host-key` cannot reach it, so it
            // is no `ssh_host_key_unknown`.
            "a5 proxycommand ssh without batchmode",
            "Host key verification failed.\r\nConnection closed by UNKNOWN port 65535\r\n",
            Unrecognized,
            "Host key verification failed.",
        ),
        (
            // The same before 8.0, and 8.0–9.4: the outer ssh's own last
            // line is the banner one, which is not the cause.
            "a5 7.6p1 proxycommand ssh without batchmode",
            "Host key verification failed.\r\nssh_exchange_identification: Connection closed by \
             remote host\r\n",
            Unrecognized,
            "Host key verification failed.",
        ),
        (
            "a5 8.2p1 proxycommand ssh without batchmode",
            "Host key verification failed.\r\nkex_exchange_identification: Connection closed by \
             remote host\r\n",
            Unrecognized,
            "Host key verification failed.",
        ),
        (
            "linux jump-auth",
            "deploy@target: Permission denied (publickey).\r\nConnection closed by UNKNOWN port \
             65535\r\n",
            AuthFailed,
            "deploy@target: Permission denied (publickey).",
        ),
        (
            "mac jump-unreachable",
            "ssh: connect to host 127.0.0.1 port 1: Connection refused\r\nConnection closed by \
             UNKNOWN port 65535\r\n",
            Unreachable,
            "ssh: connect to host 127.0.0.1 port 1: Connection refused",
        ),
        (
            "jump-dest-refused",
            "channel 0: open failed: connect failed: Connection refused\r\nstdio forwarding \
             failed\r\nConnection closed by UNKNOWN port 65535\r\n",
            Unreachable,
            "channel 0: open failed: connect failed: Connection refused",
        ),
        (
            "jump-dest-dns",
            "channel 0: open failed: connect failed: Name or service not known\r\nstdio \
             forwarding failed\r\nConnection closed by UNKNOWN port 65535\r\n",
            Unreachable,
            "channel 0: open failed: connect failed: Name or service not known",
        ),
        // OpenSSH 7.6p1 (Ubuntu 18.04) and 8.2p1 (Ubuntu 20.04) on the
        // harness network, the generated file's way (P3.3 audit).
        (
            // Before 8.2 the banner exchange had another name.
            "7.6p1 not-ssh-server",
            "ssh_exchange_identification: read: Connection reset by peer\r\n",
            Unreachable,
            "ssh_exchange_identification: read: Connection reset by peer",
        ),
        (
            "8.2p1 not-ssh-server",
            "kex_exchange_identification: banner line contains invalid characters\r\n",
            Unreachable,
            "kex_exchange_identification: banner line contains invalid characters",
        ),
        (
            "8.2p1 not-ssh-server closing",
            "kex_exchange_identification: Connection closed by remote host\r\n",
            Unreachable,
            "kex_exchange_identification: Connection closed by remote host",
        ),
        (
            // Old clients end a jump host failure with the banner line, not
            // `Connection closed by UNKNOWN port 65535`.
            "7.6p1 jump-dest-refused",
            "channel 0: open failed: connect failed: Connection refused\r\nstdio forwarding \
             failed\r\nssh_exchange_identification: Connection closed by remote host\r\n",
            Unreachable,
            "channel 0: open failed: connect failed: Connection refused",
        ),
        (
            "7.6p1 jump-host-key-unknown",
            "No ECDSA host key is known for 172.19.0.3 and you have requested strict checking.\r\n\
             Host key verification failed.\r\nssh_exchange_identification: Connection closed by \
             remote host\r\n",
            HostKeyUnknown,
            "No ECDSA host key is known for 172.19.0.3 and you have requested strict checking.",
        ),
        // A key file others can read: ssh ignores it, so the login is
        // refused, and the kept line says why (audit 3).
        (
            "mac unprotected-key",
            UNPROTECTED_KEY_MAC,
            AuthFailed,
            "Permissions 0644 for '/Volumes/Shared/projects/wp-media/apvm-p3.3/target/\
             tmp-audit3/openkey' are too open.",
        ),
        (
            // Before 8.5 `CheckHostIP yes` also records the address first.
            "7.6p1 unprotected-key",
            "Warning: Permanently added the ECDSA host key for IP address '172.19.0.3' to the \
             list of known hosts.\r\n\
             @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
             @         WARNING: UNPROTECTED PRIVATE KEY FILE!          @\r\n\
             @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
             Permissions 0644 for '/home/bob/openkey' are too open.\r\n\
             It is required that your private key files are NOT accessible by others.\r\n\
             This private key will be ignored.\r\n\
             Load key \"/home/bob/openkey\": bad permissions\r\n\
             deploy@target: Permission denied (publickey).\r\n",
            AuthFailed,
            "Permissions 0644 for '/home/bob/openkey' are too open.",
        ),
        // `CheckHostIP yes` (the default before 8.5, an ssh_config's choice
        // after) and a key known for the server's address that differs
        // from the one its name has: refused, strict and accept-new alike,
        // with the generic last line (audit 4; target `target` behind the
        // address 172.19.0.3).
        (
            "7.6p1 host-key-differs-from-ip-key",
            "Warning: the ECDSA host key for 'target' differs from the key for the IP address \
             '172.19.0.3'\nOffending key for IP in /root/kh:5\nMatching host key in \
             /root/kh:1\r\nExiting, you have requested strict checking.\r\n\
             Host key verification failed.\r\n",
            HostKeyChanged,
            "Warning: the ECDSA host key for 'target' differs from the key for the IP address \
             '172.19.0.3'",
        ),
        (
            "9.6p1 host-key-differs-from-ip-key (CheckHostIP yes)",
            "Warning: the ED25519 host key for 'target' differs from the key for the IP address \
             '172.19.0.3'\nOffending key for IP in /root/kh:5\nMatching host key in \
             /root/kh:2\r\nExiting, you have requested strict checking.\r\n\
             Host key verification failed.\r\n",
            HostKeyChanged,
            "Warning: the ED25519 host key for 'target' differs from the key for the IP address \
             '172.19.0.3'",
        ),
        // A host key the user marked revoked (audit 4, 10.3p1): refused,
        // strict and accept-new alike.
        (
            "mac host-key-revoked (@revoked in known_hosts, accept-new)",
            "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
             @       WARNING: REVOKED HOST KEY DETECTED!               @\r\n\
             @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
             The ED25519 host key for [127.0.0.1]:2722 is marked as revoked.\r\n\
             This could mean that a stolen key is being used to\r\n\
             impersonate this host.\r\n\
             ED25519 host key for [127.0.0.1]:2722 was revoked and you have requested strict \
             checking.\r\nHost key verification failed.\r\n",
            HostKeyChanged,
            "ED25519 host key for [127.0.0.1]:2722 was revoked and you have requested strict \
             checking.",
        ),
        (
            "mac host-key-revoked (RevokedHostKeys)",
            "Host key ED25519 SHA256:Ur+osiBn7eNSbfdLzumsOkJxUf7ZVNN18FB5X/Gv8R4 revoked by file \
             /Volumes/Shared/projects/wp-media/apvm-p3.3/target/tmp-audit4/revoked_keys\r\n\
             Host key verification failed.\r\n",
            HostKeyChanged,
            "Host key ED25519 SHA256:Ur+osiBn7eNSbfdLzumsOkJxUf7ZVNN18FB5X/Gv8R4 revoked by file \
             /Volumes/Shared/projects/wp-media/apvm-p3.3/target/tmp-audit4/revoked_keys",
        ),
    ];
    samples.extend(audit5_samples());
    samples
}

/// The fifth audit's samples (see the module docs): every class whose
/// phrases it added or changed, on the versions whose wording differs.
fn audit5_samples() -> Vec<(&'static str, &'static str, SshFailureKind, &'static str)> {
    use SshFailureKind::*;
    vec![
        // Any failure to connect, whatever the system calls it.
        (
            "a5 musl network unreachable",
            "ssh: connect to host 255.255.255.255 port 22: Network unreachable\r\n",
            Unreachable,
            "ssh: connect to host 255.255.255.255 port 22: Network unreachable",
        ),
        (
            "a5 mac broadcast address",
            "ssh: connect to host 255.255.255.255 port 22: Address family not supported by \
             protocol family\r\n",
            Unreachable,
            "ssh: connect to host 255.255.255.255 port 22: Address family not supported by \
             protocol family",
        ),
        (
            "a5 7.6p1 no route",
            "ssh: connect to host 172.19.0.99 port 22: No route to host\r\n",
            Unreachable,
            "ssh: connect to host 172.19.0.99 port 22: No route to host",
        ),
        (
            "a5 musl no route",
            "ssh: connect to host 172.19.0.99 port 22: Host is unreachable\r\n",
            Unreachable,
            "ssh: connect to host 172.19.0.99 port 22: Host is unreachable",
        ),
        // Names that do not resolve, or no resolver at all.
        (
            "a5 musl dns failure",
            "ssh: Could not resolve hostname nosuchhost.invalid: Name does not resolve\r\n",
            Unreachable,
            "ssh: Could not resolve hostname nosuchhost.invalid: Name does not resolve",
        ),
        (
            "a5 7.6p1 no resolver",
            "ssh: Could not resolve hostname example.com: Temporary failure in name \
             resolution\r\n",
            Unreachable,
            "ssh: Could not resolve hostname example.com: Temporary failure in name resolution",
        ),
        (
            "a5 musl no resolver",
            "ssh: Could not resolve hostname example.com: Try again\r\n",
            Unreachable,
            "ssh: Could not resolve hostname example.com: Try again",
        ),
        // Something that is not an SSH-2 server, or one that resets the
        // connection during the handshake.
        (
            "a5 7.6p1 ssh-1 server",
            "Protocol major versions differ: 2 vs. 1\r\n",
            Unreachable,
            "Protocol major versions differ: 2 vs. 1",
        ),
        (
            "a5 10.0p2 ssh-1 server",
            "Protocol major versions differ: 2 vs. 1\r\nbanner exchange: Connection to \
             172.19.0.50 port 2301: could not read protocol version\r\n",
            Unreachable,
            "Protocol major versions differ: 2 vs. 1",
        ),
        (
            "a5 7.6p1 bad banner",
            "Bad remote protocol version identification: 'SSH-garbage\n\n'\r\n",
            Unreachable,
            "Bad remote protocol version identification: 'SSH-garbage",
        ),
        (
            "a5 mac bad banner",
            "Bad remote protocol version identification: 'SSH-garbage'\r\nbanner exchange: \
             Connection to 127.0.0.1 port 27302: invalid format\r\n",
            Unreachable,
            "Bad remote protocol version identification: 'SSH-garbage'",
        ),
        (
            "a5 7.6p1 reset after the banner",
            "Connection reset by 172.19.0.50 port 2303\r\n",
            Unreachable,
            "Connection reset by 172.19.0.50 port 2303",
        ),
        // A banner that never comes: 7.6p1 calls it a connect timeout.
        (
            "a5 7.6p1 no banner",
            "ssh: connect to host 172.19.0.50 port 2305: Connection timed out\r\n",
            Timeout,
            "ssh: connect to host 172.19.0.50 port 2305: Connection timed out",
        ),
        (
            "a5 8.2p1 no banner",
            "Connection timed out during banner exchange\r\n",
            Timeout,
            "Connection timed out during banner exchange",
        ),
        // A jump host that forwards nothing: its channel refusal, on every
        // version, whatever the outer ssh's last line.
        (
            "a5 7.6p1 hop forwards nothing",
            "channel 0: open failed: administratively prohibited: open failed\r\nstdio \
             forwarding failed\r\nssh_exchange_identification: Connection closed by remote \
             host\r\n",
            Unreachable,
            "channel 0: open failed: administratively prohibited: open failed",
        ),
        (
            "a5 8.2p1 hop forwards nothing",
            "channel 0: open failed: administratively prohibited: open failed\r\nstdio \
             forwarding failed\r\nkex_exchange_identification: Connection closed by remote \
             host\r\n",
            Unreachable,
            "channel 0: open failed: administratively prohibited: open failed",
        ),
        (
            "a5 mac hop forwards nothing",
            "channel 0: open failed: administratively prohibited: open failed\r\nstdio \
             forwarding failed\r\nConnection closed by UNKNOWN port 65535\r\n",
            Unreachable,
            "channel 0: open failed: administratively prohibited: open failed",
        ),
        // The server itself opens no session (`MaxSessions 0`): no jump host
        // involved, so not unreachable (every version prints this line).
        (
            "a5 mac no session",
            "channel 0: open failed: connect failed: open failed\r\n",
            Unrecognized,
            "channel 0: open failed: connect failed: open failed",
        ),
        // A host certificate an `@cert-authority` line rejects: ssh then
        // tries its plain key, which is new, but trusting new keys would
        // bypass the CA (with accept-new the call runs, see
        // `host_key_notices_report_a_refused_certificate`).
        (
            "a5 mac certificate for another name",
            "Certificate invalid: name is not a listed principal\r\nNo ED25519 host key is known \
             for [127.0.0.1]:27203 and you have requested strict checking.\r\nHost key \
             verification failed.\r\n",
            Unrecognized,
            "Certificate invalid: name is not a listed principal",
        ),
        (
            "a5 7.6p1 certificate for another name",
            "key_cert_check_authority: invalid certificate\r\nCertificate invalid: name is not a \
             listed principal\r\nNo ED25519 host key is known for [172.19.0.50]:2203 and you \
             have requested strict checking.\r\nHost key verification failed.\r\n",
            Unrecognized,
            "Certificate invalid: name is not a listed principal",
        ),
        (
            "a5 9.6p1 expired certificate",
            "Certificate invalid: expired\r\nNo ED25519 host key is known for \
             [172.19.0.50]:2204 and you have requested strict checking.\r\nHost key \
             verification failed.\r\n",
            Unrecognized,
            "Certificate invalid: expired",
        ),
        // A `RevokedHostKeys` file that cannot be read: neither new nor
        // changed (every version prints the same).
        (
            "a5 mac revoked keys file missing",
            "Error checking host key ED25519 SHA256:UutY9AgMBAf6sMFjSzvG3xHXI+uNZ5CTlyyWq/MtfHM \
             in revoked keys file /var/folders/ch/n74cjyr900d98tcydn9lp8940000gn/T/\
             tmp.ADxSrxVeRd/missing/revoked: No such file or directory\r\nHost key \
             verification failed.\r\n",
            Unrecognized,
            "Error checking host key ED25519 SHA256:UutY9AgMBAf6sMFjSzvG3xHXI+uNZ5CTlyyWq/MtfHM \
             in revoked keys file /var/folders/ch/n74cjyr900d98tcydn9lp8940000gn/T/\
             tmp.ADxSrxVeRd/missing/revoked: No such file or directory",
        ),
        // A key file ssh cannot load: the login is refused, and the kept
        // line says why (wording from libcrypto on 9.6p1 and later Linux
        // builds).
        (
            "a5 mac key file that is no key",
            "Load key \"/var/folders/ch/n74cjyr900d98tcydn9lp8940000gn/T/tmp.ADxSrxVeRd/garbage\": \
             invalid format\r\ndeploy@127.0.0.1: Permission denied (publickey).\r\n",
            AuthFailed,
            "Load key \"/var/folders/ch/n74cjyr900d98tcydn9lp8940000gn/T/tmp.ADxSrxVeRd/garbage\": \
             invalid format",
        ),
        (
            "a5 9.6p1 key file that is no key",
            "Load key \"/tmp/tmp.76JVbZtvwf/garbage\": error in libcrypto\r\n\
             deploy@172.19.0.50: Permission denied (publickey).\r\n",
            AuthFailed,
            "Load key \"/tmp/tmp.76JVbZtvwf/garbage\": error in libcrypto",
        ),
        // An ssh_config key file that does not exist: noise before the
        // refusal.
        (
            "a5 10.0p2 missing config key",
            "no such identity: /tmp/tmp.C6nmtAPnkn/missing_key: No such file or directory\r\n\
             nosuchuser@172.19.0.50: Permission denied (publickey).\r\n",
            AuthFailed,
            "nosuchuser@172.19.0.50: Permission denied (publickey).",
        ),
        // A bad option: the line that names it, not the summary after it
        // (every version prints the same two lines).
        (
            "a5 7.6p1 bad option",
            "/tmp/tmp.xqeRanCOKU/user1: line 2: Bad configuration option: foobarbaz\r\n\
             /tmp/tmp.xqeRanCOKU/user1: terminating, 1 bad configuration options\r\n",
            Unrecognized,
            "/tmp/tmp.xqeRanCOKU/user1: line 2: Bad configuration option: foobarbaz",
        ),
    ]
}

/// What OpenSSH 10.3p1 prints for a key file others can read (8.2p1 and
/// 10.0p2 print the same lines).
const UNPROTECTED_KEY_MAC: &str = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
@         WARNING: UNPROTECTED PRIVATE KEY FILE!          @\r\n\
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
Permissions 0644 for '/Volumes/Shared/projects/wp-media/apvm-p3.3/target/tmp-audit3/openkey' are too open.\r\n\
It is required that your private key files are NOT accessible by others.\r\n\
This private key will be ignored.\r\n\
Load key \"/Volumes/Shared/projects/wp-media/apvm-p3.3/target/tmp-audit3/openkey\": bad permissions\r\n\
deploy@127.0.0.1: Permission denied (publickey).\r\n";

/// A nonce for executions.
fn nonce() -> Nonce {
    Nonce::parse("apvm-0123456789abcdef0123456789abcdef").unwrap()
}

/// An execution that ended with `exit`, printed `stdout` and `stderr`.
///
/// # Arguments
///
/// * `exit` - How it ended
/// * `stdout` - Its stdout
/// * `stderr` - Its stderr
fn execution(exit: Exit, stdout: &str, stderr: &str) -> Execution {
    Execution::new(
        nonce(),
        exit,
        Captured::whole(stdout.as_bytes().to_vec()),
        Captured::whole(stderr.as_bytes().to_vec()),
        true,
        Duration::from_millis(3),
    )
}

/// An exit-255 execution whose script never started.
///
/// # Arguments
///
/// * `stderr` - Its stderr
fn failed(stderr: &str) -> Execution {
    execution(Exit::TransportFailed, "", stderr)
}

#[test]
fn every_real_sample_gets_its_class_and_its_most_telling_line() {
    for (name, stderr, kind, line) in samples() {
        let failure = classify(&failed(stderr)).unwrap();
        assert_eq!(failure.kind(), kind, "{name}");
        assert_eq!(failure.line(), Some(line), "{name}");
        assert_eq!(failure.code(), kind.code(), "{name}");
    }
}

#[test]
fn noise_lines_around_a_failure_change_nothing() {
    // The last two lines: a5, every version (an option a build lacks, a
    // key file an ssh_config names that does not exist).
    let noise = "Warning: Permanently added '[127.0.0.1]:2722' (ED25519) to the list of known \
                 hosts.\r\nbash: warning: setlocale: LC_ALL: cannot change locale \
                 (xx_XX.UTF-8)\nPseudo-terminal will not be allocated because stdin is not a \
                 terminal.\r\n/tmp/tmp.76JVbZtvwf/user11 line 2: Unsupported option \
                 \"rsaauthentication\"\r\nno such identity: /tmp/tmp.76JVbZtvwf/missing_key: No \
                 such file or directory\r\n";
    for (name, stderr, kind, line) in samples() {
        for noisy in [format!("{noise}{stderr}"), format!("{stderr}{noise}")] {
            let failure = classify(&failed(&noisy)).unwrap();
            assert_eq!(
                (failure.kind(), failure.line()),
                (kind, Some(line)),
                "{name}"
            );
        }
    }
}

/// What a multiplexed call prints when it goes on over a connection of its
/// own (real, 10.3p1): a master at sshd's `MaxSessions`, then the line of a
/// call that could not become the master (alone when another call became
/// it first); a master whose network connection died (after its keep-alive
/// checks gave up); a stopped master (after the connect timeout).
const MUX_FALLBACKS: [&str; 3] = [
    "mux_client_request_session: session request failed: Session open refused by peer\r\n\
     ControlSocket /tmp/claude-501/mx/mux/24b95b452d45b637cea6bcd02cba7dce6b61a64e already \
     exists, disabling multiplexing\r\n",
    "mux_client_request_session: read from master failed: Broken pipe\r\n",
    "muxclient: master hello exchange failed\r\n\
     ControlSocket /tmp/claude-501/mx2/mux/24b95b452d45b637cea6bcd02cba7dce6b61a64e already \
     exists, disabling multiplexing\r\n",
];

/// Every fallback sample, one after the other.
fn mux_fallback() -> String {
    MUX_FALLBACKS.concat()
}

#[test]
fn a_multiplexed_call_that_opens_its_own_connection_prints_noise() {
    let fallback = mux_fallback();
    for line in fallback.lines() {
        assert!(is_noise(line.trim()), "{line}");
    }
    for sample in MUX_FALLBACKS {
        // Alone (the call then ran, or failed without a message).
        let failure = classify(&failed(sample)).unwrap();
        assert_eq!(
            (failure.kind(), failure.line()),
            (SshFailureKind::Unrecognized, None)
        );
    }
    for (name, stderr, kind, line) in samples() {
        let failure = classify(&failed(&format!("{fallback}{stderr}"))).unwrap();
        assert_eq!(
            (failure.kind(), failure.line()),
            (kind, Some(line)),
            "{name}"
        );
    }
    // Alone after the nonce (the master died during the call: ssh prints
    // nothing, verified on 10.3p1), it is a lost connection.
    let stdout = format!("{}\n", nonce());
    let lost = execution(Exit::TransportFailed, &stdout, &mux_fallback());
    let failure = classify(&lost).unwrap();
    assert_eq!(
        (failure.kind(), failure.line()),
        (SshFailureKind::ConnectionLost, None)
    );
    // Remote text that merely mentions multiplexing is not noise.
    assert!(!is_noise("mux_client_request_session"));
    // Only the verified fallbacks: another mux line keeps its diagnosis.
    assert!(!is_noise(
        "mux_client_request_session: unexpected response from master 0x1"
    ));
    assert!(!is_noise("note: muxclient: master hello exchange failed"));
}

#[test]
fn noise_alone_is_unrecognized_with_no_line() {
    let stderr = "Warning: Permanently added 'target' (ED25519) to the list of known hosts.\r\n\
                  bash: warning: setlocale: LC_ALL: cannot change locale (xx_XX.UTF-8)\n\
                  Connection closed by UNKNOWN port 65535\r\n";
    let failure = classify(&failed(stderr)).unwrap();
    assert_eq!(failure.kind(), SshFailureKind::Unrecognized);
    assert_eq!(failure.line(), None);
    assert_eq!(failure.code(), Code::SshFailed);
    assert_eq!(failure.to_string(), "ssh failed");
}

/// What OpenSSH 10.1+ prints on a connection whose key exchange is not
/// post-quantum (real: 10.3p1 on macOS against an 8.2p1 server).
const PQ_WARNING: &str = "** WARNING: connection is not using a post-quantum key exchange \
                          algorithm.\r\n\
                          ** This session may be vulnerable to \"store now, decrypt later\" \
                          attacks.\r\n\
                          ** The server may need to be upgraded. See \
                          https://openssh.com/pq.html\r\n";

#[test]
fn the_post_quantum_warning_is_noise() {
    // Printed before the failure (an auth failure, sampled), never its cause.
    for (name, stderr, kind, line) in samples() {
        let failure = classify(&failed(&format!("{PQ_WARNING}{stderr}"))).unwrap();
        assert_eq!(
            (failure.kind(), failure.line()),
            (kind, Some(line)),
            "{name}"
        );
    }
    // A remote shell killed by a signal: ssh exits 255 and says nothing
    // else (sampled), so no line explains it, before or after the nonce.
    let failure = classify(&failed(PQ_WARNING)).unwrap();
    assert_eq!(
        (failure.kind(), failure.line()),
        (SshFailureKind::Unrecognized, None)
    );
    let stdout = format!("{}\n", nonce());
    let lost = execution(Exit::TransportFailed, &stdout, PQ_WARNING);
    let failure = classify(&lost).unwrap();
    assert_eq!(
        (failure.kind(), failure.line()),
        (SshFailureKind::ConnectionLost, None)
    );
}

#[test]
fn a_remote_exit_255_with_empty_stderr_is_unrecognized() {
    // P0: indistinguishable from an SSH failure by status alone; the
    // scripts map their own 255 to 254, so this one is ssh's.
    let failure = classify(&failed("")).unwrap();
    assert_eq!(
        (failure.kind(), failure.line()),
        (SshFailureKind::Unrecognized, None)
    );
}

#[test]
fn an_unknown_failure_keeps_its_last_line_that_is_more_than_a_summary() {
    // A macOS `UseKeychain` read by another ssh (a5: the two lines every
    // version prints): the summary names no option (until audit 5 it was
    // the line kept).
    let stderr = "/home/u/.ssh/config: line 3: Bad configuration option: usekeychain\r\n\
                  /home/u/.ssh/config: terminating, 1 bad configuration options\r\n";
    let failure = classify(&failed(stderr)).unwrap();
    assert_eq!(failure.kind(), SshFailureKind::Unrecognized);
    assert_eq!(
        failure.to_string(),
        "ssh failed: /home/u/.ssh/config: line 3: Bad configuration option: usekeychain"
    );
    // A disconnect for a reason no class names: the reason, not the
    // `Disconnected from` after it (the pair as sampled with `Too many
    // authentication failures`; this reason is not sampled).
    let stderr = "Received disconnect from 10.0.0.5 port 22:11: Bye Bye\r\n\
                  Disconnected from 10.0.0.5 port 22\r\n";
    assert_eq!(
        classify(&failed(stderr)).unwrap().line(),
        Some("Received disconnect from 10.0.0.5 port 22:11: Bye Bye")
    );
    // Otherwise the last line, summaries alone included.
    let stderr = "first\r\nsecond\r\n";
    assert_eq!(classify(&failed(stderr)).unwrap().line(), Some("second"));
    let stderr = "Disconnected from 10.0.0.5 port 22\r\n";
    assert_eq!(
        classify(&failed(stderr)).unwrap().line(),
        Some("Disconnected from 10.0.0.5 port 22")
    );
}

#[test]
fn a_host_key_class_needs_the_line_that_ends_every_host_key_refusal() {
    // A server's login banner (printed among ssh's lines once its host key
    // passed) that quotes a host key phrase cannot turn a refused login
    // into a host key failure.
    let stderr = "Note: if you see REMOTE HOST IDENTIFICATION HAS CHANGED, call us.\n\
                  No ED25519 host key is known for x and you have requested strict checking.\n\
                  deploy@h: Permission denied (publickey).\r\n";
    let failure = classify(&failed(stderr)).unwrap();
    assert_eq!(failure.kind(), SshFailureKind::AuthFailed);
    assert_eq!(
        failure.line(),
        Some("deploy@h: Permission denied (publickey).")
    );
}

#[test]
fn some_phrases_count_only_at_the_start_of_a_line() {
    // A reset during the handshake (a5, 7.6p1–10.0p2) is unreachable; a
    // reset of a session that was up is a lost connection.
    let handshake = "Connection reset by 172.19.0.50 port 2303\r\n";
    assert_eq!(
        classify(&failed(handshake)).unwrap().kind(),
        SshFailureKind::Unreachable
    );
    let session = "Read from remote host target: Connection reset by peer\r\n";
    let failure = classify(&failed(session)).unwrap();
    assert_eq!(failure.kind(), SshFailureKind::ConnectionLost);
    // The same for a key file ssh cannot load, a missing ssh_config key
    // file and `stdio forwarding failed`: quoted inside another line, they
    // explain, hide or decide nothing (constructed lines).
    let quoted = "said: Load key \"/k\": invalid format\n\
                  deploy@h: Permission denied (publickey).\r\n";
    assert_eq!(
        classify(&failed(quoted)).unwrap().line(),
        Some("deploy@h: Permission denied (publickey).")
    );
    let quoted = "said: no such identity: x\nsaid: stdio forwarding failed\n";
    let failure = classify(&failed(quoted)).unwrap();
    assert_eq!(
        (failure.kind(), failure.line()),
        (
            SshFailureKind::Unrecognized,
            Some("said: stdio forwarding failed")
        )
    );
    assert!(is_noise("no such identity: x") && !is_noise("said: no such identity: x"));
}

#[test]
fn a_lone_handshake_close_is_unreachable_after_a_jump_host_it_is_not_the_cause() {
    // 8.2p1 closing before its banner: the line decides.
    let alone = "kex_exchange_identification: Connection closed by remote host\r\n";
    let failure = classify(&failed(alone)).unwrap();
    assert_eq!(
        (failure.kind(), failure.line()),
        (
            SshFailureKind::Unreachable,
            Some("kex_exchange_identification: Connection closed by remote host")
        )
    );
    // After a jump host's (or ProxyCommand's) own line, as the last line,
    // that line decides, as `Connection closed by UNKNOWN port 65535` lets
    // it on 9.5 and later (the outer lines as sampled; the ProxyCommand's
    // line is constructed).
    for outer in [
        "ssh_exchange_identification: Connection closed by remote host",
        "kex_exchange_identification: Connection closed by remote host",
        "Connection closed by UNKNOWN port 65535",
    ] {
        let stderr = format!("/bin/sh: 1: exec: nc: not found\n{outer}\r\n");
        let failure = classify(&failed(&stderr)).unwrap();
        assert_eq!(
            (failure.kind(), failure.line()),
            (
                SshFailureKind::Unrecognized,
                Some("/bin/sh: 1: exec: nc: not found")
            ),
            "{outer}"
        );
    }
}

#[test]
fn remote_stderr_with_the_same_phrases_is_never_an_ssh_failure() {
    // Real: a remote command printing ssh's phrases, then exit 3.
    let stderr = "deploy@x: Permission denied (publickey).\nHost key verification failed.\n";
    for exit in [
        Exit::Status(0),
        Exit::Status(3),
        Exit::Status(Exit::REMAPPED_255),
        Exit::TimedOut,
        Exit::Killed { signal: Some(9) },
    ] {
        assert_eq!(classify(&execution(exit, "", stderr)), None, "{exit:?}");
    }
}

#[test]
fn a_script_that_started_lost_its_connection_whatever_it_printed() {
    // The nonce line is out: the connection was made, so phrases a remote
    // command printed before the drop never pass for an auth failure.
    let stdout = format!("motd\n\n{}\npartial", nonce());
    let stderr = "deploy@x: Permission denied (publickey).\n\
                  Connection to target closed by remote host.\r\n";
    let failure = classify(&execution(Exit::TransportFailed, &stdout, stderr)).unwrap();
    assert_eq!(failure.kind(), SshFailureKind::ConnectionLost);
    assert_eq!(
        failure.line(),
        Some("Connection to target closed by remote host.")
    );
    assert_eq!(failure.code(), Code::SshConnectionLost);

    // Without a telling line, the last one; without any, none.
    let quiet = execution(Exit::TransportFailed, &stdout, "something else\n");
    assert_eq!(classify(&quiet).unwrap().line(), Some("something else"));
    let silent = execution(Exit::TransportFailed, &stdout, "");
    let failure = classify(&silent).unwrap();
    assert_eq!(
        (failure.kind(), failure.line()),
        (SshFailureKind::ConnectionLost, None)
    );
}

#[test]
fn lost_connections_are_classified_before_the_script_started_too() {
    // Ubuntu 9.6p1: the target paused (ServerAlive), and its session killed;
    // then OpenSSH's message when it cannot write to a dead connection (not
    // sampled).
    for (stderr, line) in [
        (
            "Timeout, server target not responding.\r\n",
            "Timeout, server target not responding.",
        ),
        (
            "Connection to target closed by remote host.\r\n",
            "Connection to target closed by remote host.",
        ),
        (
            "client_loop: send disconnect: Broken pipe\r\n",
            "client_loop: send disconnect: Broken pipe",
        ),
    ] {
        let failure = classify(&failed(stderr)).unwrap();
        assert_eq!(
            (failure.kind(), failure.line()),
            (SshFailureKind::ConnectionLost, Some(line))
        );
    }
}

#[test]
fn a_closed_handshake_is_unreachable_not_a_lost_connection() {
    // OpenSSH's message when a server closes the connection before the SSH
    // banner (MaxStartups, a firewall; sampled on 8.2p1): it holds "closed
    // by remote host" too, so its own phrase must win.
    let stderr = "kex_exchange_identification: Connection closed by remote host\r\n\
                  Connection closed by 10.0.0.5 port 22\r\n";
    let failure = classify(&failed(stderr)).unwrap();
    assert_eq!(failure.kind(), SshFailureKind::Unreachable);
    assert_eq!(
        failure.line(),
        Some("kex_exchange_identification: Connection closed by remote host")
    );
    // Before 8.2 the same phase was `ssh_exchange_identification`, and its
    // reset wording holds the lost-connection phrase (7.6p1).
    let reset = "ssh_exchange_identification: read: Connection reset by peer\r\n";
    let failure = classify(&failed(reset)).unwrap();
    assert_eq!(failure.kind(), SshFailureKind::Unreachable);
}

#[test]
fn sshs_last_message_decides_whatever_came_before_it() {
    // Until audit 6, "the most specific class wins whatever the order of
    // lines"; ssh stops at its first fatal error, so its last message is
    // the verdict, and these two orders (the ones ssh can print) keep their
    // classes (the reverse orders: the host key tests below).
    let changed_last =
        format!("ssh: connect to host x port 22: Connection refused\r\n{MAC_CHANGED}");
    assert_eq!(
        classify(&failed(&changed_last)).unwrap().kind(),
        SshFailureKind::HostKeyChanged
    );
    let auth_then_timeout = "Connection timed out during banner exchange\r\n\
                             deploy@h: Permission denied (publickey).\r\n";
    assert_eq!(
        classify(&failed(auth_then_timeout)).unwrap().kind(),
        SshFailureKind::AuthFailed
    );
}

#[test]
fn a_key_file_others_can_read_explains_only_a_refused_login() {
    // A hop's ssh may warn about the key and still log in through an
    // agent: the failure after it is the hop's, not the login's.
    let hop = format!(
        "{}channel 0: open failed: connect failed: Connection refused\r\n\
         stdio forwarding failed\r\nConnection closed by UNKNOWN port 65535\r\n",
        UNPROTECTED_KEY_MAC.replace("deploy@127.0.0.1: Permission denied (publickey).\r\n", "")
    );
    let failure = classify(&failed(&hop)).unwrap();
    assert_eq!(failure.kind(), SshFailureKind::Unreachable);
    assert_eq!(
        failure.line(),
        Some("channel 0: open failed: connect failed: Connection refused")
    );
    // Alone, the warning is no reason to fail.
    let warning =
        UNPROTECTED_KEY_MAC.replace("deploy@127.0.0.1: Permission denied (publickey).\r\n", "");
    assert_eq!(
        classify(&failed(&warning)).unwrap().kind(),
        SshFailureKind::Unrecognized
    );
    // The message then says why the login was refused.
    let failure = classify(&failed(UNPROTECTED_KEY_MAC)).unwrap();
    assert!(
        failure
            .to_string()
            .starts_with("the server refused the SSH login: Permissions 0644 for '"),
        "{failure}"
    );
}

#[test]
fn lines_are_escaped_and_cut_for_messages() {
    let long = format!(
        "deploy@h: Permission denied (publickey).{}\r\n",
        "x".repeat(600)
    );
    let failure = classify(&failed(&long)).unwrap();
    let line = failure.line().unwrap();
    assert_eq!(line.chars().count(), LINE_MAX_CHARS + 1);
    assert!(line.ends_with('…'), "{line}");

    let hostile = "deploy@h: Permission denied (publickey).\u{1b}[2J\u{202e}\r\n";
    let failure = classify(&failed(hostile)).unwrap();
    assert_eq!(
        failure.line(),
        Some("deploy@h: Permission denied (publickey).\\u{1b}[2J\\u{202e}")
    );

    let invalid = b"deploy@h: Permission denied (publickey).\xff\r\n";
    let execution = Execution::new(
        nonce(),
        Exit::TransportFailed,
        Captured::default(),
        Captured::whole(invalid.to_vec()),
        true,
        Duration::ZERO,
    );
    let failure = classify(&execution).unwrap();
    assert_eq!(failure.kind(), SshFailureKind::AuthFailed);
    assert_eq!(
        failure.line(),
        Some("deploy@h: Permission denied (publickey).\u{fffd}")
    );
}

#[test]
fn every_class_has_its_code_and_sentence() {
    use SshFailureKind::*;
    let table = [
        (
            HostKeyChanged,
            "ssh_host_key_changed",
            "the server's host key has changed",
        ),
        (
            HostKeyUnknown,
            "ssh_host_key_unknown",
            "the server's host key is not known",
        ),
        (
            AuthFailed,
            "ssh_auth_failed",
            "the server refused the SSH login",
        ),
        (Timeout, "ssh_timeout", "the SSH connection timed out"),
        (
            Unreachable,
            "ssh_unreachable",
            "the server cannot be reached over SSH",
        ),
        (
            ConnectionLost,
            "ssh_connection_lost",
            "the SSH connection was lost",
        ),
        (Unrecognized, "ssh_failed", "ssh failed"),
    ];
    for (kind, code, sentence) in table {
        assert_eq!(kind.code().as_str(), code);
        let failure = SshFailure { kind, line: None };
        assert_eq!(failure.to_string(), sentence);
        let failure = SshFailure {
            kind,
            line: Some("x".into()),
        };
        assert_eq!(failure.to_string(), format!("{sentence}: x"));
    }
    // The host key classes come from the lines before ssh's closing host
    // key line, most specific first (a6: until audit 6 one table, whose
    // host key rules needed that line anywhere); the others from ssh's
    // last message itself, in classification order.
    let host_key: Vec<SshFailureKind> = HOST_KEY_RULES.iter().map(|rule| rule.kind).collect();
    assert_eq!(
        host_key,
        [HostKeyChanged, Unrecognized, Unrecognized, HostKeyUnknown]
    );
    let own: Vec<SshFailureKind> = CAUSE_RULES.iter().map(|rule| rule.kind).collect();
    assert_eq!(own, [AuthFailed, Timeout, Unreachable, ConnectionLost]);
    for rule in HOST_KEY_RULES.iter().chain(CAUSE_RULES) {
        assert!(!rule.phrases.is_empty(), "{:?}", rule.kind);
    }
}

#[test]
fn host_key_notices_list_recorded_hosts_and_unsaved_keys() {
    let jump = "Warning: Permanently added 'target' (ED25519) to the list of known hosts.\r\n\
                Warning: Permanently added 'localhost' (ED25519) to the list of known hosts.\r\n";
    let notices = host_key_notices(&Captured::whole(jump.as_bytes().to_vec()));
    assert_eq!(notices.recorded, ["target", "localhost"]);
    assert!(!notices.not_saved);
    // OpenSSH 7.6p1 (`CheckHostIP yes` by default before 8.5): name and
    // address, as ssh names them.
    let old = "Warning: Permanently added 'target,172.19.0.3' (ECDSA) to the list of known \
               hosts.\r\n";
    let notices = host_key_notices(&Captured::whole(old.as_bytes().to_vec()));
    assert_eq!(notices.recorded, ["target,172.19.0.3"]);

    // OpenSSH 9.6p1/10.0p2 with a known_hosts file in a missing directory:
    // the key is trusted for the call and not kept.
    let unsaved = "Failed to add the host to the list of known hosts (/work/nodir/kh).\r\n";
    let notices = host_key_notices(&Captured::whole(unsaved.as_bytes().to_vec()));
    assert!(notices.recorded.is_empty());
    assert!(notices.not_saved);

    // A remote line that only looks alike, or nothing.
    for stderr in [
        "Warning: Permanently added 'x' to something else.\n",
        "Warning: Permanently added x (ED25519) to the list of known hosts.\n",
        "",
    ] {
        let notices = host_key_notices(&Captured::whole(stderr.as_bytes().to_vec()));
        assert_eq!(notices, HostKeyNotices::default(), "{stderr:?}");
    }
    // A remote command's stderr that copies ssh's notices, as it would
    // print them (`\n`), reports nothing (a6).
    let copied = "Warning: Permanently added 'evil' (ED25519) to the list of known hosts.\n\
                  Failed to add the host to the list of known hosts (/k).\n\
                  Certificate invalid: expired\n";
    let notices = host_key_notices(&Captured::whole(copied.as_bytes().to_vec()));
    assert_eq!(notices, HostKeyNotices::default());
    let hostile = "Warning: Permanently added 'a\u{1b}b' (ED25519) to the list of known hosts.\r\n";
    let notices = host_key_notices(&Captured::whole(hostile.as_bytes().to_vec()));
    assert_eq!(notices.recorded, ["a\\u{1b}b"]);
}

#[test]
fn host_key_notices_report_a_refused_certificate() {
    // a5, accept-new and a host certificate for another name (10.0p2; the
    // same on 7.6p1–10.3p1, exit 0): ssh trusted the certificate's plain
    // key as new, bypassing the CA, and the call ran.
    let stderr = "Certificate invalid: name is not a listed principal\r\nWarning: Permanently \
                  added '[172.19.0.50]:2203' (ED25519) to the list of known hosts.\r\n";
    let notices = host_key_notices(&Captured::whole(stderr.as_bytes().to_vec()));
    assert_eq!(notices.recorded, ["[172.19.0.50]:2203"]);
    assert_eq!(
        notices.refused_certificate.as_deref(),
        Some("Certificate invalid: name is not a listed principal")
    );
    // 7.6p1 prints a line before it; the reason is still the one kept.
    let old = "key_cert_check_authority: invalid certificate\r\nCertificate invalid: expired\r\n\
               Warning: Permanently added '[172.19.0.50]:2204' (ED25519) to the list of known \
               hosts.\r\n";
    let notices = host_key_notices(&Captured::whole(old.as_bytes().to_vec()));
    assert_eq!(
        notices.refused_certificate.as_deref(),
        Some("Certificate invalid: expired")
    );
    // None without one.
    let plain = "Warning: Permanently added 'target' (ED25519) to the list of known hosts.\r\n";
    let notices = host_key_notices(&Captured::whole(plain.as_bytes().to_vec()));
    assert_eq!(notices.refused_certificate, None);
}

/// A login banner that quotes ssh's phrases (a6: printed by every client
/// as the server sent it, `\n`, among ssh's own `\r\n` lines; sshd 10.0p2
/// `Banner`, clients 7.6p1, 8.2p1, 9.6p1, 10.0p2 glibc and musl, 10.6p1).
const QUOTING_BANNER: &str = "Authorized use only. Idle sessions are timed out after 15 minutes.\n\
                              WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED is a message you \
                              should report.\n\
                              Host key verification failed. means: call the help desk.\n";

#[test]
fn the_cause_is_sshs_last_message_never_a_banner_or_remote_text_before_it() {
    use SshFailureKind::*;
    // a6, real (every client above; 7.6p1 and 8.2p1 also print the
    // `CheckHostIP` notice first): the server's banner, then why ssh ended.
    for (name, last, kind) in [
        // The server closed the connection during authentication
        // (`LoginGraceTime`).
        (
            "closed during authentication",
            "Connection closed by 172.19.0.3 port 2205",
            Unreachable,
        ),
        // The login was refused.
        (
            "refused login",
            "deploy@target: Permission denied (publickey).",
            AuthFailed,
        ),
        // The session's sshd was killed before the script started (8.2p1
        // says `client_loop: send disconnect: Broken pipe`).
        (
            "session killed",
            "Connection to target closed by remote host.",
            ConnectionLost,
        ),
        (
            "8.2p1 session killed",
            "client_loop: send disconnect: Broken pipe",
            ConnectionLost,
        ),
    ] {
        let stderr = format!("{QUOTING_BANNER}{last}\r\n");
        let failure = classify(&failed(&stderr)).unwrap();
        assert_eq!(
            (failure.kind(), failure.line()),
            (kind, Some(last)),
            "{name}"
        );
    }
    // A remote startup file's stderr, then the session ended with 255 and
    // ssh printed nothing (a6, real: a `ForceCommand` that prints and exits
    // 255): no message of ssh's, so no class, whatever the text says.
    let stderr = format!("{QUOTING_BANNER}Error: Permission denied (os error 13)\n");
    let failure = classify(&failed(&stderr)).unwrap();
    assert_eq!(
        (failure.kind(), failure.line()),
        (Unrecognized, Some("Error: Permission denied (os error 13)"))
    );
    // A remote command's lines before ssh's own last one (constructed).
    let stderr = "curl: (7) Failed to connect to db port 5432: Connection refused\n\
                  Timeout, server target not responding.\r\n";
    assert_eq!(classify(&failed(stderr)).unwrap().kind(), ConnectionLost);
}

#[test]
fn host_key_lines_count_only_when_sshs_last_message_is_the_refusal() {
    // ssh ends every host key refusal with `fatal("Host key verification
    // failed.")` (sshconnect2.c, 7.6p1–10.6p1), so its lines anywhere else
    // are not ssh's verdict (constructed orders).
    let unknown = "No ED25519 host key is known for x and you have requested strict \
                   checking.\r\nHost key verification failed.\r\n";
    let stderr = format!("{unknown}deploy@h: Permission denied (publickey).\r\n");
    assert_eq!(
        classify(&failed(&stderr)).unwrap().kind(),
        SshFailureKind::AuthFailed
    );
    let stderr = format!("{MAC_CHANGED}ssh: connect to host x port 22: Connection refused\r\n");
    assert_eq!(
        classify(&failed(&stderr)).unwrap().kind(),
        SshFailureKind::Unreachable
    );
    // The refusal itself must be ssh's line (`\r\n`), not a server's text,
    // and the whole line, not a line that quotes it.
    let stderr = unknown.replace("\r\n", "\n");
    assert_eq!(
        classify(&failed(&stderr)).unwrap().kind(),
        SshFailureKind::Unrecognized
    );
    let stderr = "No ED25519 host key is known for x and you have requested strict \
                  checking.\r\nHost key verification failed. Call us.\r\n";
    assert_eq!(
        classify(&failed(stderr)).unwrap().line(),
        Some("Host key verification failed. Call us.")
    );
}

#[test]
fn ssh_s_messages_end_with_cr_lf_and_what_it_relays_does_not_decide() {
    // ssh logs every message with `\r\n` (sampled on every client); a line
    // after its last one that ends with `\n` alone is the server's (or a
    // ProxyCommand program's), and is not the cause.
    let stderr = "deploy@h: Permission denied (publickey).\r\nsaid: Connection refused\n";
    let failure = classify(&failed(stderr)).unwrap();
    assert_eq!(
        (failure.kind(), failure.line()),
        (
            SshFailureKind::AuthFailed,
            Some("deploy@h: Permission denied (publickey).")
        )
    );
    // A stderr without a line end at all (cut at its limit) keeps its last
    // message of ssh's.
    let stderr = "ssh: connect to host h port 22: Connection refused\r\nbash: war";
    assert_eq!(
        classify(&failed(stderr)).unwrap().kind(),
        SshFailureKind::Unreachable
    );
    // An explanation counts only as ssh's own line.
    let stderr = "banner: Permissions 0644 for '/k' are too open.\n\
                  deploy@h: Permission denied (publickey).\r\n";
    assert_eq!(
        classify(&failed(stderr)).unwrap().line(),
        Some("deploy@h: Permission denied (publickey).")
    );
}

#[test]
fn a_jump_host_that_times_out_reaching_the_next_hop_is_a_timeout() {
    // The channel line decides a hop's failure (constructed from the
    // sampled refusals with the system's timeout wording).
    let stderr = "channel 0: open failed: connect failed: Connection timed out\r\n\
                  stdio forwarding failed\r\nConnection closed by UNKNOWN port 65535\r\n";
    let failure = classify(&failed(stderr)).unwrap();
    assert_eq!(
        (failure.kind(), failure.line()),
        (
            SshFailureKind::Timeout,
            Some("channel 0: open failed: connect failed: Connection timed out")
        )
    );
}
