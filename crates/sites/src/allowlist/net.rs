//! Network values: the site URL WP-CLI gets as `--url`, and SSH jump hosts.

use std::net::{Ipv4Addr, Ipv6Addr};

use super::{HOST, Rule, describe, user};
use crate::problem::quoted;

/// Longest site URL accepted, in bytes.
const URL_MAX_BYTES: usize = 2048;
/// Longest DNS name accepted, in bytes.
const DNS_NAME_MAX_BYTES: usize = 253;
/// Longest DNS label accepted, in bytes.
const DNS_LABEL_MAX_BYTES: usize = 63;
/// Longest `proxy_jump` list accepted, in bytes.
const PROXY_JUMP_MAX_BYTES: usize = 1024;
/// Most hops a `proxy_jump` list may have (OpenSSH starts one nested ssh
/// per hop).
const PROXY_JUMP_MAX_HOPS: usize = 8;

/// A site URL: `http(s)://host[:port][/path]` with a lowercase scheme, at
/// most 2048 bytes, and no user info, query or fragment. The host is a DNS
/// name, an IPv4 address, or an IPv6 address in brackets; the path holds
/// `[A-Za-z0-9._~/-]` and `%`-escapes, and no `.` or `..` segment.
pub(crate) fn url(value: &str) -> Rule {
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return Err("must start with 'http://' or 'https://'".into());
    };
    if value.len() > URL_MAX_BYTES {
        return Err(format!("must be at most {URL_MAX_BYTES} bytes"));
    }
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    // Checked first, and never echoed: it may hold a password.
    if authority.contains('@') {
        return Err("must not contain user info (user[:password]@)".into());
    }
    let address = split_host_port(authority)?;
    if address.bracketed {
        ipv6(address.host)?;
    } else {
        dns_name_or_ipv4(address.host)?;
    }
    address.port.map_or(Ok(()), port_text)?;
    url_path(path)
}

/// A URL host that is a DNS name (labels of `[A-Za-z0-9-]`, 1–63 bytes, no
/// leading or trailing `-`) or, when made of digits and dots only, an IPv4
/// address.
fn dns_name_or_ipv4(host: &str) -> Rule {
    if host.is_empty() {
        return Err("must name a host".into());
    }
    if host.len() > DNS_NAME_MAX_BYTES {
        return Err(format!(
            "has a host longer than {DNS_NAME_MAX_BYTES} characters"
        ));
    }
    if host.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return host
            .parse::<Ipv4Addr>()
            .map(drop)
            .map_err(|_| format!("has an invalid IPv4 address {}", quoted(host)));
    }
    host.split('.').try_for_each(dns_label)
}

/// One label of a DNS name.
fn dns_label(label: &str) -> Rule {
    if label.is_empty() {
        return Err("has an empty host label (a dot at either end, or two in a row)".into());
    }
    if let Some(bad) = label
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-'))
    {
        return Err(format!(
            "contains {} in its host; only letters, digits, '.' and '-' are allowed",
            describe(bad)
        ));
    }
    if label.len() > DNS_LABEL_MAX_BYTES {
        return Err(format!(
            "has a host label longer than {DNS_LABEL_MAX_BYTES} characters"
        ));
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err("has a host label that starts or ends with '-'".into());
    }
    Ok(())
}

/// An IPv6 address (written in brackets in URLs and jump hosts).
fn ipv6(host: &str) -> Rule {
    host.parse::<Ipv6Addr>()
        .map(drop)
        .map_err(|_| format!("has an invalid IPv6 address {}", quoted(host)))
}

/// A URL path: `[A-Za-z0-9._~/-]` and `%XX` escapes, with no `.` or `..`
/// segment, escaped (`%2e`) or not.
fn url_path(path: &str) -> Rule {
    let is_hex = |c: Option<char>| c.is_some_and(|c| c.is_ascii_hexdigit());
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            if !(is_hex(chars.next()) && is_hex(chars.next())) {
                return Err("has a '%' not followed by two hex digits".into());
            }
        } else if !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '/' | '-')) {
            return Err(format!(
                "contains {} in its path; only letters, digits, '.', '_', '~', '/', '-' \
                 and %-escapes are allowed (no query or fragment)",
                describe(c)
            ));
        }
    }
    let dot_segment = |segment: &str| {
        let decoded = segment.replace("%2e", ".").replace("%2E", ".");
        matches!(decoded.as_str(), "." | "..")
    };
    if path.split('/').any(dot_segment) {
        return Err("must not have a '.' or '..' path segment".into());
    }
    Ok(())
}

/// `host[:port]`, split.
struct HostPort<'a> {
    /// The host, without brackets.
    host: &'a str,
    /// The port text, when given.
    port: Option<&'a str>,
    /// Whether the host was written in brackets (an IPv6 address).
    bracketed: bool,
}

