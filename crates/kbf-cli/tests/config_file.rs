//! `kbf.toml` read in-process: what a file stands for ([`kbf_cli::parse_with`]), what it
//! is refused for, and the golden file of each mode, one full `kbf.toml` generated from
//! the mode's flag definitions (`tests/golden/kbf-<mode>.toml`).
//!
//! To rewrite the goldens after an intended change to a mode's flags:
//! `BLESS=1 cargo test -p kbf-cli --test config_file golden`
//!
//! The processes (a start from a file alone, the exit codes of `--check`) are
//! `tests/config_process.rs`.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use clap::error::ErrorKind;
use kbf_cli::{Invocation, config, names};
use kbf_testkit::dir::TestDir;

/// A fresh, empty scratch directory under the workspace's `target/`, removed when it drops.
fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("config-file-{name}"))
}

/// A file named `kbf.toml` in `dir` holding `text`, with permission bits `mode`.
fn file(dir: &Path, text: &str, mode: u32) -> PathBuf {
    let path = dir.join("kbf.toml");
    // A file of an earlier mode (read-only, say) is replaced, not written through.
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, text).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode");
    path
}

fn argv(words: &[&str], config: Option<&Path>) -> Vec<OsString> {
    let mut v: Vec<OsString> = words.iter().map(OsString::from).collect();
    if let Some(path) = config {
        v.push("--config".into());
        v.push(path.into());
    }
    v
}

/// The frontend a file stands for: `--storage` (required) and `--listen` in the file.
const FRONTEND_FILE: &str = "\
[storage-client]
storage = \"http://127.0.0.1:1\"

[listener]
listen = \"127.0.0.1:9980\"
";

fn frontend(invocation: &Invocation) -> &kbf_server::frontend::FrontendFlags {
    match invocation {
        Invocation::Frontend(flags) => flags,
        other => panic!("not the frontend: {other:?}"),
    }
}

/// Catches a file that is read but not applied: `kbf frontend --config FILE` with its
/// required `--storage` and `--listen` only in the file parses, to the file's values; the
/// same command line without the file is refused for the missing `--storage` (so it is
/// the file that satisfied it).
#[test]
fn a_mode_takes_every_setting_from_the_file_alone() {
    let dir = scratch("file_only");
    let path = file(&dir, FRONTEND_FILE, 0o600);
    let parsed =
        kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&path)), None).expect("parses");
    assert!(!parsed.check);
    let flags = frontend(&parsed.invocation);
    assert_eq!(flags.server.listen.to_string(), "127.0.0.1:9980");
    assert_eq!(flags.storage.uri().to_string(), "http://127.0.0.1:1/");
    let err = kbf_cli::parse_with(argv(&["kbf", "frontend"], None), None).expect_err("no storage");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument, "{err}");
}

/// Catches `no-memory-climb` missing from the file's keys (the file cannot turn the
/// climb off), or read the wrong way round: absent or `false` leaves the climb on, `true`
/// turns it off, for the frontend, the mode a server runs as.
#[test]
fn the_file_turns_the_memory_climb_off() {
    let dir = scratch("no_memory_climb");
    for (line, off) in [
        ("", false),
        ("no-memory-climb = false\n", false),
        ("no-memory-climb = true\n", true),
    ] {
        let text = format!("{FRONTEND_FILE}\n[memory]\n{line}");
        let path = file(&dir, &text, 0o600);
        let parsed =
            kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&path)), None).expect("parses");
        assert_eq!(
            frontend(&parsed.invocation).server.no_memory_climb,
            off,
            "{line:?}"
        );
    }
}

