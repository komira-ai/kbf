//! How the command line is split before a mode parses it: the three flags `kbf` takes
//! itself (`--mode`, `--config`, `--label`), the end-of-flags word, an argument that is
//! not text, and the mode that comes from the file. Each case is a refusal that exits 2
//! naming its cause, or a parse to the value the words stand for.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use clap::error::ErrorKind;
use kbf_cli::{Invocation, config};

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("command-line-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn file(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("kbf.toml");
    std::fs::write(&path, text).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    path
}

fn words(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn refused(list: &[&str]) -> clap::Error {
    kbf_cli::parse_with(words(list), None).expect_err("refused")
}

const FRONTEND_FILE: &str = "\
[storage-client]
storage = \"http://127.0.0.1:1\"

[listener]
listen = \"127.0.0.1:9980\"
";

/// Catches a flag of ours given as the last word with no value being read as nothing (the
/// mode or file silently unset), or the refusal not naming the flag: each exits 2 with
/// "a value is required for '--<flag>'".
#[test]
fn a_flag_of_ours_with_no_value_is_refused_naming_it() {
    for flag in ["mode", "config", "label"] {
        let err = refused(&["kbf", "--mode=daemon", &format!("--{flag}")]);
        assert_eq!(err.exit_code(), 2, "{flag}");
        let msg = err.to_string();
        assert!(
            msg.contains(&format!(
                "a value is required for '--{flag}' but none was supplied"
            )),
            "{flag}: {msg}"
        );
    }
}

/// Catches a flag that only begins with one of ours (`--configfoo`) being taken for it: it
/// is an unknown flag, which clap refuses naming it, and the file it would have named is
/// never read.
#[test]
fn a_flag_that_only_starts_with_one_of_ours_is_an_unknown_flag() {
    for flag in [
        "--configfoo=/nonexistent/kbf.toml",
        "--modes=daemon",
        "--labels=a=b",
    ] {
        let err = refused(&["kbf", "--mode=daemon", flag]);
        assert_eq!(err.exit_code(), 2, "{flag}");
        let msg = err.to_string();
        assert!(
            msg.contains(flag.split('=').next().expect("a name")),
            "{flag}: {msg}"
        );
        assert!(
            !msg.contains("cannot read the config file"),
            "{flag}: {msg}"
        );
    }
}

/// Catches the end-of-flags word being ignored: the words after `--` are not ours, so
/// `--config=...` after it is not the settings file (reading it would name a file that
/// is not there); clap refuses it as a stray word.
#[test]
fn the_end_of_flags_word_ends_our_flags() {
    let err = refused(&[
        "kbf",
        "--mode=frontend",
        "--storage=http://127.0.0.1:1",
        "--",
        "--config=/nonexistent/kbf.toml",
    ]);
    assert_eq!(err.exit_code(), 2);
    assert!(
        !err.to_string().contains("cannot read the config file"),
        "{err}"
    );
}

/// Catches an argument that is not text panicking or being dropped: it reaches the mode's
/// parser, which refuses it (exit 2, an unexpected-argument error naming it), not a start with the word
/// missing.
#[test]
fn an_argument_that_is_not_text_is_refused_not_dropped() {
    let mut argv = words(&["kbf", "--mode=frontend", "--storage=http://127.0.0.1:1"]);
    argv.push(OsString::from_vec(vec![b'-', b'-', 0xff, 0xfe]));
    let err = kbf_cli::parse_with(argv, None).expect_err("refused");
    assert_eq!(err.exit_code(), 2);
    assert_eq!(err.kind(), ErrorKind::UnknownArgument, "{err}");
    assert!(err.to_string().contains('\u{fffd}'), "{err}");
}

/// Catches a file whose `mode` is not a mode being started as some mode, or refused
/// without naming the value and the file: exit 2, `mode = "..."`, the file, and the modes.
#[test]
fn a_mode_in_the_file_that_is_not_a_mode_is_refused() {
    let dir = scratch("bad_mode");
    let path = file(&dir, "mode = \"no-such-mode\"\n");
    let err = kbf_cli::parse_with(
        words(&["kbf", "--config", path.to_str().expect("utf8")]),
        None,
    )
    .expect_err("refused");
    assert_eq!(err.exit_code(), 2);
    assert_eq!(err.kind(), ErrorKind::InvalidValue);
    let msg = err.to_string();
    for part in [
        "mode = \"no-such-mode\"",
        &path.display().to_string(),
        "is not a mode (one of ",
        "frontend",
    ] {
        assert!(msg.contains(part), "{part}: {msg}");
    }
}

/// Catches a bare `kbf` with no file anywhere being refused without naming where the mode
/// would have come from: the message names `--mode` and the default settings file.
#[test]
fn no_mode_and_no_file_names_the_default_file() {
    let err = refused(&["kbf"]);
    assert_eq!(err.exit_code(), 2);
    let msg = err.to_string();
    assert!(msg.contains("--mode=<"), "{msg}");
    assert!(msg.contains(config::DEFAULT_PATH), "{msg}");
    assert!(msg.contains("there is no such file"), "{msg}");
}

/// Catches the file's mode being lost when flags are also given: `kbf --config FILE
/// --listen ADDR` with `mode = "frontend"` in the file starts the frontend, with the
/// flag's address and the file's storage, and says nothing of overriding a mode.
#[test]
fn a_mode_from_the_file_takes_flags_too() {
    let dir = scratch("file_mode_and_flags");
    let path = file(&dir, &format!("mode = \"frontend\"\n{FRONTEND_FILE}"));
    let parsed = kbf_cli::parse_with(
        words(&[
            "kbf",
            "--config",
            path.to_str().expect("utf8"),
            "--listen=127.0.0.1:9981",
        ]),
        None,
    )
    .expect("parses");
    let Invocation::Frontend(flags) = &parsed.invocation else {
        panic!("not the frontend: {:?}", parsed.invocation);
    };
    assert_eq!(flags.server.listen.to_string(), "127.0.0.1:9981");
    assert_eq!(flags.storage.uri().to_string(), "http://127.0.0.1:1/");
}

/// Catches `check_settings` accepting a name that is no mode (it would check nothing and
/// pass), and one that checks the wrong mode's keys: an unknown mode is refused naming it,
/// a file of a mode's settings passes for it and its bad key is named.
#[test]
fn check_settings_knows_the_modes() {
    let dir = scratch("check_settings");
    let good = file(&dir, FRONTEND_FILE);
    assert_eq!(
        kbf_cli::check_settings("no-such-mode", &good),
        Err("no-such-mode is not a mode".to_owned())
    );
    assert_eq!(kbf_cli::check_settings("frontend", &good), Ok(()));
    let bad = file(&dir, "[listener]\nlisten-typo = 1\n");
    let why = kbf_cli::check_settings("frontend", &bad).expect_err("bad key");
    assert!(why.contains("listener.listen-typo"), "{why}");
}

/// Catches the "no mode" refusal naming a file that is not the one looked for: with a
/// default settings file that is not there it names that file (the one an operator
/// would create), and a lone `--` and a stray word (nothing before them names a mode) is the same refusal,
/// not a crash or a mode.
#[test]
fn no_mode_names_the_default_file_given_and_a_lone_end_of_flags_is_no_mode() {
    let dir = scratch("no_mode_default");
    let absent = dir.join("absent.toml");
    let err = kbf_cli::parse_with(words(&["kbf"]), Some(&absent)).expect_err("no mode");
    assert_eq!(err.exit_code(), 2);
    let msg = err.to_string();
    assert!(msg.contains(&absent.display().to_string()), "{msg}");
    assert!(msg.contains("there is no such file"), "{msg}");

    let err = refused(&["kbf", "--", "stray"]);
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("no mode: give --mode=<"), "{err}");
}

/// Catches a settings file that cannot say what mode it names (not TOML, `mode` a number)
/// being passed over as "no mode" or started as some mode: the start is refused, exit 2,
/// and the message is the file's own (`file_mode`'s), naming the file.
#[test]
fn a_file_that_cannot_name_its_mode_is_refused_with_its_own_message() {
    let dir = scratch("unreadable_mode");
    for (text, want) in [
        ("mode = 3\n", "`mode` is a string naming one mode"),
        ("mode = = =\n", "is not valid TOML"),
    ] {
        let path = file(&dir, text);
        let err = kbf_cli::parse_with(
            words(&["kbf", "--config", path.to_str().expect("utf8")]),
            None,
        )
        .expect_err(text);
        assert_eq!(err.exit_code(), 2, "{text}");
        assert_eq!(err.kind(), ErrorKind::Io, "{text}");
        let msg = err.to_string();
        assert!(msg.contains(want), "{text}: {msg}");
        assert!(msg.contains(&path.display().to_string()), "{text}: {msg}");
    }
}
