//! The deadline of a tool run, with the system's `head`, `ls`, `setsid` and `sleep`.

use super::*;

fn tool(program: &str, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command.args(args);
    command
}

/// Catches output or status lost on the happy path: stdout (more than a pipe holds, so
/// the readers are not waited behind), stderr and a failing status all come back.
#[test]
fn a_tool_that_finishes_gives_its_status_and_both_streams() {
    let within = Duration::from_secs(20);
    let (status, out, err) = run_within(
        &mut tool("/usr/bin/head", &["-c", "200000", "/dev/zero"]),
        within,
    )
    .expect("ran");
    assert!(status.success());
    assert_eq!(out.len(), 200_000);
    assert!(err.is_empty());
    let (status, out, err) =
        run_within(&mut tool("/bin/ls", &["/nonexistent-kbf-dir"]), within).expect("ran");
    assert!(!status.success());
    assert!(out.is_empty());
    assert!(!err.is_empty());
}

/// Catches a deadline that holds only while the tool runs: a tool that exits at once but
/// leaves a child holding the pipe (`setsid -f` starts it detached and returns) is
/// reported within the deadline, not after the child. Linux only: no `setsid` on a Mac.
#[cfg(target_os = "linux")]
#[test]
fn a_child_that_keeps_the_pipe_open_does_not_hold_the_call_past_its_deadline() {
    let started = Instant::now();
    let result = run_within(
        &mut tool("/usr/bin/setsid", &["-f", "/bin/sleep", "20"]),
        Duration::from_millis(700),
    );
    let took = started.elapsed();
    let why = result.expect_err("the output never closed");
    assert!(why.contains("not closed"), "{why}");
    assert!(took < Duration::from_secs(10), "took {took:?}");
}

/// Catches a tool that is not killed at the deadline: a tool that never exits is
/// reported as not finished, within the deadline plus the wait for the kill.
#[test]
fn a_tool_that_does_not_exit_is_killed_at_the_deadline() {
    let started = Instant::now();
    let result = run_within(&mut tool("/bin/sleep", &["30"]), Duration::from_millis(300));
    let took = started.elapsed();
    let why = result.expect_err("killed");
    assert!(why.starts_with("did not finish within"), "{why}");
    assert!(!why.contains("after kill"), "{why}");
    assert!(took < Duration::from_secs(10), "took {took:?}");
}

/// Catches a wait with no bound: a child that has not exited is given up on at the time
/// asked, which is what bounds the wait after a kill (a child that does not die from
/// SIGKILL cannot be made here, so the bound is tried on one that is running).
#[test]
fn waiting_for_a_child_stops_at_its_bound() {
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawned");
    let started = Instant::now();
    assert!(poll_exit(&mut child, started + Duration::from_millis(150)).is_none());
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(150) && took < Duration::from_secs(5));
    child.kill().expect("killed");
    assert!(poll_exit(&mut child, Instant::now() + Duration::from_secs(20)).is_some());
}

/// Catches the report of a tool that did not die dropped: both messages say what
/// happened.
#[test]
fn the_report_says_whether_the_tool_went_when_killed() {
    let within = Duration::from_secs(1);
    assert_eq!(timeout_message(within, true), "did not finish within 1s");
    assert!(timeout_message(within, false).ends_with("did not exit after kill"));
}

/// Catches a tool that cannot be started taken as a finished one: the call fails.
#[test]
fn a_tool_that_cannot_start_is_an_error() {
    let why = run_within(
        &mut Command::new("/nonexistent/kbf-tool"),
        Duration::from_secs(5),
    )
    .expect_err("not started");
    assert!(why.contains("No such file"), "{why}");
}