/// Catches a default config path that is ignored, or one that is required: `parse_with`
/// reads the default file when it exists and goes on without it when it does not.
#[test]
fn the_default_path_is_read_when_it_exists_and_skipped_when_it_does_not() {
    let dir = scratch("default_path");
    let path = file(&dir, FRONTEND_FILE, 0o600);
    let parsed = kbf_cli::parse_with(argv(&["kbf", "frontend"], None), Some(&path)).expect("file");
    assert_eq!(
        frontend(&parsed.invocation).server.listen.to_string(),
        "127.0.0.1:9980"
    );
    let parsed = kbf_cli::parse_with(
        argv(&["kbf", "frontend", "--storage=http://127.0.0.1:2"], None),
        Some(&dir.join("absent.toml")),
    )
    .expect("no file, the flags are enough");
    assert_eq!(
        frontend(&parsed.invocation).storage.uri().to_string(),
        "http://127.0.0.1:2/"
    );
}

/// Catches `--config FILE` naming a file that is not there being treated like an absent
/// default: an operator who named a file gets the refusal naming it.
#[test]
fn a_named_file_that_is_missing_is_refused() {
    let dir = scratch("named_missing");
    let missing = dir.join("nope.toml");
    let err = kbf_cli::parse_with(
        argv(
            &["kbf", "frontend", "--storage=http://127.0.0.1:2"],
            Some(&missing),
        ),
        None,
    )
    .expect_err("refused");
    assert!(
        err.to_string().contains(&missing.display().to_string()),
        "{err}"
    );
    assert_eq!(err.exit_code(), 2);
}

/// Catches the file winning over the command line (or the two clashing as a flag given
/// twice): `--listen` on the command line replaces the file's `listen`, and a setting
/// the command line does not give still comes from the file.
#[test]
fn a_flag_on_the_command_line_overrides_its_key() {
    let dir = scratch("override");
    let path = file(&dir, FRONTEND_FILE, 0o600);
    let parsed = kbf_cli::parse_with(
        argv(&["kbf", "frontend", "--listen=127.0.0.1:9981"], Some(&path)),
        None,
    )
    .expect("parses");
    let flags = frontend(&parsed.invocation);
    assert_eq!(flags.server.listen.to_string(), "127.0.0.1:9981");
    assert_eq!(
        flags.storage.uri().to_string(),
        "http://127.0.0.1:1/",
        "the file still supplies the rest"
    );
}

/// Catches an unknown key passing silently (a typo that leaves a setting at its default),
/// and a refusal that does not say which key or which file: a key of no flag, a table of
/// no group, a key in another flag's table, a key outside any table, and a flag the mode
/// refuses (`--cold-store` is no frontend setting) each exit 2 naming the key and the file.
#[test]
fn an_unknown_key_is_refused_naming_the_key_and_the_file() {
    let dir = scratch("unknown");
    for (text, key) in [
        ("[listener]\nlisten-typo = 1\n", "`listener.listen-typo`"),
        ("[nowhere]\nlisten = \"127.0.0.1:1\"\n", "`nowhere.listen`"),
        ("[tls]\nlisten = \"127.0.0.1:1\"\n", "`tls.listen`"),
        ("listen = \"127.0.0.1:1\"\n", "`listen`"),
        (
            "[stores]\ncold-store = \"file:///x\"\n",
            "`stores.cold-store`",
        ),
    ] {
        // A scalar before the first table is a top-level key; a table goes after the
        // storage table the frontend needs.
        let body = if text.starts_with('[') {
            format!("[storage-client]\nstorage = \"http://127.0.0.1:1\"\n\n{text}")
        } else {
            format!("{text}\n[storage-client]\nstorage = \"http://127.0.0.1:1\"\n")
        };
        let path = file(&dir, &body, 0o600);
        let err =
            kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&path)), None).expect_err(text);
        let msg = err.to_string();
        assert!(
            msg.contains("unknown key") || msg.contains("outside a table"),
            "{text}: {msg}"
        );
        assert!(msg.contains(key), "{text}: {msg}");
        assert!(msg.contains(&path.display().to_string()), "{text}: {msg}");
        assert_eq!(err.exit_code(), 2);
    }
}

/// Catches a refusal that stops at the first problem: two bad keys are both named.
#[test]
fn every_problem_of_the_file_is_named() {
    let dir = scratch("every_problem");
    let path = file(&dir, "[listener]\nfirst = 1\nsecond = 2\n", 0o600);
    let err = kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&path)), None).expect_err("bad");
    let msg = err.to_string();
    assert!(
        msg.contains("`listener.first`") && msg.contains("`listener.second`"),
        "{msg}"
    );
}

