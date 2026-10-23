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
    first_unit(&shown, &mode, crate::setup::plan::mode_units(known, false))
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
    let table = read_table(layout)?.ok_or_else(|| format!("{} is not there", layout.kbf_toml()))?;
    match table.get("mode").and_then(toml::Value::as_str) {
        Some(mode) => Ok(mode.to_owned()),
        None => Err(format!("{} has no mode", layout.kbf_toml())),
    }
}

/// The host's `kbf.toml` as a table; `None` when the file is not there, why not when it
/// cannot be read, is too big, or is not TOML.
pub(crate) fn read_table(layout: &Layout) -> Result<Option<toml::Table>, String> {
    let path = layout.join(&layout.kbf_toml());
    let shown = layout.kbf_toml();
    let mut bytes = Vec::new();
    match std::fs::File::open(&path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{shown} cannot be read: {e}")),
    }
    if bytes.len() as u64 > MAX_BYTES {
        return Err(format!("{shown} is over {MAX_BYTES} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{shown} is not UTF-8 text"))?;
    let table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("{shown} is not TOML: {}", e.message()))?;
    Ok(Some(table))
}

#[cfg(test)]
mod tests {
    use super::{first_unit, frontend_host};
    use crate::testkit::{host_root, mkdirs};

    /// Catches `frontend_host` accepting a mode that is not a frontend: exactly `frontend` and
    /// `all-in-one` are accepted, and every other mode string of kbf (`storage`,
    /// `storage-and-daemon`, `daemon`), an unknown one and a missing mode or file is refused,
    /// the refusal naming the mode it saw.
    ///
    /// Catches: `| "storage"` added to the accepted arm of `frontend_host` fails the
    /// `storage` row; any other mode added fails its own row.
    #[test]
    fn exactly_frontend_and_all_in_one_are_frontend_hosts() {
        // (file text, accepted, text the refusal must contain)
        let rows: [(Option<&str>, bool, &str); 8] = [
            (Some("mode = \"frontend\"\n"), true, ""),
            (Some("mode = \"all-in-one\"\n"), true, ""),
            (Some("mode = \"storage\"\n"), false, "\"storage\""),
            (
                Some("mode = \"storage-and-daemon\"\n"),
                false,
                "\"storage-and-daemon\"",
            ),
            (Some("mode = \"daemon\"\n"), false, "\"daemon\""),
            (Some("mode = \"bogus\"\n"), false, "\"bogus\""),
            (Some("listen = \"x\"\n"), false, "has no mode"),
            (None, false, "is not there"),
        ];
        for (index, (text, accepted, why)) in rows.into_iter().enumerate() {
            let (layout, root) = host_root(&format!("host-mode-{index}"));
            if let Some(text) = text {
                let path = layout.join(&layout.kbf_toml());
                mkdirs(&root, path.parent().expect("a directory"), 0o755);
                std::fs::write(&path, text).expect("write kbf.toml");
            }
            let got = frontend_host(&layout);
            if accepted {
                assert_eq!(got, Ok(()), "{text:?}");
            } else {
                let err = got.expect_err(&format!("{text:?} must be refused"));
                assert!(err.contains(why), "{text:?}: {err}");
            }
        }
    }

    /// Catches a mode whose unit list is empty panicking `run` (or being given a default unit)
    /// instead of being refused with the file and the mode named.
    ///
    /// Catches: an empty list answered with a fixed unit.
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
