use std::cell::Cell;
use std::ffi::OsString;

use clap::Parser;

use super::*;
use crate::pilot::{GIT, MARKER};
use kbf_host::Step;
use kbf_host::exec::Entry;
use kbf_host::step::Perms;
use kbf_host::testkit::{FakeExec, RUNNER, reply, scratch};

const EXAMPLE: &str = include_str!("../../templates/pilot/config.example");

#[derive(Parser)]
struct Top {
    #[command(subcommand)]
    pilot: Pilot,
}

fn parse(args: &[&str]) -> Result<Pilot, clap::Error> {
    Top::try_parse_from(std::iter::once("pilot").chain(args.iter().copied())).map(|t| t.pilot)
}

/// The example written to a scratch config file, with `extra` lines appended.
fn config(name: &str, extra: &str) -> String {
    let dir = scratch(name);
    let path = dir.join("config.local");
    std::fs::write(&path, format!("{EXAMPLE}{extra}")).expect("write the config");
    path.to_string_lossy().into_owned()
}

fn env_home(key: &str) -> Option<OsString> {
    (key == "HOME").then(|| OsString::from("/srv/op"))
}

fn cli(
    exec: &mut FakeExec,
    env: &dyn Fn(&str) -> Option<OsString>,
    args: &[&str],
) -> (u8, String, String) {
    let pilot = parse(args).expect("valid flags");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let pauses = Cell::new(0);
    let code = run(
        pilot,
        exec,
        env,
        SystemTime::UNIX_EPOCH,
        &mut out,
        &mut err,
        &|| pauses.set(pauses.get() + 1),
    );
    let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
    let code = if code == ExitCode::SUCCESS {
        0
    } else if code == ExitCode::FAILURE {
        1
    } else {
        assert_eq!(code, ExitCode::from(2));
        2
    };
    (code, text(out), text(err))
}

/// Catches flags that clap takes when they must be refused: an install with no
/// binary, a relative path, `--uninstall` with install flags, and a worktree that is
/// not absolute.
#[test]
fn bad_flags_are_refused_by_the_parser() {
    let refused: [&[&str]; 6] = [
        &["server-install", "--config", "c"],
        &["server-install", "--config", "c", "--bin", "kbf-server"],
        &[
            "server-install",
            "--config",
            "c",
            "--uninstall",
            "--bin",
            "/opt/kbf-server",
        ],
        &[
            "server-install",
            "--config",
            "c",
            "--uninstall",
            "--pki",
            "/srv/pki",
        ],
        &["buckconfig", "--config", "c", "--worktree", "wt"],
        &["buckconfig", "--config", "c"],
    ];
    for args in refused {
        assert!(parse(args).is_err(), "{args:?}");
    }
    assert!(parse(&["server-install", "--config", "c", "--uninstall"]).is_ok());
}

/// Catches a refusal that exits 0 or 1, or is written to stdout without `REFUSED:`
/// (an operator's script would read it as a plan); and an unreadable config file or
/// a missing or relative HOME going unnoticed.
#[test]
fn a_refusal_exits_2_with_refused_on_stderr() {
    let cfg = config("pilot_cli_refused", "");
    let mut exec = FakeExec::new(0);
    let install = [
        "server-install",
        "--config",
        &cfg,
        "--bin",
        "/opt/kbf-server",
    ];
    let (code, out, err) = cli(&mut exec, &env_home, &install);
    assert_eq!((code, out.as_str()), (2, ""));
    assert_eq!(err, format!("REFUSED: {}\n", PilotError::IsRoot));
    let (code, _, err) = cli(&mut exec, &|_| None, &install);
    assert_eq!(
        (code, err.as_str()),
        (2, "REFUSED: HOME is not set in the environment\n")
    );
    let (code, _, err) = cli(&mut exec, &|_| Some(OsString::from("op")), &install);
    assert_eq!(
        (code, err.as_str()),
        (2, "REFUSED: HOME: \"op\" is not absolute\n")
    );
    let empty_xdg = |k: &str| match k {
        "HOME" => Some(OsString::from("/srv/op")),
        _ => Some(OsString::new()),
    };
    let (code, _, err) = cli(&mut exec, &empty_xdg, &install);
    assert_eq!(
        (code, err),
        (2, format!("REFUSED: {}\n", PilotError::IsRoot))
    );
    let (code, _, err) = cli(
        &mut exec,
        &env_home,
        &[
            "server-install",
            "--config",
            "/srv/none/config",
            "--bin",
            "/opt/kbf-server",
        ],
    );
    assert_eq!(code, 2);
    assert!(
        err.starts_with("REFUSED: cannot read /srv/none/config: "),
        "{err}"
    );
    let bad = config("pilot_cli_bad_key", "KBF_SERVER_HOSTS=x\n");
    let (code, _, err) = cli(
        &mut exec,
        &env_home,
        &["buckconfig", "--config", &bad, "--worktree", "/srv/wt"],
    );
    assert_eq!(code, 2);
    assert!(err.contains("unknown key KBF_SERVER_HOSTS"), "{err}");
    let (code, _, err) = cli(
        &mut exec,
        &env_home,
        &[
            "server-install",
            "--config",
            &cfg,
            "--bin",
            "/opt/kbf-server",
            "--bind",
            "x",
        ],
    );
    assert_eq!(
        (code, err.as_str()),
        (2, "REFUSED: bind address \"x\" is not an IPv4 address\n")
    );
}