/// Catches a file that anyone but its owner may change being read: write bits for group
/// or other, in any combination, are refused naming the file and the fix; a file only
/// readable by them (0644) is fine, since nothing in it is a secret. Red before the check.
#[test]
fn a_file_writable_by_group_or_other_is_refused() {
    let dir = scratch("permissions");
    for mode in [0o660, 0o664, 0o620, 0o602, 0o606, 0o666, 0o777] {
        let path = file(&dir, FRONTEND_FILE, mode);
        let err = kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&path)), None)
            .expect_err(&format!("{mode:o}"));
        let msg = err.to_string();
        assert!(
            msg.contains("writable by group or other"),
            "{mode:o}: {msg}"
        );
        assert!(msg.contains(&path.display().to_string()), "{mode:o}: {msg}");
        assert!(msg.contains(&format!("{mode:04o}")), "{mode:o}: {msg}");
        assert_eq!(err.exit_code(), 2);
    }
    for mode in [0o600, 0o640, 0o644, 0o400, 0o440, 0o444] {
        let path = file(&dir, FRONTEND_FILE, mode);
        kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&path)), None)
            .unwrap_or_else(|e| panic!("{mode:o}: {e}"));
    }
}

/// Catches a config path that is a directory or a link to one (or any other file that is
/// not a regular file) being opened: refused as not a regular file.
#[test]
fn a_config_that_is_not_a_regular_file_is_refused() {
    let dir = scratch("not_regular");
    let err = kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&dir)), None).expect_err("dir");
    assert!(err.to_string().contains("not a regular file"), "{err}");
    let link = dir.join("link.toml");
    std::os::unix::fs::symlink(&dir, &link).expect("symlink");
    let err = kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&link)), None).expect_err("link");
    assert!(err.to_string().contains("not a regular file"), "{err}");
}

/// Catches a file that is not TOML being read as empty: refused, naming the file.
#[test]
fn a_file_that_is_not_toml_is_refused() {
    let dir = scratch("not_toml");
    let path = file(&dir, "this is = = not toml", 0o600);
    let err = kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&path)), None).expect_err("bad");
    let msg = err.to_string();
    assert!(
        msg.contains("not valid TOML") && msg.contains(&path.display().to_string()),
        "{msg}"
    );
}

/// Catches a file value skipping the flag's own validation (the same validation is the
/// point of mapping keys to flags): a listen address that is not an address is refused
/// as the flag refuses it; a table where a value goes, and an array for a flag that takes
/// one value, are refused naming the key.
#[test]
fn a_value_gets_the_validation_of_its_flag() {
    let dir = scratch("validation");
    let storage = "[storage-client]\nstorage = \"http://127.0.0.1:1\"\n\n";
    let bad = file(
        &dir,
        &format!("{storage}[listener]\nlisten = \"not an address\"\n"),
        0o600,
    );
    let err = kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&bad)), None).expect_err("bad");
    assert!(err.to_string().contains("--listen"), "{err}");
    for (value, want) in [
        ("[\"127.0.0.1:1\", \"127.0.0.1:2\"]", "takes one value"),
        ("{ a = 1 }", "expects a string or a number"),
        ("true", "invalid value 'true' for '--listen"),
    ] {
        let path = file(
            &dir,
            &format!("{storage}[listener]\nlisten = {value}\n"),
            0o600,
        );
        let err =
            kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&path)), None).expect_err(value);
        let msg = err.to_string();
        assert!(msg.contains(want), "{value}: {msg}");
        if !value.starts_with("true") {
            assert!(msg.contains("listener.listen"), "{value}: {msg}");
        }
    }
}

