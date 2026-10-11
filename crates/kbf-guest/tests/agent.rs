//! The agent over a Unix socket pair, running real commands: what a run reports, how
//! it ends, and what it leaves behind.

mod common;

use std::time::{Duration, Instant};

use common::{Served, Shares, act, alive, program, user_shell, wait_for_file};
use kbf_guest::wire::{End, OutputKind};

/// Catches an exit status or a terminating signal lost or swapped on the way to the
/// host (`Exited(15)` for a SIGTERM, or a shell-style 128+n).
#[test]
fn the_exit_code_and_the_signal_are_carried() {
    let shares = Shares::new("exit-code");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    client.run(&act("exit 3", &[])).expect("started");
    assert_eq!(client.wait().expect("exited").end, End::Exited(3));
    drop(client);
    served.join();

    let shares = Shares::new("exit-signal");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    client.run(&act("kill-self 15", &[])).expect("started");
    let exit = client.wait().expect("exited");
    assert_eq!(exit.end, End::Signaled(libc::SIGTERM));
    assert!(!exit.stragglers);
}

/// Catches stdout and stderr swapped, altered (a NUL or a missing newline), or
/// written to after the exit was reported: a process the command left in its group
/// writes a second later, and must have been killed before `Exited` was sent.
/// A user's shell on purpose: the late writer is a `&` job in the command's group,
/// and a NUL cannot travel in an argv.
#[test]
fn stdout_and_stderr_are_byte_exact_and_end_at_the_exit() {
    let shares = Shares::new("streams");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    let script = "printf 'out\\000bin\\n'; printf 'err' >&2; \
                  (sleep 1; echo late; echo late >&2) & exit 0";
    client.run(&user_shell(script, &[])).expect("started");
    let exit = client.wait().expect("exited");
    assert_eq!(exit.end, End::Exited(0));
    assert!(!exit.stragglers);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(shares.out("stdout"), b"out\0bin\n");
    assert_eq!(shares.out("stderr"), b"err");
}

/// Catches a kill of the leader alone: a grandchild that ignores SIGTERM and would
/// outlive its parent must be gone by the time `Exited(Killed)` arrives.
/// A user's shell on purpose: a trap, a `&` job and `$$` make the grandchild.
#[test]
fn kill_ends_a_grandchild_that_ignores_sigterm() {
    let shares = Shares::new("kill-grandchild");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    let pid_file = shares.outputs.join("grandchild.pid");
    let script = "sh -c 'trap \"\" TERM; echo $$ > \"$PIDFILE\"; exec sleep 60' & wait";
    let pidfile = pid_file.to_str().expect("utf-8 path");
    let leader = client
        .run(&user_shell(script, &[("PIDFILE", pidfile)]))
        .expect("started");
    let grandchild: i32 = wait_for_file(&pid_file).parse().expect("pid");
    assert!(alive(grandchild));
    client.killer().expect("killer").kill().expect("kill sent");
    let exit = client.wait().expect("exited");
    assert_eq!(exit.end, End::Killed);
    assert!(!exit.stragglers, "the group did not empty");
    assert!(
        !alive(grandchild),
        "grandchild {grandchild} survived the kill"
    );
    assert!(
        !alive(i32::try_from(leader).expect("pid")),
        "leader survived"
    );
}

