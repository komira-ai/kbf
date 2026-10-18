//! The static unit files of the kbf-installer package (`packaging/linux/units/`) against
//! this build's own `kbf` parser. The rules over the files are in
//! `crates/kbf-it/tests/package_units.rs`; this layer has `kbf_cli`.
//!
//! Catches:
//! - a package unit for a mode `kbf` does not have (the files and `kbf_cli::names::MODES`
//!   drifting apart);
//! - a unit whose
//!   `ExecStart=` or `ExecStartPre=` `kbf` line is a command line `kbf <mode>` refuses.

use std::ffi::OsString;
use std::path::Path;

use kbf_cli::names::MODES;

fn units() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux/units");
    let mut out: Vec<_> = std::fs::read_dir(&dir)
        .expect("packaging/linux/units")
        .map(|e| e.expect("entry").path())
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?.to_owned();
            let mode = name
                .strip_prefix("kbf-")?
                .strip_suffix(".service")?
                .trim_end_matches('@')
                .to_owned();
            (!["envoy", "installer"].contains(&mode.as_str()))
                .then(|| (mode, std::fs::read_to_string(&p).expect("read")))
        })
        .collect();
    out.sort();
    out
}

/// The `kbf` command lines of the unit (`ExecStart=` and `ExecStartPre=` that run the
/// installer's `kbf`), as words after the program.
fn kbf_lines(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .filter_map(|l| {
            let v = l
                .strip_prefix("ExecStart=")
                .or_else(|| l.strip_prefix("ExecStartPre="))?;
            let mut w = v.split_whitespace().map(str::to_owned);
            let program = w.next()?;
            program.ends_with("/kbf").then(|| w.collect())
        })
        .collect()
}

/// Catches a package unit for a mode `kbf` lacks, and a mode of `kbf` with no unit.
#[test]
fn the_package_has_a_unit_for_each_kbf_mode() {
    let have: Vec<String> = units().into_iter().map(|u| u.0).collect();
    let mut want: Vec<String> = MODES.iter().map(|m| m.name.to_owned()).collect();
    want.sort();
    assert_eq!(have, want);
}

/// Catches a unit command line `kbf <mode>` refuses: every `kbf` line of a package unit
/// (`ExecStartPre=` `--check` and `ExecStart=`) is parsed by kbf's own parser, the words
/// after `kbf` exactly as systemd passes them (`%i` is instance a). A unit names few
/// flags and leaves the rest to `/etc/kbf/kbf.toml`, which a parser with no file reads as
/// missing flags, so that one refusal is let through; an unknown flag, another mode's
/// flag and a value kbf refuses are not.
#[test]
fn each_package_unit_command_line_parses_with_kbfs_own_parser() {
    for (mode, text) in units() {
        let lines = kbf_lines(&text);
        assert!(
            lines.len() >= 2,
            "{mode}: fewer kbf lines than a check and a start"
        );
        for line in lines {
            assert!(line[0].starts_with("--mode="), "{mode} {line:?}");
            let argv: Vec<OsString> = std::iter::once("kbf".to_owned())
                .chain(line.iter().map(|f| f.replace("%i", "a")))
                .map(OsString::from)
                .collect();
            match kbf_cli::parse_from(argv) {
                Ok(_) => {}
                Err(e) if e.kind() == clap::error::ErrorKind::MissingRequiredArgument => {}
                Err(e) => panic!("{mode} {line:?}: {e}"),
            }
        }
    }
}