/// Catches a `label` key (a node label, removed: placement uses what the node reports)
/// read again, or refused like any unknown key without saying that it was removed and
/// what replaces it: in `[identity]`, where it used to be, in another table, and outside
/// any table, it exits 2 naming the key, the file and the one-line tombstone.
#[test]
fn a_label_key_is_refused_with_its_replacement() {
    let dir = scratch("label-key");
    let identity = "server = \"https://127.0.0.1:1\"\nca-cert = \"ca.pem\"\ncert = \"node.pem\"\nkey = \"node.key\"\nnode-id = \"n\"\n";
    for (body, key) in [
        (
            format!("[identity]\n{identity}label = [\"a=b\"]\n\n[daemon]\ndriver = \"native\"\n"),
            "`identity.label`",
        ),
        (
            format!("[identity]\n{identity}\n[daemon]\ndriver = \"native\"\nlabel = [\"a=b\"]\n"),
            "`daemon.label`",
        ),
        (
            format!("label = [\"a=b\"]\n[identity]\n{identity}\n[daemon]\ndriver = \"native\"\n"),
            "`label`",
        ),
    ] {
        let path = file(&dir, &body, 0o600);
        let err = kbf_cli::parse_with(argv(&["kbf", "daemon"], Some(&path)), None).expect_err(key);
        assert_eq!(err.exit_code(), 2, "{key}");
        let msg = err.to_string();
        assert!(msg.contains(key), "{key}: {msg}");
        assert!(msg.contains(&path.display().to_string()), "{key}: {msg}");
        assert!(
            msg.contains(
                "`label` was removed: placement uses what the node reports (OSFamily, ISA); delete the key"
            ),
            "{key}: {msg}"
        );
    }
}

/// Catches a repeatable flag read as one value, and a switch read as a value: `server`
/// (given once per frontend on the command line) is an array in the file, and a switch is
/// `true` to turn it on and `false` to leave it off, refused for anything else.
#[test]
fn a_repeatable_flag_is_an_array_and_a_switch_is_a_boolean() {
    let dir = scratch("shapes");
    let node = "[identity]\nserver = [\"https://127.0.0.1:1\", \"https://farm.example:1\"]\nca-cert = \"ca.pem\"\ncert = \"node.pem\"\nkey = \"node.key\"\nnode-id = \"n\"\n\n[daemon]\ndriver = \"native\"\n";
    let path = file(&dir, node, 0o600);
    let parsed = kbf_cli::parse_with(argv(&["kbf", "daemon"], Some(&path)), None).expect("parses");
    let text = format!("{:?}", parsed.invocation);
    assert!(
        text.contains(r#"["https://127.0.0.1:1", "https://farm.example:1"]"#),
        "{text}"
    );
    // `require-metal-toolchain` is a switch of the daemon.
    for (value, on) in [("true", true), ("false", false)] {
        let path = file(
            &dir,
            &format!("{node}\n[macos]\nrequire-metal-toolchain = {value}\n"),
            0o600,
        );
        let parsed = kbf_cli::parse_with(argv(&["kbf", "daemon"], Some(&path)), None).expect(value);
        let text = format!("{:?}", parsed.invocation);
        assert_eq!(
            text.contains("require_metal_toolchain: true"),
            on,
            "{value}: {text}"
        );
    }
    let path = file(
        &dir,
        &format!("{node}\n[macos]\nrequire-metal-toolchain = \"yes\"\n"),
        0o600,
    );
    let err =
        kbf_cli::parse_with(argv(&["kbf", "daemon"], Some(&path)), None).expect_err("not a bool");
    assert!(err.to_string().contains("takes true or false"), "{err}");
}

/// Catches `--check` and `--config` not parsed by every mode, and `--check` read as a
/// start: each mode takes both and reports `check` only when asked.
#[test]
fn every_mode_takes_check_and_config() {
    let dir = scratch("every_mode");
    let path = file(&dir, "", 0o600);
    let node = [
        "--driver=container",
        "--server=https://127.0.0.1:1",
        "--ca-cert=ca.pem",
        "--cert=node.pem",
        "--key=node.key",
        "--node-id=n",
    ];
    // `all-in-one` is refused before any file is read; binary.rs pins that.
    for mode in names::MODES.iter().filter(|m| m.name != names::ALL_IN_ONE) {
        let mut words = vec!["kbf", mode.name];
        match mode.name {
            names::FRONTEND => words.push("--storage=http://127.0.0.1:1"),
            names::STORAGE => {}
            _ => words.extend(node),
        }
        let plain = kbf_cli::parse_with(argv(&words, Some(&path)), None).expect(mode.name);
        assert!(!plain.check, "{}", mode.name);
        words.push("--check");
        let checked = kbf_cli::parse_with(argv(&words, Some(&path)), None).expect(mode.name);
        assert!(checked.check, "{}", mode.name);
    }
}

/// Catches a flag of the mode with no key in the file, and a key with no flag: each
/// mode's golden file, one full `kbf.toml` generated from the flag definitions, is
/// compared byte for byte. A flag added to a mode changes its golden (and fails
/// `config::tests::every_flag_of_every_mode_has_a_group` until it has a group), so the
/// reviewer sees the key. On a mismatch the actual file is printed.
#[test]
fn each_modes_golden_matches_the_flag_definitions() {
    let bless = std::env::var_os("BLESS").is_some();
    let mut stale = Vec::new();
    for mode in names::MODES {
        let actual = kbf_cli::config_template(mode.name).expect("a mode");
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(format!("kbf-{}.toml", mode.name));
        if bless {
            std::fs::write(&path, &actual).expect("bless");
            continue;
        }
        let golden = std::fs::read_to_string(&path).unwrap_or_default();
        if golden != actual {
            stale.push(format!("{}\n{actual}", path.display()));
        }
    }
    assert!(
        stale.is_empty(),
        "differs from its golden file:\n{}",
        stale.join("\n=====\n")
    );
}

/// Catches `--executor-locate` missing from the frontend's `kbf.toml`, or on in it: the template
/// generated from the flag definitions (not the golden file) names the key, off, and the golden
/// file carries the same line.
#[test]
fn kbf_toml_golden_names_executor_locate() {
    let template = kbf_cli::config_template("frontend").expect("a mode");
    let golden = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/kbf-frontend.toml"),
    )
    .expect("golden");
    for text in [&template, &golden] {
        let lines = text.lines().filter(|l| l.starts_with("executor-locate"));
        assert_eq!(lines.collect::<Vec<_>>(), ["executor-locate = false"]);
    }
}

