//! The static unit files of the kbf-installer package (`packaging/linux/units/`) against
//! this build's own `kbf` parser. The rules over the files are in
//! `crates/kbf-integration-test/tests/package_units.rs`; this layer has `kbf_cli`.
//!
//! Catches:
//! - a package unit for a mode `kbf` does not have (the files and `kbf_cli::names::MODES`
//!   drifting apart);
//! - a unit whose
//!   `ExecStart=` or `ExecStartPre=` `kbf` line is a command line `kbf <mode>` refuses;
//! - a unit whose `kbf boot-check` line is not the one `kbf boot-check` takes, for the unit's
//!   own mode, as the first `ExecStartPre=` and as root.

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
                .to_owned();
            (!["installer", "proxy"].contains(&mode.as_str()))
                .then(|| (mode, std::fs::read_to_string(&p).expect("read")))
        })
        .collect();
    out.sort();
    out
}

/// Every `kbf` command line of the unit (`ExecStart=` and `ExecStartPre=` that run the
/// package's `kbf`), as words after the program.
fn exec_lines(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .filter_map(|l| {
            let v = l
                .strip_prefix("ExecStart=")
                .or_else(|| l.strip_prefix("ExecStartPre="))?;
            let mut w = v
                .trim_start_matches('+')
                .split_whitespace()
                .map(str::to_owned);
            let program = w.next()?;
            program.ends_with("/kbf").then(|| w.collect())
        })
        .collect()
}

/// The command lines of the unit that start a mode or check it (`--mode=<mode> ...`): the
/// unit's `kbf` lines but its `boot-check`.
fn kbf_lines(text: &str) -> Vec<Vec<String>> {
    let mut lines = exec_lines(text);
    lines.retain(|w| w.first().is_none_or(|word| word != "boot-check"));
    lines
}

/// The `kbf boot-check` command lines of the unit, as words after the program.
fn boot_check_lines(text: &str) -> Vec<Vec<String>> {
    let mut lines = exec_lines(text);
    lines.retain(|w| w.first().is_some_and(|word| word == "boot-check"));
    lines
}

/// Catches a package unit for a mode `kbf` lacks, and a mode of `kbf` with no unit.
#[test]
fn the_package_has_a_unit_for_each_kbf_mode() {
    let have: Vec<String> = units().into_iter().map(|u| u.0).collect();
    let mut want: Vec<String> = MODES.iter().map(|m| m.name.to_owned()).collect();
    want.sort();
    assert_eq!(have, want);
}

/// Asserts that every mode unit runs `kbf boot-check` exactly once, as root (`+`) and as the
/// first `ExecStartPre=`, for its own mode and with no other flag, and that `kbf`'s own parser
/// takes the line and reads the unit's mode and the default `installer.toml` from it.
#[test]
fn each_package_unit_runs_one_boot_check_for_its_own_mode_that_kbfs_own_parser_takes() {
    let mut checked = 0;
    for (mode, text) in units() {
        let flag = format!("--mode={mode}");
        let want = [["boot-check", flag.as_str()]];
        assert_eq!(boot_check_lines(&text), want, "{mode}");
        let first = text
            .lines()
            .find_map(|l| l.strip_prefix("ExecStartPre="))
            .expect("a start check");
        let line = format!("+/usr/bin/kbf boot-check {flag}");
        assert_eq!(first, line, "{mode}");
        let argv = ["kbf", "boot-check", flag.as_str()].map(OsString::from);
        let parsed = kbf_cli::boot_check::parse(&argv).unwrap_or_else(|e| panic!("{mode}: {e}"));
        assert_eq!(parsed.mode, mode);
        let default = Path::new(kbf_cli::boot_check::DEFAULT_INSTALLER_CONFIG);
        assert_eq!(parsed.installer_config, default, "{mode}");
        checked += 1;
    }
    assert_eq!(checked, MODES.len(), "a mode unit was not checked");
}