/// Split `host[:port]` or `[ipv6][:port]`. An IPv6 address without
/// brackets is refused: its colons cannot be told from a port separator.
///
/// # Arguments
///
/// * `text` - The address (no user info)
///
/// # Errors
///
/// The reason the address cannot be split.
fn split_host_port(text: &str) -> Result<HostPort<'_>, String> {
    if let Some(inner) = text.strip_prefix('[') {
        let (host, after) = inner
            .split_once(']')
            .ok_or_else(|| "has an unclosed '['".to_string())?;
        let port = match after {
            "" => None,
            _ => Some(
                after
                    .strip_prefix(':')
                    .ok_or_else(|| "has text after ']' that is not ':<port>'".to_string())?,
            ),
        };
        return Ok(HostPort {
            host,
            port,
            bracketed: true,
        });
    }
    let (host, port) = match text.split_once(':') {
        Some((_, port)) if port.contains(':') => {
            return Err("needs brackets around an IPv6 address, as in [2001:db8::1]:22".into());
        }
        Some((host, port)) => (host, Some(port)),
        None => (text, None),
    };
    Ok(HostPort {
        host,
        port,
        bracketed: false,
    })
}

/// A port written as text: digits only, 1–65535.
fn port_text(text: &str) -> Rule {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("has a port {} that is not a number", quoted(text)));
    }
    match text.parse::<u16>() {
        Ok(port) if port > 0 => Ok(()),
        _ => Err("has a port outside 1–65535".into()),
    }
}

/// Jump hosts, as `ssh -J` takes them: a comma list of at most 8
/// `[user@]host[:port]` hops, at most 1024 bytes.
///
/// - `none` (any case) is refused: it would turn off a jump host set in the
///   user's ssh_config, so the connection would go direct.
/// - A bracketed IPv6 address is allowed in the last hop only: ssh pastes
///   the earlier hops unquoted into a command run by the user's shell, where
///   `[` is a pattern (zsh fails with "no matches found"; verified). The other
///   allowed characters are literal in every shell.
pub(crate) fn proxy_jump(value: &str) -> Rule {
    if value.is_empty() {
        return Err("must not be empty".into());
    }
    if value.eq_ignore_ascii_case("none") {
        return Err(
            "must not be 'none' (it turns off jump hosts set in ssh_config; omit the field)".into(),
        );
    }
    if value.len() > PROXY_JUMP_MAX_BYTES {
        return Err(format!("must be at most {PROXY_JUMP_MAX_BYTES} bytes"));
    }
    let hops: Vec<&str> = value.split(',').collect();
    if hops.len() > PROXY_JUMP_MAX_HOPS {
        return Err(format!("must have at most {PROXY_JUMP_MAX_HOPS} hops"));
    }
    let last = hops.len().saturating_sub(1);
    for (index, hop) in hops.into_iter().enumerate() {
        jump_hop(hop, index == last)
            .map_err(|reason| format!("hop {} {}: {reason}", index + 1, quoted(hop)))?;
    }
    Ok(())
}

/// One `[user@]host[:port]` hop of a `proxy_jump` list.
///
/// # Arguments
///
/// * `hop` - The hop
/// * `last` - Whether it is the last hop (the only one that may use brackets)
fn jump_hop(hop: &str, last: bool) -> Rule {
    if hop.is_empty() {
        return Err("is empty".into());
    }
    let (login, address) = match hop.split_once('@') {
        Some((login, address)) => (Some(login), address),
        None => (None, hop),
    };
    if let Some(login) = login {
        user(login).map_err(|reason| format!("user {reason}"))?;
    }
    let address = split_host_port(address)?;
    if address.bracketed {
        if !last {
            return Err(
                "may not be a bracketed IPv6 address: only the last hop may (ssh passes the \
                 others through the local shell, where '[' is special)"
                    .into(),
            );
        }
        ipv6(address.host)?;
    } else {
        HOST.check(address.host)
            .map_err(|reason| format!("host {reason}"))?;
    }
    address.port.map_or(Ok(()), port_text)
}

#[cfg(test)]
mod tests {
    use super::super::table;
    use super::*;