/// Catches a golden whose keys the loader would refuse (a template that is not a config):
/// every mode's golden, as a file, is accepted by the loader up to what the flags
/// themselves refuse (a required flag still commented), never for a key. Every setting of
/// the mode appears in it, as a live line or a comment, so a key cannot be missing.
#[test]
fn each_modes_golden_is_a_file_its_mode_reads() {
    let dir = scratch("golden_loads");
    for mode in names::MODES {
        let text = kbf_cli::config_template(mode.name).expect("a mode");
        let path = file(&dir, &text, 0o600);
        if let Err(e) = kbf_cli::parse_with(argv(&["kbf", mode.name], Some(&path)), None) {
            let msg = e.to_string();
            assert!(
                !msg.contains(&path.display().to_string()),
                "{}: the file is refused: {msg}",
                mode.name
            );
        }
        // Every key is accepted: uncomment every `# key = value` line and read it again.
        let all: String = text
            .lines()
            .map(|l| {
                let live = l.strip_prefix("# ").filter(|rest| {
                    rest.split_once(" = ").is_some_and(|(k, _)| {
                        !k.is_empty()
                            && k.bytes()
                                .all(|b| b.is_ascii_lowercase() || b == b'-' || b.is_ascii_digit())
                    })
                });
                format!("{}\n", live.unwrap_or(l))
            })
            .collect();
        let path = file(&dir, &all, 0o600);
        if let Err(e) = kbf_cli::parse_with(argv(&["kbf", mode.name], Some(&path)), None) {
            let msg = e.to_string();
            assert!(
                !msg.contains("unknown key")
                    && !msg.contains("is in no group")
                    && !msg.contains("not valid TOML"),
                "{}: {msg}",
                mode.name
            );
        }
    }
}

