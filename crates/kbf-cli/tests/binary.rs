//! The `kbf` binary end to end: the refusals exit 2 naming the flag, each built mode
//! reaches its own entry point.

use std::process::Command;

use kbf_cli::names;

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

fn run(args: &[&str]) -> (Option<i32>, String, String) {
    run_with(args, &[])
}

/// [`run`] with `env` added to the environment.
fn run_with(args: &[&str], env: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let out = Command::new(BIN)
        .args(args)
        .envs(env.iter().copied())
        .output()
        .expect("run kbf");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A daemon's certificate files as the startup self-checks want them (a certificate of the
/// daemon row for node `n`, its CA, the key private to this user) under a scratch
/// directory of `name`: the `--ca-cert`, `--cert` and `--key` flags. The tests below that
/// reach the mode's own start need a host the self-checks pass.
#[cfg(target_os = "linux")]
fn daemon_files(name: &str) -> ([String; 3], kbf_testkit::dir::TestDir) {
    use std::os::unix::fs::PermissionsExt as _;

    use rcgen::{ExtendedKeyUsagePurpose, KeyUsagePurpose, SanType};

    let dir = scratch(name);
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = rcgen::CertifiedIssuer::self_signed(ca, rcgen::KeyPair::generate().expect("key"))
        .expect("CA");
    let mut leaf = rcgen::CertificateParams::new(vec!["n".to_owned()]).expect("params");
    leaf.subject_alt_names.push(SanType::URI(
        "urn:kbf:role:daemon".to_owned().try_into().expect("IA5"),
    ));
    leaf.is_ca = rcgen::IsCa::ExplicitNoCa;
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    leaf.use_authority_key_identifier_extension = true;
    let key = rcgen::KeyPair::generate().expect("key");
    let cert = leaf.signed_by(&key, &ca).expect("signed");
    let write = |file: &str, text: String, mode: u32| {
        let path = dir.join(file);
        std::fs::write(&path, text).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode");
        path.display().to_string()
    };
    let flags = [
        format!("--ca-cert={}", write("ca.pem", ca.pem(), 0o644)),
        format!("--cert={}", write("node.pem", cert.pem(), 0o644)),
        format!("--key={}", write("node.key", key.serialize_pem(), 0o600)),
    ];
    (flags, dir)
}

/// Catches a build node that starts with a flag of another mode (exit 0 or 1 instead of
/// the usage error 2), or a refusal that is clap's generic "unexpected argument" and
/// does not name the flag and say why.
#[test]
fn the_daemon_refuses_the_store_flags_with_exit_2() {
    for flag in ["--cold-store=file:///x", "--hot-store", "--meta-dir=/x"] {
        let (code, out, err) = run(&[
            names::DAEMON,
            "--driver=container",
            "--server=https://127.0.0.1:1",
            "--ca-cert=ca.pem",
            "--cert=node.pem",
            "--key=node.key",
            "--node-id=n",
            flag,
        ]);
        let name = flag.split('=').next().unwrap_or(flag);
        assert_eq!(code, Some(2), "{flag}: {err}");
        assert!(
            err.contains(&format!("kbf {} refuses {name}: ", names::DAEMON)),
            "{err}"
        );
        assert!(out.is_empty(), "{out}");
    }
}

/// The last line of the daemon's refused start (the same text `kbf-daemon` prints).
#[cfg(target_os = "linux")]
const DAEMON_REFUSAL: &str =
    "kbf-daemon: --data-dir: make /proc/nonexistent/kbf: No such file or directory (os error 2)";

/// Catches a `kbf --version` that names the package version alone: the artifacts
/// workflow's build-commit check wants `kbf <package version>+<commit>`, the stamp of
/// the server library it links.
#[test]
fn version_names_the_commit_built() {
    let (code, out, err) = run(&["--version"]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, format!("kbf {}\n", kbf_server::SERVER_VERSION));
    assert!(out.contains('+'), "{out}");
}

/// Catches a `kbf --version --json` that is not one JSON object on one line of stdout,
/// that lacks or adds a field (a host reads these to check what it runs, so the set is
/// the contract), that reports another commit or protocol than the libraries the binary
/// links carry, or that calls a debug build a release (or the reverse).
#[test]
fn version_json_reports_the_build_identity() {
    for argv in [["--version", "--json"], ["--json", "--version"]] {
        let (code, out, err) = run(&argv);
        assert_eq!(code, Some(0), "{argv:?}: {err}");
        assert!(err.is_empty(), "{err}");
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(out.ends_with('\n'), "{out:?}");
        let v: serde_json::Value = serde_json::from_str(&out).expect("one JSON value");
        let fields: Vec<&str> = v
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        // `commit_full` is printed by a build that knows its full commit, and by no other.
        let known = !kbf_server::BUILD_COMMIT_FULL.is_empty();
        let full = known.then_some("commit_full");
        let mut want = vec![
            "admin_protocol",
            "commit",
            "meta_log",
            "min_protocol",
            "profile",
            "protocol",
            "relay",
            "seq",
            "state_schema",
            "version",
        ];
        want.extend(full);
        want.sort_unstable();
        assert_eq!(fields, want, "{out}");
        assert_eq!(v["relay"], kbf_cli::version::RELAY);
        assert_eq!(v["commit"], kbf_server::BUILD_COMMIT);
        assert_eq!(
            v["commit_full"].as_str().unwrap_or_default(),
            kbf_server::BUILD_COMMIT_FULL
        );
        assert_eq!(v["admin_protocol"], 1);
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        assert_eq!(v["profile"], profile);
        assert_eq!(v["meta_log"], kbf_server::raft_log::RUNS.get());
        assert_eq!(v["protocol"], kbf_server::worker::PROTOCOL_VERSION);
        assert_eq!(
            v["min_protocol"],
            kbf_server::worker::OLDEST_PROTOCOL_VERSION
        );
        assert!(v["state_schema"].as_u64().is_some_and(|n| n >= 1), "{out}");
        // A number, whatever the checkout the test was built in: 0 where it could not count.
        assert!(v["seq"].is_u64(), "{out}");
    }
}

/// Catches `--json` taken as a flag of its own: without `--version` it is a usage error
/// (exit 2, the flag named, nothing on stdout), so a typo never reads as the identity.
#[test]
fn json_alone_is_a_usage_error() {
    for argv in [&["--json"][..], &["--json", "daemon"]] {
        let (code, out, err) = run(argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(out.is_empty(), "{argv:?}: {out}");
        assert!(err.contains("--json"), "{argv:?}: {err}");
    }
}

/// Catches a built mode that takes a flag it does not have, or exits other than 2 for
/// it.
#[test]
fn an_unknown_flag_is_a_usage_error_2() {
    for mode in [names::DAEMON, names::STORAGE_AND_DAEMON] {
        let (code, out, err) = run(&[mode, "--no-such-flag"]);
        assert_eq!(code, Some(2), "{mode}: {err}");
        assert!(
            err.contains("unexpected argument '--no-such-flag'"),
            "{err}"
        );
        assert!(out.is_empty(), "{out}");
    }
}

/// Catches a help that is not printed, lists a mode more or less than the table's (they
/// are the values of `--mode`, in the table's order), lists the old words as commands,
/// or a bare `kbf` with no config that does not exit 2 naming both places a mode comes
/// from; and an unknown mode, a word, or `state` that is accepted.
#[test]
fn help_lists_the_five_modes_and_unknown_modes_exit_2() {
    let (code, out, _) = run(&["--help"]);
    assert_eq!(code, Some(0));
    assert!(!out.contains("Commands:"), "{out}");
    let want: Vec<&str> = names::MODES
        .iter()
        .map(|m| m.name)
        .filter(|name| *name != names::ALL_IN_ONE)
        .collect();
    assert!(
        out.contains(&format!("[possible values: {}]", want.join(", "))),
        "{out}"
    );
    assert!(!out.contains("all-in-one"), "{out}");
    for mode in &want {
        assert!(
            !out.lines().any(|l| l.starts_with(&format!("  {mode} "))),
            "{mode} is listed as a command: {out}"
        );
    }
    for argv in [
        &["state"][..],
        &["no-such-mode"],
        &["--mode=state"],
        &["--mode", "no-such-mode"],
    ] {
        let (code, _, err) = run(argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
    }
    let (code, out, err) = run(&["--config", "/nonexistent/kbf.toml"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("--mode=<")
            && err.contains("mode = ")
            && err.contains("/nonexistent/kbf.toml"),
        "{err}"
    );
}

/// Catches an alias that stops working or shows in the help, and a mode flag that does not
/// reach the same mode: for each mode `kbf <mode> --help` and `kbf --mode=<mode> --help`
/// print the same page (exit 0, usage in the flag form), and `kbf --help` lists no alias.
#[test]
fn each_alias_works_hidden_and_the_flag_form_is_the_same_page() {
    let (_, top, _) = run(&["--help"]);
    for mode in names::MODES.iter().filter(|m| m.name != names::ALL_IN_ONE) {
        let (code, alias, err) = run(&[mode.name, "--help"]);
        assert_eq!(code, Some(0), "{}: {err}", mode.name);
        let (code, flag, err) = run(&[&format!("--mode={}", mode.name), "--help"]);
        assert_eq!(code, Some(0), "{}: {err}", mode.name);
        assert_eq!(alias, flag, "{}", mode.name);
        assert!(
            flag.contains(&format!("Usage: kbf --mode={}", mode.name)),
            "{flag}"
        );
        assert!(
            !top.lines()
                .any(|l| l.trim_start().starts_with(&format!("{} ", mode.name))
                    && !l.contains("--mode")),
            "{} is listed in the help: {top}",
            mode.name
        );
    }
}

/// Catches a positional argument other than the hidden aliases being accepted: the words
/// `node` and `enroll` (removed), a word after the mode flag, and a second mode.
#[test]
fn positional_words_are_refused_with_exit_2() {
    for argv in [
        &["node"][..],
        &["enroll"],
        &["--mode=daemon", "extra"],
        &["--mode=daemon", "node"],
        &["--mode=storage", "daemon"],
        &["storage", "daemon"],
    ] {
        let (code, out, err) = run(argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(out.is_empty(), "{out}");
        assert!(!err.is_empty(), "{argv:?}");
    }
    let (_, _, err) = run(&["node"]);
    assert!(err.contains("unrecognized subcommand 'node'"), "{err}");
}

/// The smoke test the README names: the command line in backticks after `Smoke test:`
/// in README.md. Catches a README that stops naming a command `kbf` runs, and a
/// `kbf --mode=storage-and-daemon --help` that does not exit 0.
#[test]
fn the_readmes_smoke_test_runs() {
    let readme = include_str!("../../../README.md");
    let line = readme
        .lines()
        .find(|l| l.starts_with("Smoke test: `kbf "))
        .expect("the README has a smoke-test line");
    let cmd = line.split('`').nth(1).expect("a command in backticks");
    assert_eq!(cmd, "kbf --mode=storage-and-daemon --help");
    let args: Vec<&str> = cmd.split_whitespace().skip(1).collect();
    let (code, out, err) = run(&args);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.contains("Usage: kbf --mode=storage-and-daemon"),
        "{out}"
    );
}

#[cfg(target_os = "linux")]
/// Catches `kbf daemon` that does not reach the daemon's start: a daemon whose data
/// directory cannot be made exits 1 with the daemon's message.
#[test]
fn daemon_runs_the_daemons_start() {
    let ([ca, cert, key], _files) = daemon_files("daemon_runs_the_daemons_start");
    let (code, _, err) = run(&[
        names::DAEMON,
        "--driver=native",
        "--cas=https://127.0.0.1:1",
        "--data-dir=/proc/nonexistent/kbf",
        "--server=https://127.0.0.1:1",
        &ca,
        &cert,
        &key,
        "--node-id=n",
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert_eq!(err.lines().last(), Some(DAEMON_REFUSAL));
}

/// Catches an all-in-one start that is not refused, or a refusal that does not name what
/// replaces the mode: the mode asked for as the flag, as the flag with a separate value,
/// as the hidden word, with `--check`, with a flag of the mode and with the help, each
/// exits 2 with the one message and writes nothing on standard output.
#[test]
fn every_way_to_ask_for_all_in_one_exits_2_with_the_message() {
    for argv in [
        &["--mode=all-in-one"][..],
        &["--mode", "all-in-one"],
        &["all-in-one"],
        &["--mode=all-in-one", "--check"],
        &["--mode=all-in-one", "--listen=127.0.0.1:0"],
        &["--mode=all-in-one", "--help"],
        &["all-in-one", "--help"],
        &["--config=/nonexistent/kbf.toml", "--mode=all-in-one"],
    ] {
        let (code, out, err) = run(argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(out.is_empty(), "{argv:?}: {out}");
        assert!(err.contains(kbf_cli::ALL_IN_ONE_REMOVED), "{argv:?}: {err}");
    }
    assert!(kbf_cli::ALL_IN_ONE_REMOVED.contains("--mode=frontend"));
    assert!(kbf_cli::ALL_IN_ONE_REMOVED.contains("--mode=storage-and-daemon"));
    assert!(kbf_cli::ALL_IN_ONE_REMOVED.contains("docs/deploy/kbf-modes.md"));
}

/// Catches `mode = "all-in-one"` in `kbf.toml` that starts, or is refused with another
/// message: a bare `kbf` and `kbf --check` that read the file's mode exit 2 with the
/// message, and the list of modes the refusal gives for other mistakes leaves it out.
#[test]
fn a_kbf_toml_with_the_all_in_one_mode_is_refused() {
    let dir = scratch("all_in_one_in_the_file");
    let file = dir.join("kbf.toml");
    let write = |text: &str| {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(&file, text).expect("kbf.toml");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("mode");
    };
    write("mode = \"all-in-one\"\n");
    let path = file.display().to_string();
    for argv in [
        vec!["--config", path.as_str()],
        vec!["--config", path.as_str(), "--check"],
    ] {
        let (code, out, err) = run(&argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(out.is_empty(), "{argv:?}: {out}");
        assert!(err.contains(kbf_cli::ALL_IN_ONE_REMOVED), "{argv:?}: {err}");
    }
    write("mode = \"nonsense\"\n");
    let (code, _, err) = run(&["--config", path.as_str()]);
    assert_eq!(code, Some(2), "{err}");
    assert!(!err.contains("all-in-one"), "{err}");
}

/// Catches `kbf boot-check --mode=all-in-one` that reaches the database step, and
/// `kbf setup --mode=all-in-one` that does anything but refuse: both exit 2 with the
/// message, as the flag with one word or two, and `setup --apply` as a user that is not
/// root is refused for the mode first.
#[test]
fn boot_check_and_setup_refuse_all_in_one() {
    for argv in [
        &["boot-check", "--mode=all-in-one"][..],
        &["boot-check", "--mode", "all-in-one"],
        &["setup", "--mode=all-in-one"],
        &["setup", "--mode", "all-in-one", "--apply"],
    ] {
        let (code, out, err) = run(argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(out.is_empty(), "{argv:?}: {out}");
        assert!(err.contains(kbf_cli::ALL_IN_ONE_REMOVED), "{argv:?}: {err}");
    }
    let (_, help, _) = run(&["boot-check", "--help"]);
    assert!(!help.contains("all-in-one"), "{help}");
    let (_, help, _) = run(&["setup", "--help"]);
    assert!(
        help.contains("[possible values: frontend, storage, daemon, storage-and-daemon]"),
        "{help}"
    );
}

/// Catches the daemon's helper re-execs read as `kbf`'s own command line: a keeper
/// started from this binary (the container driver re-runs the daemon's own executable
/// with `__keeper`) answers as the keeper does, exit 2 and its usage line, not as a
/// usage error about modes.
#[test]
fn the_binary_answers_as_a_keeper_before_it_reads_its_command_line() {
    let (code, out, err) = run(&["__keeper", "groups"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(
        err.starts_with("kbf-keeper: usage: __keeper group"),
        "{err}"
    );
    assert!(out.is_empty(), "{out}");
}

/// A fresh scratch directory under the workspace's `target/`, removed when it drops.
fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// Catches `--label` (a node label, removed: placement uses what the node reports) coming
/// back as a flag of the binary: a start that gives it exits 2 with the tombstone naming the
/// flag and the way out, nothing on stdout and nothing started (no data directory made, no
/// enrolment), with or without a mode's flags, whatever the spelling; and `kbf --help` and
/// each mode's help do not list it.
#[test]
fn the_label_flag_is_not_a_flag_of_kbf() {
    let dir = scratch("label_retired");
    let data = dir.join("data");
    let data_flag = format!("--data-dir={}", data.display());
    let config = dir.join("kbf.toml");
    let config_flag = format!("--config={}", config.display());
    let argvs: [&[&str]; 4] = [
        &[
            "--mode=daemon",
            "--label",
            "pool=x",
            "--driver=container",
            "--server=https://127.0.0.1:1",
            "--ca-cert=ca.pem",
            "--cert=node.pem",
            "--key=node.key",
            "--node-id=n",
            &data_flag,
        ],
        &["--mode=daemon", "--label=pool=x"],
        &["--mode=daemon", &config_flag, "--label", "pool=x"],
        &["--mode=storage-and-daemon", "--label=pool=x"],
    ];
    for argv in argvs {
        let (code, out, err) = run(argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(out.is_empty(), "{argv:?}: {out}");
        assert!(
            err.contains(
                "--label was removed: placement uses what the node reports (OSFamily, ISA); delete the flag"
            ),
            "{argv:?}: {err}"
        );
        assert!(!data.exists(), "{argv:?} started: {} made", data.display());
        assert!(!config.exists(), "{argv:?} wrote {}", config.display());
    }
    for help in [
        &["--help"][..],
        &["--mode=daemon", "--help"],
        &["--mode=storage-and-daemon", "--help"],
        &["daemon", "--help"],
    ] {
        let (code, out, err) = run(help);
        assert_eq!(code, Some(0), "{help:?}: {err}");
        assert!(!out.contains("--label"), "{help:?}: {out}");
    }
}

/// Catches `check-args` coming back as a word of `kbf`: it was `--check`'s parse stage for
/// `kbf-deploy`, with a deprecation note, and is retired. Now it is refused as any word
/// that is not a mode is (exit 2, nothing on stdout, nothing started), with or without a
/// mode after it, and `kbf --help` and `-h` do not name it.
#[test]
fn check_args_is_not_a_word_of_kbf() {
    let dir = scratch("check_args_retired");
    let data = dir.join("data");
    let data_flag = format!("--data-dir={}", data.display());
    let argvs: [&[&str]; 2] = [
        &[
            "check-args",
            names::DAEMON,
            "--driver=container",
            "--server=https://127.0.0.1:1",
            "--ca-cert=ca.pem",
            "--cert=node.pem",
            "--key=node.key",
            "--node-id=n",
            &data_flag,
        ],
        &["check-args"],
    ];
    for argv in argvs {
        let (code, out, err) = run(argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(out.is_empty(), "{argv:?}: {out}");
        assert!(!err.contains("deprecated"), "{argv:?}: {err}");
        assert!(!data.exists(), "{argv:?} started: {} made", data.display());
    }
    for flag in ["--help", "-h"] {
        let (code, out, err) = run(&[flag]);
        assert_eq!(code, Some(0), "{flag}: {err}");
        assert!(!out.contains("check-args"), "{flag}: {out}");
        assert!(!err.contains("check-args"), "{flag}: {err}");
    }
}

/// Catches `kbf frontend` with flags no longer being the mode, and `kbf frontend handover`
/// (a command of the retired `kbf-frontend@.service` template) coming back: the word is
/// refused like any positional argument (exit 2) and nothing is started or run.
#[test]
fn frontend_stays_a_mode_and_handover_is_not_a_command() {
    let (code, out, err) = run(&[names::FRONTEND, "handover"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(!err.contains("kbf frontend handover"), "{err}");
    let (code, _, err) = run(&[names::FRONTEND, "--listen=127.0.0.1:0"]);
    assert_eq!(
        code,
        Some(2),
        "a frontend with no --storage is refused: {err}"
    );
    assert!(!err.contains("handover"), "{err}");
}

/// `kbf update-hook` is the update hook of the kbf service, not a mode: a command line the
/// updater would never give ends with exit 1 and the contract's last line, and a step on a
/// record directory that is not there ends the same way and says which. Neither starts a mode
/// or touches the host.
///
/// Catches: the word falling through to the mode parser (exit 2, no last line), which the
/// generic updater reads as a hook that died.
#[test]
fn update_hook_answers_with_the_contracts_last_line() {
    let (code, out, _) = run(&[kbf_cli::UPDATE_HOOK]);
    assert_eq!(code, Some(1));
    let last = out.lines().last().expect("a last line");
    assert!(last.contains("\"reason\":\"bad_arguments\""), "{out}");
    assert!(last.starts_with('{') && last.ends_with('}'), "{out}");

    let dir = scratch("update-hook");
    let params = dir.join("params.toml");
    std::fs::write(
        &params,
        "\"updater.target_version\" = \"2.0.0\"\n\"updater.artifact_digest\" = \
         \"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n",
    )
    .expect("params");
    let missing = dir.join("no-record-directory");
    let (code, out, _) = run(&[
        kbf_cli::UPDATE_HOOK,
        "--record-dir",
        &missing.display().to_string(),
        "--channel",
        "stable",
        "--client-cert",
        "/unused/cert.pem",
        "--client-key",
        "/unused/key.pem",
        "--ca",
        "/unused/ca.pem",
        "verify",
        "--params",
        &params.display().to_string(),
    ]);
    assert_eq!(code, Some(1));
    assert!(out.contains("\"reason\":\"record_unusable\""), "{out}");
    assert!(!missing.exists());

    // A record directory that is there: the real ports are wired next, and a host with no
    // certificate files ends the step as the environment's fault, naming the file.
    let (code, out, _) = run(&[
        kbf_cli::UPDATE_HOOK,
        "--record-dir",
        &dir.display().to_string(),
        "--channel",
        "stable",
        "--client-cert",
        "/unused/cert.pem",
        "--client-key",
        "/unused/key.pem",
        "--ca",
        "/unused/ca.pem",
        "verify",
        "--params",
        &params.display().to_string(),
    ]);
    assert_eq!(code, Some(1));
    assert!(out.contains("\"reason\":\"world\""), "{out}");
    assert!(out.contains("/unused/cert.pem"), "{out}");
}