/// Catches a timeout that is not enforced, or enforced on the leader only.
/// A user's shell on purpose: the child is a `&` job, its pid read from `$!`.
#[test]
fn the_timeout_kills_the_group() {
    let shares = Shares::new("timeout");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    let pid_file = shares.outputs.join("child.pid");
    let pidfile = pid_file.to_str().expect("utf-8 path");
    let mut req = user_shell(
        "sleep 60 & echo $! > \"$PIDFILE\"; wait",
        &[("PIDFILE", pidfile)],
    );
    req.timeout_ms = 300;
    let started = Instant::now();
    client.run(&req).expect("started");
    let exit = client.wait().expect("exited");
    assert_eq!(exit.end, End::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(exit.usage.wall_micros >= 300_000, "{:?}", exit.usage);
    let child: i32 = wait_for_file(&pid_file).parse().expect("pid");
    assert!(!alive(child), "the timed-out group's child survived");
}

/// Catches a dropped connection that leaves the command running: with no host to
/// report to, the group is killed and the agent goes on to the next connection.
#[test]
fn a_dropped_connection_kills_the_group() {
    let shares = Shares::new("dropped");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    let leader = client.run(&act("sleep 60", &[])).expect("started");
    drop(client);
    let agent = served.join();
    assert!(agent.has_run());
    assert!(
        !alive(i32::try_from(leader).expect("pid")),
        "the command outlived its host"
    );
}

/// Catches a working directory not resolved inside the inputs share.
#[test]
fn the_command_runs_in_its_working_directory() {
    let shares = Shares::new("cwd");
    std::fs::create_dir(shares.inputs.join("sub")).expect("sub");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    let mut req = act("pwd", &[]);
    req.cwd = "sub".into();
    client.run(&req).expect("started");
    assert_eq!(client.wait().expect("exited").end, End::Exited(0));
    let sub = shares.inputs.join("sub").canonicalize().expect("canonical");
    let want = format!("{}\n", sub.to_str().expect("utf-8"));
    assert_eq!(
        String::from_utf8(shares.out("stdout")).expect("utf-8"),
        want
    );
}

/// Catches an inherited environment: the agent's own variables leaking into the
/// command, or the request's left out.
#[test]
fn the_command_gets_exactly_its_environment() {
    let shares = Shares::new("env");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    client
        .run(&program(&["/usr/bin/env"], &[("ONLY", "1")]))
        .expect("started");
    assert_eq!(client.wait().expect("exited").end, End::Exited(0));
    let out = String::from_utf8(shares.out("stdout")).expect("utf-8");
    let vars: Vec<&str> = out.lines().collect();
    assert!(vars.contains(&"ONLY=1"), "{vars:?}");
    assert!(vars.contains(&"PATH=/usr/bin:/bin"), "{vars:?}");
    // The test process has these; the command must not.
    for leaked in ["HOME=", "CARGO_", "RUST_"] {
        assert!(
            !vars.iter().any(|v| v.starts_with(leaked)),
            "{leaked} leaked: {vars:?}"
        );
    }
}

/// Catches an output report that follows a symlink (at the end or on the way), gives
/// a file the wrong size, or loses request order.
#[test]
fn requested_outputs_are_reported_without_following_symlinks() {
    let shares = Shares::new("outputs");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    let out = &shares.outputs;
    std::fs::write(out.join("f"), b"hello").expect("f");
    std::fs::create_dir(out.join("d")).expect("d");
    std::fs::write(out.join("d/x"), b"x").expect("d/x");
    std::os::unix::fs::symlink("/", out.join("l")).expect("l");
    std::os::unix::fs::symlink("d", out.join("via")).expect("via");
    let made = std::process::Command::new("mkfifo")
        .arg(out.join("p"))
        .status()
        .expect("mkfifo ran");
    assert!(made.success(), "mkfifo: {made}");
    let mut req = act("exit 0", &[]);
    req.outputs = [
        "f",
        "d",
        "d/x",
        "l",
        "via/x",
        "p",
        "absent",
        "f/under-a-file",
    ]
    .map(String::from)
    .to_vec();
    client.run(&req).expect("started");
    let exit = client.wait().expect("exited");
    assert_eq!(exit.end, End::Exited(0));
    let got: Vec<(&str, OutputKind, u64)> = exit
        .outputs
        .iter()
        .map(|o| (o.path.as_str(), o.kind, o.size))
        .collect();
    assert_eq!(
        got,
        vec![
            ("f", OutputKind::File, 5),
            ("d", OutputKind::Directory, 0),
            ("d/x", OutputKind::File, 1),
            ("l", OutputKind::Symlink, 0),
            ("via/x", OutputKind::Missing, 0),
            ("p", OutputKind::Other, 0),
            ("absent", OutputKind::Missing, 0),
            ("f/under-a-file", OutputKind::Missing, 0),
        ]
    );
}

/// Catches usage fields left zero or taken from the wrong clock.
#[test]
fn usage_is_measured() {
    let shares = Shares::new("usage");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    client.run(&act("sleep 0.2", &[])).expect("started");
    let exit = client.wait().expect("exited");
    assert!(exit.usage.wall_micros >= 200_000, "{:?}", exit.usage);
    assert!(exit.usage.wall_micros < 10_000_000, "{:?}", exit.usage);
    // At least a page, and not a KiB count read as bytes on Linux (any running
    // program is more than 64 KiB resident).
    assert!(exit.usage.peak_rss_bytes > 64 * 1024, "{:?}", exit.usage);
}
