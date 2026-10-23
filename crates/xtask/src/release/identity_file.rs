//! The `kbf --version --json` identity files of a release (read by `release notes`): one JSON object naming the build it is for.

use std::path::Path;

/// The most a `kbf --version --json` file may hold.
pub(super) const IDENTITY_CAP: u64 = 8192;

/// The identity (`kbf --version --json`) the file `path` holds for the build of `commit`.
/// A file that is not the build's identity is a problem, not an unknown.
pub(super) fn identity_of(
    path: &Path,
    commit: &str,
    flag: &str,
) -> Result<serde_json::Value, String> {
    let at = |e: &dyn std::fmt::Display| format!("{flag} {}: {e}", path.display());
    let file = std::fs::File::open(path).map_err(|e| at(&e))?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::Read::take(file, IDENTITY_CAP + 1), &mut text)
        .map_err(|e| at(&e))?;
    if text.len() as u64 > IDENTITY_CAP {
        return Err(at(&format!("over {IDENTITY_CAP} bytes")));
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| at(&format!("not JSON: {e}")))?;
    let stamp = value["commit"].as_str();
    let short = &commit[..12];
    if stamp != Some(short) {
        return Err(at(&format!("names commit {stamp:?}, not {short}")));
    }
    Ok(value)
}

/// The whole number the identity `value` states under `key`: `None` when it lacks the
/// field (a build before it), an error when the field is not a whole number.
pub(super) fn whole_field(value: &serde_json::Value, key: &str) -> Result<Option<u64>, String> {
    match value.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{key} {v} is not a whole number")),
    }
}