/// Catches `check_file` and the in-process loader disagreeing about what a good file is.
#[test]
fn check_file_names_the_file() {
    let dir = scratch("check_file");
    let path = file(&dir, "", 0o664);
    let why = config::check_file(&path).expect_err("writable");
    assert!(why.contains(&path.display().to_string()), "{why}");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    assert_eq!(config::check_file(&path), Ok(()));
}

/// Catches `file_mode` taking a `mode` that is not a string (a number, a table) for a mode
/// or for none, or an unreadable file for a file with no mode: a non-string `mode` is
/// refused saying it is a string naming one mode; a file whose bytes are not text is
/// refused as unreadable, naming the file; one with no `mode` answers `None`.
#[test]
fn file_mode_reads_a_string_and_refuses_the_rest() {
    let dir = scratch("file_mode");
    let path = file(&dir, "mode = 3\n", 0o600);
    let why = config::file_mode(&path).expect_err("not a string");
    assert!(
        why.contains("`mode` is a string naming one mode")
            && why.contains(&path.display().to_string()),
        "{why}"
    );
    let path = file(&dir, "mode = \"daemon\"\n", 0o600);
    assert_eq!(config::file_mode(&path), Ok(Some("daemon".to_owned())));
    let path = file(&dir, "[node]\nid = \"a\"\n", 0o600);
    assert_eq!(config::file_mode(&path), Ok(None));

    let path = dir.join("kbf.toml");
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, [0xff, 0xfe, 0xfd]).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    let why = config::file_mode(&path).expect_err("not text");
    assert!(
        why.starts_with("cannot read the config file ")
            && why.contains(&path.display().to_string()),
        "{why}"
    );
    let err = kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&path)), None).expect_err("bad");
    assert!(
        err.to_string().contains("cannot read the config file"),
        "{err}"
    );
    assert_eq!(err.exit_code(), 2);
}

/// Catches enrollment's tables (`[node]`, `[enroll]`, ...) in the shared file being passed
/// to a mode as flags, or a key of them that enrollment does not read being let by: a
/// known key rides along without becoming a flag, an unknown one is refused naming
/// `table.key` and the file (a table that is no table is refused by
/// `a_value_where_an_enrollment_table_is_refused_naming_key_and_file`).
#[test]
fn enrollments_tables_are_in_the_file_but_are_not_flags() {
    let dir = scratch("own_tables");
    let with = |extra: &str| file(&dir, &format!("{FRONTEND_FILE}{extra}"), 0o600);
    let parse = |path: &Path| kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(path)), None);

    let path = with("[node]\nid = \"node-02\"\nzone = \"a\"\n[enroll]\ntoken_file = \"/t\"\n");
    let parsed = parse(&path).expect("enrollment's known keys are let through");
    assert_eq!(
        frontend(&parsed.invocation).server.listen.to_string(),
        "127.0.0.1:9980"
    );

    let path = with("[node]\nid = \"node-02\"\nbogus = 1\n");
    let err = parse(&path).expect_err("unknown key of an enrollment table");
    let msg = err.to_string();
    assert!(msg.contains("unknown key `node.bogus`"), "{msg}");
    assert!(msg.contains("enrollment's table"), "{msg}");
    assert!(msg.contains(&path.display().to_string()), "{msg}");
    assert_eq!(err.exit_code(), 2);
}