/// Catches `--uninstall` and a relative XDG_CONFIG_HOME not reaching the verb: the
/// rollback reads only the state key, and runs as the user.
#[test]
fn uninstall_runs_through_the_command_line() {
    let cfg = config("pilot_cli_uninstall", "");
    let mut exec = FakeExec::new(1000);
    let (code, out, err) = cli(
        &mut exec,
        &env_home,
        &["server-install", "--config", &cfg, "--uninstall"],
    );
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(
        out.contains(
            "DRY-RUN: run /usr/bin/systemctl --user disable --now kbf-pilot-server.service"
        ),
        "{out}"
    );
    assert!(exec.log.is_empty());
    let xdg = |k: &str| match k {
        "HOME" => Some(OsString::from("/srv/op")),
        _ => Some(OsString::from("cfg")),
    };
    let (code, _, err) = cli(
        &mut exec,
        &xdg,
        &["server-install", "--config", &cfg, "--uninstall"],
    );
    assert_eq!(
        (code, err.as_str()),
        (2, "REFUSED: XDG_CONFIG_HOME: \"cfg\" is not absolute\n")
    );
    let path = scratch("pilot_cli_uninstall_state").join("c");
    std::fs::write(&path, "KBF_SERVER_STATE=rel\n").expect("write");
    let path = path.to_string_lossy().into_owned();
    let (code, _, err) = cli(
        &mut exec,
        &env_home,
        &["server-install", "--config", &path, "--uninstall"],
    );
    assert_eq!(
        (code, err.as_str()),
        (2, "REFUSED: KBF_SERVER_STATE: \"rel\" is not absolute\n")
    );
}

/// Catches an install whose host check fails not reaching the verb through the
/// command line, and a failed step that exits 2 (refused) instead of 1.
#[test]
fn server_install_runs_through_the_command_line() {
    let cfg = config("pilot_cli_install", "");
    let mut exec = FakeExec::new(1000);
    exec.answer = Box::new(|cmd| Ok(reply(1, "", &format!("{} not here", cmd.program))));
    let (code, _, err) = cli(
        &mut exec,
        &env_home,
        &[
            "server-install",
            "--config",
            &cfg,
            "--bin",
            "/opt/kbf-server",
        ],
    );
    assert_eq!(code, 2);
    assert_eq!(
        err,
        "REFUSED: tailscale0 does not carry 192.0.2.10 (ip -4 addr show dev tailscale0)\n"
    );
}

fn worktree(exec: &mut FakeExec) {
    exec.answer = Box::new(|cmd| {
        Ok(
            match (cmd.program.as_str(), cmd.args.get(3).map(String::as_str)) {
                (GIT, Some("--path-format=absolute")) => {
                    reply(0, "/srv/m/.git/worktrees/wt\n/srv/m/.git\n", "")
                }
                _ => reply(0, "", ""),
            },
        )
    });
    let entry = Entry::File {
        perms: Perms::new(0o644),
        owner: RUNNER,
        bytes: b"[cells]\n".to_vec(),
    };
    exec.entries
        .insert(HostPath::new("/srv/wt/.buckconfig").expect("path"), entry);
}

/// Catches the buckconfig arms not reaching their verbs (a dry run that writes, a
/// `--remove` that writes, an apply that does not), and a failed step that exits 2.
#[test]
fn buckconfig_writes_removes_and_fails_through_the_command_line() {
    let cfg = config("pilot_cli_buckconfig", "");
    let mut exec = FakeExec::new(1000);
    worktree(&mut exec);
    let write = ["buckconfig", "--config", &cfg, "--worktree", "/srv/wt"];
    let (code, out, _) = cli(&mut exec, &env_home, &write);
    assert_eq!(code, 0);
    assert!(
        out.ends_with("dry run: nothing changed. Re-run with --apply.\n"),
        "{out}"
    );
    assert!(exec.log.is_empty());
    exec.fail_at = Some(0);
    let apply = [
        "buckconfig",
        "--config",
        &cfg,
        "--worktree",
        "/srv/wt",
        "--apply",
    ];
    let (code, _, err) = cli(&mut exec, &env_home, &apply);
    assert_eq!(code, 1);
    assert!(
        err.starts_with("kbf-deploy: step 0 (write /srv/wt/.buckconfig.local"),
        "{err}"
    );
    exec.fail_at = None;
    exec.log.clear();
    let (code, _, _) = cli(&mut exec, &env_home, &apply);
    assert_eq!(code, 0);
    let target = HostPath::new("/srv/wt/.buckconfig.local").expect("path");
    assert!(
        matches!(exec.entries.get(&target), Some(Entry::File { bytes, .. }) if bytes.starts_with(MARKER.as_bytes()))
    );
    let remove = [
        "buckconfig",
        "--config",
        &cfg,
        "--worktree",
        "/srv/wt",
        "--remove",
        "--apply",
    ];
    let (code, _, _) = cli(&mut exec, &env_home, &remove);
    assert_eq!(code, 0);
    assert_eq!(exec.log.last(), Some(&Step::Remove { path: target }));
    let missing = config("pilot_cli_buckconfig_missing", "").replace("config.local", "none");
    let (code, _, err) = cli(
        &mut exec,
        &env_home,
        &["buckconfig", "--config", &missing, "--worktree", "/srv/wt"],
    );
    assert_eq!(code, 2);
    assert!(err.starts_with("REFUSED: cannot read"), "{err}");
    let empty = config("pilot_cli_buckconfig_empty", "").replace("config.local", "c2");
    std::fs::write(&empty, "").expect("write");
    let (code, _, err) = cli(
        &mut exec,
        &env_home,
        &["buckconfig", "--config", &empty, "--worktree", "/srv/wt"],
    );
    assert_eq!(
        (code, err.as_str()),
        (2, "REFUSED: KBF_SERVER_HOST is not set (config file)\n")
    );
}
