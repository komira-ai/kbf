//! Whether this host runs a frontend: the top-level `mode` of its `kbf.toml`
//! (docs/design/kbf-installer.md section 4: a `frontend_update` record is accepted only on a
//! host whose mode is `frontend` or `all-in-one`). Only that one key is read, and a file that
//! is missing, unreadable, too big or not TOML is a host that is not a frontend, with the
//! reason in the answer: a malformed `kbf.toml` is the node's to refuse, never a reason for
//! the helper to stop.

use std::io::Read;

use crate::layout::Layout;

/// The most of `kbf.toml` that is read.
const MAX_BYTES: u64 = 1 << 20;

/// `Ok` when the host's `kbf.toml` says `mode = "frontend"` or `"all-in-one"`; otherwise why not.
pub fn frontend_host(layout: &Layout) -> Result<(), String> {
    let path = layout.join(&layout.kbf_toml());
    let shown = layout.kbf_toml();
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("{shown} is not there"),
            _ => format!("{shown} cannot be read: {e}"),
        })?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(format!("{shown} is over {MAX_BYTES} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{shown} is not UTF-8 text"))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("{shown} is not TOML: {}", e.message()))?;
    match table.get("mode").and_then(toml::Value::as_str) {
        Some("frontend" | "all-in-one") => Ok(()),
        Some(other) => Err(format!("{shown} mode is {other:?}")),
        None => Err(format!("{shown} has no mode")),
    }
}
