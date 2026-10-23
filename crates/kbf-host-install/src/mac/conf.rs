//! The node's settings file, `av2_mac.conf`: the shell form `KEY=VALUE` the Mac kit reads,
//! read here as data and never run.
//!
//! The file is strict: every line is blank, a `#` comment, or `KEY=VALUE` with a key of
//! capital letters, digits and `_`, and a value that is either bare (letters, digits and
//! `. _ : / @ % + , = -`) or in one pair of quotes with none of `' " \ $` or a backtick
//! inside. A line of any other shape makes the whole file unreadable, since a file the
//! kit's shell would read differently than this must not be believed. `NODE_ID` is the one
//! key used, and it must be a DNS label; it must appear once.

use std::path::Path;

use crate::node_id::is_label;
use crate::trust::Trust;

/// The id of this node, if `path` (read as `trust` allows) names one.
pub fn node_id(trust: &Trust, path: &Path) -> Option<String> {
    parse_node_id(&trust.read(path).ok()?)
}

/// The `NODE_ID` of settings `text`, if the text is well formed and holds one.
pub fn parse_node_id(text: &str) -> Option<String> {
    let mut id = None;
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let key_ok = key.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
            && key
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        if !key_ok {
            return None;
        }
        let value = plain(value)?;
        if key == "NODE_ID" && id.replace(value).is_some() {
            return None;
        }
    }
    id.filter(|id| is_label(id)).map(str::to_owned)
}

/// The value of a bare or quoted word, or none for anything a shell would read as more.
fn plain(value: &str) -> Option<&str> {
    let bare = |text: &str| {
        text.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/@%+,=-".contains(&b))
    };
    for quote in ['\'', '"'] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            let clean = !inner
                .chars()
                .any(|c| c.is_control() || matches!(c, '\'' | '"' | '\\' | '$' | '`'));
            return clean.then_some(inner);
        }
    }
    bare(value).then_some(value)
}
