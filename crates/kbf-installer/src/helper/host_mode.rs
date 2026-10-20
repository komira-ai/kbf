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
    match read_mode(layout)?.as_str() {
        "frontend" | "all-in-one" => Ok(()),
        other => Err(format!("{} mode is {other:?}", layout.kbf_toml())),
    }
}

/// The systemd unit that runs this host's mode: the first unit `setup` enabled for the `mode`
/// of its `kbf.toml`. It is how `run` learns what to restart when the service is not told
/// (`run` with neither `--unit` nor `--launchd-label`).
pub fn mode_unit(layout: &Layout) -> Result<&'static str, String> {
    use clap::ValueEnum;
    let mode = read_mode(layout)?;
    let shown = layout.kbf_toml();
    let known = crate::cli::Mode::from_str(&mode, false)
        .map_err(|_| format!("{shown} mode is {mode:?}, which is no mode of kbf"))?;
    first_unit(&shown, &mode, crate::setup::plan::mode_units(known))
}

/// The first of the `units` a mode is run by; why not when the mode has none.
fn first_unit(
    shown: &impl std::fmt::Display,
    mode: &str,
    units: &'static [&'static str],
) -> Result<&'static str, String> {
    units
        .first()
        .copied()
        .ok_or_else(|| format!("{shown} mode {mode:?} has no unit to restart"))
}

/// The top-level `mode` of the host's `kbf.toml`; why not when there is none.
fn read_mode(layout: &Layout) -> Result<String, String> {
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
        Some(mode) => Ok(mode.to_owned()),
        None => Err(format!("{shown} has no mode")),
    }
}

#[cfg(test)]
mod tests {
    use super::first_unit;

    /// Catches a mode whose unit list is empty panicking `run` (or being given a default unit)
    /// instead of being refused with the file and the mode named.
    ///
    /// Mutant planted: an empty list answered with a fixed unit.
    #[test]
    fn a_mode_with_no_unit_is_refused_naming_the_file_and_the_mode() {
        let why = first_unit(&"/etc/kbf/kbf.toml", "storage", &[]).expect_err("no unit");
        assert!(why.contains("/etc/kbf/kbf.toml"), "{why}");
        assert!(why.contains("\"storage\""), "{why}");
        assert_eq!(
            first_unit(&"/etc/kbf/kbf.toml", "storage", &["a.service", "b.service"]),
            Ok("a.service")
        );
    }
}
