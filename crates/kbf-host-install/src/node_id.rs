//! The node's id, as `query` reports it: the name the farm knows this host by.
//!
//! It is read from the node's `kbf.toml` the way `kbf` reads it: `[node] id`, else the
//! `[identity] node-id` of a host that does not enroll, else the host's short name in lower
//! case (what an enrolling node uses when its configuration names none). A value that is not
//! a DNS label, or a configuration that is not root's alone to write, gives no id: the
//! client treats a host with no id as one it cannot tell from another.

use std::path::Path;

use toml::Value;

use crate::trust::Trust;

/// The id of this node, if the configuration at `config` (read as `trust` allows) or the
/// host name at `hostname` gives one.
pub fn read(trust: &Trust, config: &Path, hostname: &Path) -> Option<String> {
    let text = trust.read(config).ok()?;
    let table: Value = toml::from_str(&text).ok()?;
    let configured = table
        .get("node")
        .and_then(|node| node.get("id"))
        .or_else(|| table.get("identity")?.get("node-id"));
    match configured {
        Some(value) => value.as_str().filter(|id| is_label(id)).map(str::to_owned),
        None => from_hostname(hostname),
    }
}

/// The short host name in lower case, if that is a usable id.
fn from_hostname(hostname: &Path) -> Option<String> {
    let name = std::fs::read_to_string(hostname).ok()?;
    let short = name.trim().split('.').next()?.to_ascii_lowercase();
    is_label(&short).then_some(short)
}

/// `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`: a DNS label, which is what a node id is.
pub(crate) fn is_label(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

#[cfg(test)]
mod tests;
