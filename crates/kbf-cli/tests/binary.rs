//! The `kbf` binary end to end: the refusals exit 2 naming the flag, each built mode
//! reaches its own entry point.

use std::process::Command;

use kbf_cli::names;

const BIN: &str = env!("CARGO_BIN_EXE_kbf");
#[cfg(target_os = "linux")]
const FAKE_CRUN: &str = env!("CARGO_BIN_EXE_kbf-cli-fake-crun");

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

/// The same for the combined binary's refused start.
#[cfg(target_os = "linux")]
const ALL_IN_ONE_REFUSAL: &str = "kbf-all-in-one: server: --meta-dir is required: the metadata log lives in the hot store's directory when --hot-store is a file:// store, and nowhere else is given";

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
        assert_eq!(
            fields,
            [
                "admin_protocol",
                "commit",
                "min_protocol",
                "profile",
                "protocol",
                "state_schema",
                "version"
            ],
            "{out}"
        );
        assert_eq!(v["commit"], kbf_server::BUILD_COMMIT);
        assert_eq!(v["admin_protocol"], kbf_server::managed::ADMIN_PROTOCOL);
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        assert_eq!(v["profile"], profile);
        assert_eq!(v["protocol"], kbf_server::worker::PROTOCOL_VERSION);
        assert_eq!(
            v["min_protocol"],
            kbf_server::worker::OLDEST_PROTOCOL_VERSION
        );
        assert!(v["state_schema"].as_u64().is_some_and(|n| n >= 1), "{out}");
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
    for mode in [names::DAEMON, names::ALL_IN_ONE] {
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
    let want: Vec<&str> = names::MODES.iter().map(|m| m.name).collect();
    assert!(
        out.contains(&format!("[possible values: {}]", want.join(", "))),
        "{out}"
    );
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
    for mode in names::MODES {
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
/// `kbf --mode=all-in-one --help` that does not exit 0.
#[test]
fn the_readmes_smoke_test_runs() {
    let readme = include_str!("../../../README.md");
    let line = readme
        .lines()
        .find(|l| l.starts_with("Smoke test: `kbf "))
        .expect("the README has a smoke-test line");
    let cmd = line.split('`').nth(1).expect("a command in backticks");
    assert_eq!(cmd, "kbf --mode=all-in-one --help");
    let args: Vec<&str> = cmd.split_whitespace().skip(1).collect();
    let (code, out, err) = run(&args);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("Usage: kbf --mode=all-in-one"), "{out}");
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

#[cfg(target_os = "linux")]
/// Catches `kbf all-in-one` that does not reach the combined start: the server half
/// refuses a cold store that cannot be made (exit 2, as in `kbf-all-in-one`).
#[test]
fn all_in_one_runs_both_halves_start() {
    let ([ca, cert, key], _files) = daemon_files("all_in_one_runs_both_halves_start");
    // The container driver's host, as the self-checks want it: a `crun` that answers
    // `--version` (never run otherwise; the start stops at the cold store).
    let dir = scratch("all_in_one_runs_both_halves_start_host");
    let crun = dir.join("crun");
    std::os::unix::fs::symlink(FAKE_CRUN, &crun).expect("link crun");
    let runtime = format!("--oci-runtime={}", crun.display());
    let (code, _, err) = run_with(
        &[
            names::ALL_IN_ONE,
            "--driver=container",
            "--server=https://127.0.0.1:1",
            &ca,
            &cert,
            &key,
            "--node-id=n",
            &runtime,
            "--listen=127.0.0.1:0",
            "--worker-listen=127.0.0.1:0",
            "--cold-store=file:///proc/nonexistent/cold",
        ],
        &[],
    );
    assert_eq!(code, Some(2), "{err}");
    assert_eq!(err.lines().last(), Some(ALL_IN_ONE_REFUSAL));
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

/// Catches `kbf check-args` starting the mode it checks (the daemon's start makes its
/// `--data-dir`; here the directory must stay absent), printing anything for a command
/// line that parses, and exiting other than 0 for one: each built mode's own flags,
/// with a data directory that a start would make.
#[test]
fn check_args_parses_and_starts_nothing() {
    let dir = scratch("check_args_starts_nothing");
    let data = dir.join("data");
    let data_flag = format!("--data-dir={}", data.display());
    for mode in [names::DAEMON, names::ALL_IN_ONE] {
        let (code, out, err) = run(&[
            kbf_cli::CHECK_ARGS,
            mode,
            "--driver=container",
            "--server=https://127.0.0.1:1",
            "--ca-cert=/nonexistent/ca.pem",
            "--cert=/nonexistent/node.pem",
            "--key=/nonexistent/node.key",
            "--node-id=n",
            &data_flag,
        ]);
        assert_eq!((code, out.as_str()), (Some(0), ""), "{mode}");
        // Only the deprecation note (tests/config_process.rs).
        assert_eq!(err.lines().count(), 1, "{mode}: {err}");
        assert!(err.contains("deprecated"), "{mode}: {err}");
        assert!(!data.exists(), "{mode} started: {} made", data.display());
    }
}

/// Catches `kbf check-args` passing what a start refuses: a flag no mode has, another
/// mode's flag (named, as the start names it), a mode not built yet (every start of it
/// exits 2), `--help` (which prints and starts nothing), no mode, and an unknown one.
/// Each exits 2 with the message on stderr and nothing on stdout.
#[test]
fn check_args_exits_2_with_the_refusal_a_start_gets() {
    let daemon = [
        names::DAEMON,
        "--driver=container",
        "--server=https://127.0.0.1:1",
        "--ca-cert=ca.pem",
        "--cert=node.pem",
        "--key=node.key",
        "--node-id=n",
    ];
    let with = |extra: &'static str| -> Vec<&'static str> {
        std::iter::once(kbf_cli::CHECK_ARGS)
            .chain(daemon.iter().copied())
            .chain([extra])
            .collect()
    };
    let bogus = with("--bogus-flag");
    let cold = with("--cold-store=file:///x");
    let help = with("--help");
    let rows: [(&[&str], &str); 6] = [
        (&bogus, "unexpected argument '--bogus-flag'"),
        (&cold, "kbf daemon refuses --cold-store: "),
        (
            &[kbf_cli::CHECK_ARGS, names::STORAGE_AND_DAEMON],
            "required arguments were not provided",
        ),
        (&help, "--help and --version print and exit 0"),
        (&[kbf_cli::CHECK_ARGS], "Usage: kbf"),
        (&[kbf_cli::CHECK_ARGS, "no-such-mode"], "no-such-mode"),
    ];
    for (argv, want) in rows {
        let (code, out, err) = run(argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(err.contains(want), "{argv:?}: {err}");
        assert!(out.is_empty(), "{argv:?}: {out}");
    }
}

/// Catches `check-args` shown to an operator as if it were a mode or a flag: neither
/// `kbf --help` nor `kbf -h` names it anywhere (it is for `kbf-deploy`, and its
/// absence from the help is what keeps the five-mode list exact).
#[test]
fn help_does_not_name_check_args() {
    for flag in ["--help", "-h"] {
        let (code, out, err) = run(&[flag]);
        assert_eq!(code, Some(0), "{flag}: {err}");
        assert!(!out.contains(kbf_cli::CHECK_ARGS), "{flag}: {out}");
        assert!(!err.contains(kbf_cli::CHECK_ARGS), "{flag}: {err}");
    }
}

/// Catches `kbf frontend handover` read as the frontend mode (exit 2 for a word it does
/// not take, or a start of the frontend), and `kbf frontend` with flags no longer being
/// the mode. The handover reaches its own entry: with a `systemctl` that is not there it
/// exits 1 naming the program, starting nothing; a flag of the command that is not
/// allowed (`--poll-ms` without `--wait`) and a flag of the mode (`--listen`) exit 2.
#[test]
fn frontend_handover_is_a_command_and_frontend_stays_a_mode() {
    let (code, out, err) = run(&[
        names::FRONTEND,
        "handover",
        "--systemctl=/nonexistent/systemctl",
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(out.is_empty(), "{out}");
    assert_eq!(
        err,
        "kbf frontend handover: cannot run /nonexistent/systemctl: No such file or directory (os error 2)\n"
    );
    for args in [
        &[names::FRONTEND, "handover", "--poll-ms=5"][..],
        &[names::FRONTEND, "handover", "--listen=127.0.0.1:1"][..],
    ] {
        let (code, out, err) = run(args);
        assert_eq!(code, Some(2), "{args:?}: {err}");
        assert!(out.is_empty(), "{out}");
        assert!(err.contains("kbf frontend handover"), "{err}");
    }
    let (code, _, err) = run(&[names::FRONTEND, "--listen=127.0.0.1:0"]);
    assert_eq!(
        code,
        Some(2),
        "a frontend with no --storage is refused: {err}"
    );
    assert!(!err.contains("handover"), "{err}");
}