    #[test]
    fn urls() {
        let long_path = format!("https://example.com/{}", "a".repeat(2028));
        let too_long = format!("https://example.com/{}", "a".repeat(2029));
        let label = "a".repeat(63);
        let longest_host = [label.as_str(), &label, &label, &"b".repeat(61)].join(".");
        assert_eq!(longest_host.len(), 253);
        table(
            url,
            &[
                "https://shop.example.com",
                "http://localhost:8080",
                "https://example.com/sub1/",
                "http://127.0.0.1",
                "http://[::1]:8080/x",
                "https://[2001:db8::1]",
                "https://[::ffff:192.0.2.1]",
                "https://example.com/caf%C3%A9",
                "https://xn--caf-dma.example/~user/a_b.c-d",
                "https://1.example.com",
                "https://example.com/a..b/",
                &format!("https://{label}.example"),
                &format!("https://{longest_host}"),
                &long_path,
            ],
            &[
                ("", "must start with"),
                ("example.com", "must start with"),
                ("ftp://example.com", "must start with"),
                ("HTTPS://example.com", "must start with"),
                ("https://", "must name a host"),
                ("https://:8080", "must name a host"),
                ("https://exa mple.com", "U+0020"),
                ("https://u@example.com", "user info"),
                ("https://example.com?x", "'?'"),
                ("https://example.com/?a=b", "'?'"),
                ("https://example.com/#x", "'#'"),
                ("https://example.com:0", "outside 1–65535"),
                ("https://example.com:99999", "outside 1–65535"),
                ("https://example.com:", "not a number"),
                ("https://example.com:x", "not a number"),
                ("https://-x.com", "starts or ends with '-'"),
                ("https://a-.com", "starts or ends with '-'"),
                ("https://.x.com", "empty host label"),
                ("https://a..b", "empty host label"),
                ("https://example.com.", "empty host label"),
                ("https://0", "invalid IPv4"),
                ("https://999.999.999.999", "invalid IPv4"),
                ("https://01.2.3.4", "invalid IPv4"),
                ("https://example.com/%zz", "two hex digits"),
                ("https://example.com/%4", "two hex digits"),
                ("https://example.com/a b", "U+0020"),
                ("https://example.com/../wp-admin", "'..' path segment"),
                ("https://example.com/./x", "'..' path segment"),
                ("https://example.com/%2e%2E/x", "'..' path segment"),
                ("https://[::1", "unclosed '['"),
                ("https://[]/", "invalid IPv6"),
                ("https://[:]", "invalid IPv6"),
                ("https://[::::::]", "invalid IPv6"),
                ("https://[1.2.3.4:]", "invalid IPv6"),
                ("https://[fffff::]", "invalid IPv6"),
                ("https://[fe80::1%25eth0]", "invalid IPv6"),
                ("https://[::1]x", "not ':<port>'"),
                ("https://[example.com]", "invalid IPv6"),
                ("https://2001:db8::1", "needs brackets"),
                ("https://example.com/\u{202e}", "U+202E"),
                (
                    &format!("https://{}.example", "a".repeat(64)),
                    "longer than 63",
                ),
                (&format!("https://a{longest_host}"), "longer than 253"),
                (&too_long, "at most 2048 bytes"),
            ],
        );
    }

    #[test]
    fn user_info_is_never_echoed() {
        let refused = url("https://admin:S3cretPass@example.com").unwrap_err();
        assert_eq!(refused, "must not contain user info (user[:password]@)");
        assert!(!refused.contains("S3cret"));
    }

    #[test]
    fn proxy_jumps() {
        let eight = ["h"; 8].join(",");
        let nine = ["h"; 9].join(",");
        table(
            proxy_jump,
            &[
                "bastion",
                "deploy@bastion",
                "bastion:2222",
                "deploy@bastion:2222,jump2",
                "[2001:db8::1]:22",
                "user@[::1]",
                "10.0.0.1,10.0.0.2:2200",
                "jump1,deploy@[::1]:2222",
                &eight,
            ],
            &[
                ("", "must not be empty"),
                ("none", "must not be 'none'"),
                ("NONE", "must not be 'none'"),
                (",", "hop 1 '': is empty"),
                ("a,,b", "hop 2 '': is empty"),
                ("a,", "hop 2 '': is empty"),
                ("@host", "user must not be empty"),
                ("user@", "host must not be empty"),
                ("0@host", "user must not be all digits"),
                ("host:", "not a number"),
                ("host:0", "outside 1–65535"),
                ("host:70000", "outside 1–65535"),
                ("2001:db8::1", "needs brackets"),
                ("[::1", "unclosed '['"),
                ("[::1]x", "not ':<port>'"),
                ("[foo]", "invalid IPv6"),
                ("[.]", "invalid IPv6"),
                ("u@[::1]:2,127.0.0.1:1", "only the last hop may"),
                ("-oProxyCommand=x", "contains '='"),
                ("a b", "U+0020"),
                ("user@-x", "host must start with"),
                ("-user@host", "user must start with"),
                ("a@b@c", "host contains '@'"),
                ("ssh://host", "not a number"),
                ("a%h", "contains '%'"),
                (&nine, "at most 8 hops"),
            ],
        );
        assert_eq!(
            proxy_jump("bastion,-x"),
            Err("hop 2 '-x': host must start with a letter, digit, '.', '_' or ':'".into())
        );
    }

    #[test]
    fn proxy_jump_is_limited_to_1024_bytes() {
        let hop = "h".repeat(127);
        let longest = format!("{}a", [hop.as_str(); 8].join(","));
        assert_eq!(longest.len(), 1024);
        assert_eq!(proxy_jump(&longest), Ok(()));
        assert_eq!(
            proxy_jump(&format!("{longest}b")),
            Err("must be at most 1024 bytes".into())
        );
    }
}