/// Catches a value where one of enrollment's tables is being skipped by the flags' check
/// (it would reach the mode as nothing, so `storage = "http://..."` meant as the
/// frontend's `--storage` is silently dropped): a top-level `storage`, `daemon`, `node`,
/// `tls` as a string, number or bool is refused naming the key and the file, in the
/// flags' parser, `--check`'s settings check and a `kbf <mode>` parse alike.
#[test]
fn a_value_where_an_enrollment_table_is_refused_naming_key_and_file() {
    let dir = scratch("own_scalar");
    for key in [
        "storage", "daemon", "node", "tls", "enroll", "dirs", "frontend",
    ] {
        for value in ["\"x\"", "1", "true"] {
            let path = file(&dir, &format!("{key} = {value}\n{FRONTEND_FILE}"), 0o600);
            let at = path.display().to_string();
            let err = kbf_cli::parse_with(argv(&["kbf", "frontend"], Some(&path)), None)
                .expect_err("a value for a table");
            let msg = err.to_string();
            assert!(
                msg.contains(&format!("`{key}`")) && msg.contains(&at),
                "{key} = {value}: {msg}"
            );
            assert_eq!(err.exit_code(), 2, "{key} = {value}");
            let why = kbf_cli::check_settings("frontend", &path).expect_err("check");
            assert!(
                why.contains(&format!("`{key}`")) && why.contains(&at),
                "{key} = {value}: {why}"
            );
        }
    }
}

/// A command with three settings, to read files against `config::file_args` directly:
/// `listen` and `storage` are in the real group table (`listener`, `storage-client`),
/// `zz-ungrouped` is in none, which no real mode allows (`config::tests` refuses it).
fn small_command() -> clap::Command {
    clap::Command::new("small")
        .arg(clap::Arg::new("listen").long("listen"))
        .arg(clap::Arg::new("zz-ungrouped").long("zz-ungrouped"))
}

fn small_args(text: &str, name: &str) -> Result<Vec<OsString>, String> {
    let dir = scratch(name);
    let path = file(&dir, text, 0o600);
    config::file_args(
        &small_command(),
        &std::collections::BTreeSet::new(),
        &path,
        &std::collections::BTreeSet::new(),
    )
}

/// Catches a file that cannot be read as text passing as an empty one: bytes that are not
/// UTF-8 are refused, naming the file (the metadata check passes, the read fails).
#[test]
fn a_file_that_is_not_text_is_refused() {
    let dir = scratch("not_text");
    let path = dir.join("kbf.toml");
    std::fs::write(&path, [0xff_u8, 0xfe, 0x00]).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    let err = config::file_args(
        &small_command(),
        &std::collections::BTreeSet::new(),
        &path,
        &std::collections::BTreeSet::new(),
    )
    .expect_err("not UTF-8");
    assert!(err.contains("cannot read the config file"), "{err}");
}

/// Catches a top-level key that is no group being dropped: a key outside a table that
/// names no flag of the mode is refused as unknown (not told to move under a group).
#[test]
fn a_top_level_key_that_is_no_setting_is_refused() {
    let err = small_args("nonsense = 1\n", "top_unknown").expect_err("unknown");
    assert!(err.contains("unknown key `nonsense`"), "{err}");
}

/// Catches the same name being read either way: a top-level `worker-listen` is a real
/// setting of another mode's `[listener]` but not a flag of this command, so it is unknown
/// here, while the command's own `listen` at top level is told its table. (Not `storage`:
/// that is enrollment's table name, whose scalar enrollment's own parser refuses.)
#[test]
fn a_top_level_flag_of_another_mode_is_unknown_and_of_this_one_is_misplaced() {
    let other = small_args("worker-listen = \"x\"\n", "top_other").expect_err("unknown");
    assert!(other.contains("unknown key `worker-listen`"), "{other}");
    let own = small_args("listen = \"x\"\n", "top_own").expect_err("misplaced");
    assert!(own.contains("belongs under [listener]"), "{own}");
}

/// Catches a flag with no group being read from any table: the key is refused as in no
/// group, naming the flag.
#[test]
fn a_flag_that_is_in_no_group_is_refused() {
    let err = small_args("[listener]\nzz-ungrouped = \"x\"\n", "ungrouped").expect_err("none");
    assert!(err.contains("--zz-ungrouped is in no group"), "{err}");
}

/// Catches a float value being refused or rounded: it becomes the flag's text as written.
#[test]
fn a_float_setting_becomes_its_flag() {
    let args = small_args("[listener]\nlisten = 1.5\n", "float").expect("float");
    assert_eq!(args, vec![OsString::from("--listen=1.5")]);
}
