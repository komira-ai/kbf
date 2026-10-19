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
    let mut have: Vec<String> = units().into_iter().map(|u| u.0).collect();
    // The frontend has two units (the handover template and the plain one): one mode.
    have.dedup();
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
        .find(|(m, t)| m == "frontend" && t.contains("releases/current/kbf"))
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
