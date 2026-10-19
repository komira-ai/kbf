//! The static unit files of the kbf-installer package (`packaging/linux/units/`) against
//! this build's own `kbf` parser. The rules over the files are in
//! `crates/kbf-it/tests/package_units.rs`; this layer has `kbf_cli`.
//!
//! Catches:
//! - a package unit for a mode `kbf` does not have (the files and `kbf_cli::names::MODES`
//!   drifting apart);
//! - (ignored until `kbf --mode=<mode> --check` exists, P12) a unit whose
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
            (!["envoy", "installer", "backup"].contains(&mode.as_str()))
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

/// Catches a unit command line `kbf <mode>` refuses. `kbf <mode> --check` is P12 and
/// `kbf node` and `kbf enroll` are #966, so this test is ignored until they land.
#[test]
#[ignore = "P12: `kbf --mode=<mode> --check` does not exist yet"]
fn each_package_unit_command_line_parses_with_kbfs_own_parser() {
    for (mode, text) in units() {
        let lines = kbf_lines(&text);
        assert!(
            lines.len() >= 2,
            "{mode}: fewer kbf lines than a check and a start"
        );
        for line in lines {
            // [--mode=M, flags...]; today's parser takes the mode as a word (the subcommand
            // forms stay as aliases), and %i is instance a.
            let m = line[0].strip_prefix("--mode=").expect("--mode=");
            let argv: Vec<OsString> = ["kbf", "check-args", m]
                .into_iter()
                .map(str::to_owned)
                .chain(line[1..].iter().map(|f| f.replace("%i", "a")))
                .map(OsString::from)
                .collect();
            kbf_cli::check_args(&argv).unwrap_or_else(|e| panic!("{mode} {line:?}: {e}"));
        }
    }
}
