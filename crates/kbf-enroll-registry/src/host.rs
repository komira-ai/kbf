//! What counts as a host name or address. The same rule as the CA's advertise-host check
//! (`kbf-ca`), so a host the registry lets through is one the CA signs for.

use std::net::IpAddr;

/// One DNS label: 1 to 63 of `a-z`, `0-9` and `-`, not starting or ending with `-`.
fn valid_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// A lower-case DNS name of at most 253 characters whose labels are valid (so no trailing
/// dot, no empty label) and whose last label is not all digits (so `1.2.3.4.5` is not taken
/// for a name).
fn is_dns_name(text: &str) -> bool {
    text.len() <= 253
        && text.split('.').all(valid_label)
        && !text
            .rsplit('.')
            .next()
            .is_some_and(|last| last.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether `text` is an IP address or a lower-case DNS name.
pub(crate) fn is_host(text: &str) -> bool {
    text.parse::<IpAddr>().is_ok() || is_dns_name(text)
}

/// Whether `text` is a token `names` entry: a host, or a suffix that starts with `.` and
/// continues as a DNS name (an address is no suffix). Case does not matter: matching
/// lower-cases it.
pub(crate) fn is_name_rule(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    match text.strip_prefix('.') {
        Some(suffix) => is_dns_name(suffix),
        None => is_host(&text),
    }
}

/// Whether `text` can begin a node id: it has a node id's characters, does not start with
/// `-` and is no longer than a node id.
pub(crate) fn is_node_prefix(text: &str) -> bool {
    (1..=63).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !text.starts_with('-')
}
