//! The `kbf` binary end to end: the refusals exit 2 naming the flag, each built mode
//! reaches its own entry point, and an unbuilt mode says so.

use std::process::Command;

use kbf_cli::names;

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(BIN).args(args).output().expect("run kbf");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
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

/// Catches a help that is not printed, lists a mode more or less than the table's, or a
/// bare `kbf` that does not exit 2, and an unknown mode or `state` that is accepted.
#[test]
fn help_lists_the_five_modes_and_unknown_modes_exit_2() {
    let (code, out, _) = run(&["--help"]);
    assert_eq!(code, Some(0));
    let listed: Vec<&str> = out
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    let want: Vec<&str> = names::MODES.iter().map(|m| m.name).collect();
    assert_eq!(listed, want, "{out}");
    assert_eq!(listed.len(), 5);
    for argv in [&[][..], &["state"], &["no-such-mode"]] {
        let (code, _, err) = run(argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
    }
}

/// Catches a mode that is not built running something, exiting other than 2, printing
/// more than the one line, or printing on stdout: each exits 2 with one stderr line
/// naming the mode, whatever flags it is given.
#[test]
fn an_unbuilt_mode_exits_2_with_one_line() {
    for mode in names::MODES {
        if mode.entry != names::Entry::NotBuilt {
            continue;
        }
        let (code, out, err) = run(&[mode.name, "--any-flag=1", "positional"]);
        assert_eq!(code, Some(2), "{}: {err}", mode.name);
        assert_eq!(err, format!("{}\n", names::not_built_line(mode.name)));
        assert!(out.is_empty(), "{out}");
    }
}

/// The smoke test the README names: the command line in backticks after `Smoke test:`
/// in README.md. Catches a README that stops naming a command `kbf` runs, and a
/// `kbf all-in-one --help` that does not exit 0.
#[test]
fn the_readmes_smoke_test_runs() {
    let readme = include_str!("../../../README.md");
    let line = readme
        .lines()
        .find(|l| l.starts_with("Smoke test: `kbf "))
        .expect("the README has a smoke-test line");
    let cmd = line.split('`').nth(1).expect("a command in backticks");
    assert_eq!(cmd, "kbf all-in-one --help");
    let args: Vec<&str> = cmd.split_whitespace().skip(1).collect();
    let (code, out, err) = run(&args);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("Usage: kbf all-in-one"), "{out}");
}

#[cfg(target_os = "linux")]
/// Catches `kbf daemon` that does not reach the daemon's start: a daemon whose data
/// directory cannot be made exits 1 with the daemon's message.
#[test]
fn daemon_runs_the_daemons_start() {
    let (code, _, err) = run(&[
        names::DAEMON,
        "--driver=container",
        "--cas=https://127.0.0.1:1",
        "--id-files=/nonexistent/ids",
        "--data-dir=/proc/nonexistent/kbf",
        "--server=https://127.0.0.1:1",
        "--ca-cert=/nonexistent/ca.pem",
        "--cert=/nonexistent/node.pem",
        "--key=/nonexistent/node.key",
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
    let (code, _, err) = run(&[
        names::ALL_IN_ONE,
        "--driver=container",
        "--server=https://127.0.0.1:1",
        "--ca-cert=/nonexistent/ca.pem",
        "--cert=/nonexistent/node.pem",
        "--key=/nonexistent/node.key",
        "--node-id=n",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--cold-store=file:///proc/nonexistent/cold",
    ]);
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
