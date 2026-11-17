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

/// Catches a frontend that starts with a flag of another mode (exit 0 or 1 instead of the
/// usage error 2), or a refusal that is clap's generic "unexpected argument" and does not name
/// the flag and say why.
#[test]
fn the_frontend_refuses_the_store_flags_with_exit_2() {
    for flag in [
        "--cold-store=file:///x",
        "--hot-store=file:///x",
        "--meta-dir=/x",
    ] {
        let (code, out, err) = run(&[names::FRONTEND, "--storage=http://127.0.0.1:1", flag]);
        let name = flag.split('=').next().unwrap_or(flag);
        assert_eq!(code, Some(2), "{flag}: {err}");
        assert!(
            err.contains(&format!("kbf {} refuses {name}: ", names::FRONTEND)),
            "{err}"
        );
        assert!(out.is_empty(), "{out}");
    }
}

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
    for argv in [&["--json"][..], &["--json", "storage"]] {
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
    for mode in [names::STORAGE, names::FRONTEND] {
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
fn help_lists_the_two_modes_and_unknown_modes_exit_2() {
    let (code, out, _) = run(&["--help"]);
    assert_eq!(code, Some(0));
    assert!(!out.contains("Commands:"), "{out}");
    let want: Vec<&str> = names::MODES.iter().map(|m| m.name).collect();
    assert!(
        out.contains(&format!("[possible values: {}]", want.join(", "))),
        "{out}"
    );
    for removed in names::REMOVED_MODES {
        assert!(!out.contains(removed), "{removed}: {out}");
    }
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
        &["--mode=storage", "extra"],
        &["--mode=storage", "node"],
        &["--mode=storage", "frontend"],
        &["storage", "frontend"],
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
/// `kbf --mode=storage --help` that does not exit 0.
#[test]
fn the_readmes_smoke_test_runs() {
    let readme = include_str!("../../../README.md");
    let line = readme
        .lines()
        .find(|l| l.starts_with("Smoke test: `kbf "))
        .expect("the README has a smoke-test line");
    let cmd = line.split('`').nth(1).expect("a command in backticks");
    assert_eq!(cmd, "kbf --mode=storage --help");
    let args: Vec<&str> = cmd.split_whitespace().skip(1).collect();
    let (code, out, err) = run(&args);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("Usage: kbf --mode=storage"), "{out}");
}

/// Catches a removed mode (daemon, storage-and-daemon, all-in-one) that starts, or a refusal
/// that does not name what replaces it: the mode asked for as the flag, as the flag with a
/// separate value, as the hidden word, with `--check`, with a flag of the mode and with the
/// help, each exits 2 with the one message and writes nothing on standard output.
#[test]
fn every_way_to_ask_for_a_removed_mode_exits_2_with_the_message() {
    for removed in names::REMOVED_MODES {
        let flag = format!("--mode={removed}");
        for argv in [
            vec![flag.as_str()],
            vec!["--mode", removed],
            vec![removed],
            vec![flag.as_str(), "--check"],
            vec![flag.as_str(), "--listen=127.0.0.1:0"],
            vec![flag.as_str(), "--help"],
            vec![removed, "--help"],
            vec!["--config=/nonexistent/kbf.toml", flag.as_str()],
        ] {
            let (code, out, err) = run(&argv);
            assert_eq!(code, Some(2), "{argv:?}: {err}");
            assert!(out.is_empty(), "{argv:?}: {out}");
            assert!(err.contains(kbf_cli::REMOVED), "{argv:?}: {err}");
        }
    }
    assert!(kbf_cli::REMOVED.contains("kbf runs no daemon any more"));
    assert!(kbf_cli::REMOVED.contains("--executor js-agent"));
    assert!(kbf_cli::REMOVED.contains("--mode=storage"));
    assert!(kbf_cli::REMOVED.contains("docs/deploy/kbf-modes.md"));
}

/// Catches `mode = "daemon"` (or either other removed mode) in `kbf.toml` that starts, or is
/// refused with another message: a bare `kbf` and `kbf --check` that read the file's mode exit 2
/// with the message, and the list of modes the refusal gives for other mistakes leaves them out.
#[test]
fn a_kbf_toml_with_a_removed_mode_is_refused() {
    let dir = scratch("removed_mode_in_the_file");
    let file = dir.join("kbf.toml");
    let write = |text: &str| {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(&file, text).expect("kbf.toml");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("mode");
    };
    let path = file.display().to_string();
    for removed in names::REMOVED_MODES {
        write(&format!("mode = \"{removed}\"\n"));
        for argv in [
            vec!["--config", path.as_str()],
            vec!["--config", path.as_str(), "--check"],
        ] {
            let (code, out, err) = run(&argv);
            assert_eq!(code, Some(2), "{removed} {argv:?}: {err}");
            assert!(out.is_empty(), "{removed} {argv:?}: {out}");
            assert!(err.contains(kbf_cli::REMOVED), "{removed} {argv:?}: {err}");
        }
    }
    write("mode = \"nonsense\"\n");
    let (code, _, err) = run(&["--config", path.as_str()]);
    assert_eq!(code, Some(2), "{err}");
    assert!(!err.contains(kbf_cli::REMOVED), "{err}");
}

/// Catches `kbf boot-check --mode=<removed>` that reaches the database step, and
/// `kbf setup --mode=<removed>` that does anything but refuse: both exit 2 with the
/// message, as the flag with one word or two, and `setup --apply` as a user that is not
/// root is refused for the mode first.
#[test]
fn boot_check_and_setup_refuse_a_removed_mode() {
    for removed in names::REMOVED_MODES {
        let flag = format!("--mode={removed}");
        for argv in [
            vec!["boot-check", flag.as_str()],
            vec!["boot-check", "--mode", removed],
            vec!["setup", flag.as_str()],
            vec!["setup", "--mode", removed, "--apply"],
        ] {
            let (code, out, err) = run(&argv);
            assert_eq!(code, Some(2), "{argv:?}: {err}");
            assert!(out.is_empty(), "{argv:?}: {out}");
            assert!(err.contains(kbf_cli::REMOVED), "{argv:?}: {err}");
        }
    }
    let (_, help, _) = run(&["boot-check", "--help"]);
    for removed in names::REMOVED_MODES {
        assert!(!help.contains(removed), "{removed}: {help}");
    }
    let (_, help, _) = run(&["setup", "--help"]);
    assert!(help.contains("frontend or storage"), "{help}");
    for removed in names::REMOVED_MODES {
        assert!(!help.contains(removed), "{removed}: {help}");
    }
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
    let data_flag = format!("--meta-dir={}", data.display());
    let config = dir.join("kbf.toml");
    let config_flag = format!("--config={}", config.display());
    let argvs: [&[&str]; 4] = [
        &[
            "--mode=storage",
            "--label",
            "pool=x",
            "--storage-listen=127.0.0.1:0",
            &data_flag,
        ],
        &["--mode=storage", "--label=pool=x"],
        &["--mode=storage", &config_flag, "--label", "pool=x"],
        &["--mode=frontend", "--label=pool=x"],
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
        &["--mode=storage", "--help"],
        &["--mode=frontend", "--help"],
        &["storage", "--help"],
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
    let data_flag = format!("--meta-dir={}", data.display());
    let argvs: [&[&str]; 2] = [
        &[
            "check-args",
            names::STORAGE,
            "--storage-listen=127.0.0.1:0",
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

/// `kbf package-hook` is the update hook of the native package, not a mode: a command line the
/// package manager would never give ends with exit 1 and the contract's last line, a step on a
/// params file that names a unit and a record directory reaches the hook's own steps (a `verify`
/// with no update prepared is refused as `not_prepared`, and the record directory is made), and
/// neither starts a mode or runs systemctl.
///
/// Catches: the word falling through to the mode parser (exit 2, no last line), which hostkeeper
/// reads as a hook that died.
#[test]
fn package_hook_answers_with_the_contracts_last_line() {
    let (code, out, _) = run(&[kbf_cli::PACKAGE_HOOK]);
    assert_eq!(code, Some(1));
    let last = out.lines().last().expect("a last line");
    assert!(last.contains("\"reason\":\"bad_arguments\""), "{out}");
    assert!(last.starts_with('{') && last.ends_with('}'), "{out}");

    let dir = scratch("package-hook");
    let state = dir.join("state");
    let params = dir.join("params.toml");
    std::fs::write(
        &params,
        format!(
            "unit = \"kbf-storage.service\"\nmode = \"storage\"\n\
             state_dir = \"{}\"\n\"updater.target_version\" = \"0.1.7\"\n",
            state.display()
        ),
    )
    .expect("params");
    let (code, out, _) = run(&[
        kbf_cli::PACKAGE_HOOK,
        "verify",
        "--params",
        &params.display().to_string(),
    ]);
    assert_eq!(code, Some(1));
    let last = out.lines().last().expect("a last line");
    assert!(last.contains("\"reason\":\"not_prepared\""), "{out}");
    assert!(state.is_dir());
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