/// Catches a unit command line `kbf <mode>` refuses: every `kbf` line of a package unit
/// (`ExecStartPre=` `--check` and `ExecStart=`) is parsed by kbf's own parser, the words
/// after `kbf` exactly as systemd passes them. A unit names few
/// flags and leaves the rest to `/etc/kbf/kbf.toml`, which a parser with no file reads as
/// missing flags, so that one refusal is let through; an unknown flag, another mode's
/// flag and a value kbf refuses are not. A line that names `--config` (the plain frontend
/// unit's start) reads a scratch `kbf.toml` in place of the host's.
#[test]
fn each_package_unit_command_line_parses_with_kbfs_own_parser() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "unit-lines");
    let config = frontend_config(&dir);
    for (mode, text) in units() {
        let lines = kbf_lines(&text);
        assert!(
            lines.len() >= 2,
            "{mode}: fewer kbf lines than a check and a start"
        );
        for line in lines {
            assert!(line[0].starts_with("--mode="), "{mode} {line:?}");
            let words: Vec<String> = line.clone();
            let argv: Vec<OsString> = if words.iter().any(|w| w == "--config") {
                argv_with(&words, &config)
            } else {
                std::iter::once("kbf".to_owned())
                    .chain(words)
                    .map(OsString::from)
                    .collect()
            };
            match kbf_cli::parse_from(argv) {
                Ok(_) => {}
                Err(e) if e.kind() == clap::error::ErrorKind::MissingRequiredArgument => {}
                Err(e) => panic!("{mode} {line:?}: {e}"),
            }
        }
    }
}

/// A `kbf.toml` that satisfies the frontend's required settings (`--storage`), written
/// 0600 in a scratch directory: the parser reads the file `--config` names, and the
/// host's own `/etc/kbf/kbf.toml` is not there in a test.
fn frontend_config(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("kbf.toml");
    std::fs::write(
        &path,
        "[storage-client]\nstorage = \"http://127.0.0.1:1\"\n",
    )
    .expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    path
}

/// `words` (the unit's words after the program, `--config /etc/kbf/kbf.toml` among them or
/// not) as `kbf`'s argv, with the host's config path replaced by `config` and a `--config
/// config` added where the line names none (the check line).
fn argv_with(words: &[String], config: &Path) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec!["kbf".into()];
    let mut named = false;
    let mut it = words.iter();
    while let Some(w) = it.next() {
        if w == "--config" {
            let _host_path = it.next();
            out.extend(["--config".into(), config.into()]);
            named = true;
        } else {
            out.push(w.into());
        }
    }
    if !named {
        out.extend(["--config".into(), config.into()]);
    }
    out
}

/// Catches the plain frontend unit's command lines (`kbf-frontend.service`, for a host
/// without the proxy) that `kbf`'s own parser refuses: both the `--check` line and the
/// start, with the host's `kbf.toml` replaced by one that stands for the frontend, must
/// parse to the frontend, the start not as a check. The defects, planted in the unit's
/// own start line, must each be refused by that parser: a mode that does not exist, and
/// `--instance` without `--handover-socket` (kbf refuses it).
#[test]
fn the_plain_frontend_unit_command_lines_parse_with_kbfs_own_parser() {
    let (_, text) = units()
        .into_iter()
        .find(|(m, t)| m == "frontend" && t.contains("/usr/bin/kbf --mode=frontend --config"))
        .expect("kbf-frontend.service");
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "plain-frontend-unit");
    let config = frontend_config(&dir);
    let lines = kbf_lines(&text);
    assert_eq!(lines.len(), 2, "a check and a start: {lines:?}");
    for (line, is_check) in lines.iter().zip([true, false]) {
        let parsed = kbf_cli::parse_with(argv_with(line, &config), None)
            .unwrap_or_else(|e| panic!("{line:?}: {e}"));
        assert!(
            matches!(parsed.invocation, kbf_cli::Invocation::Frontend(_)),
            "{line:?} is not the frontend"
        );
        assert_eq!(parsed.check, is_check, "{line:?}");
    }
    let start = lines[1].clone();
    let refused = |edit: &dyn Fn(&mut Vec<String>)| {
        let mut words = start.clone();
        edit(&mut words);
        kbf_cli::parse_with(argv_with(&words, &config), None).is_err()
    };
    assert!(
        refused(&|w| w[0] = "--mode=no-such-mode".into()),
        "an unknown mode parsed"
    );
    assert!(
        refused(&|w| w.push("--instance=a".into())),
        "--instance without --handover-socket parsed"
    );
}
