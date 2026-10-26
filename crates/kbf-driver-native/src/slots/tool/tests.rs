//! The deadline of a tool run: stand-ins for the process and its pipe on a clock the test
//! moves, and the system's `head`, `ls` and `sleep` for the real thing.

use std::sync::mpsc;

use super::*;
use crate::pace::Manual;

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

/// A stand-in process: it exits at the `exits_at`th look (never, for `None`), and dies
/// when killed unless it is `unkillable`.
struct Stand {
    looks: u32,
    exits_at: Option<u32>,
    unkillable: bool,
    killed: u32,
}

impl Stand {
    fn new(exits_at: Option<u32>) -> Self {
        Self {
            looks: 0,
            exits_at,
            unkillable: false,
            killed: 0,
        }
    }
}

impl Waited for Stand {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        use std::os::unix::process::ExitStatusExt as _;
        self.looks += 1;
        let gone = self.exits_at.is_some_and(|at| self.looks >= at)
            || (self.killed > 0 && !self.unkillable);
        Ok(gone.then(|| std::process::ExitStatus::from_raw(0)))
    }
    fn kill(&mut self) -> std::io::Result<()> {
        self.killed += 1;
        Ok(())
    }
}

/// A receiver whose senders are held by the caller: nothing is sent and it is never
/// closed, as the pipe of an orphan that still holds the tool's output.
fn held_open() -> Held {
    mpsc::channel()
}

/// The two ends of the channel a tool's streams are sent on.
type Held = (
    mpsc::Sender<(usize, Vec<u8>)>,
    mpsc::Receiver<(usize, Vec<u8>)>,
);

const KILL: Duration = Duration::from_secs(5);

/// Catches a deadline that holds only while the tool runs: a tool that exits at once but
/// leaves a process holding the pipe is reported when the deadline has passed (the
/// time is read on the test's clock, so it is exactly the deadline, not a wait), and
/// the call does not give up before it.
#[test]
fn a_child_that_keeps_the_pipe_open_does_not_hold_the_call_past_its_deadline() {
    let pace = Manual::default();
    let (_orphan, receiver) = held_open();
    let within = Duration::from_millis(700);
    let why = finish(&pace, &mut Stand::new(Some(1)), &receiver, (within, KILL))
        .expect_err("the output never closed");
    assert!(why.contains("not closed"), "{why}");
    // Looked at every 5 ms until the 700 ms had passed, and no longer.
    assert_eq!(pace.now(), within);
    assert_eq!(pace.pauses(), 140);
}

/// Catches a tool that is not killed at the deadline: a tool that never exits is looked
/// at until the deadline, killed once, and reported as not finished.
#[test]
fn a_tool_that_does_not_exit_is_killed_at_the_deadline() {
    let pace = Manual::default();
    let (_orphan, receiver) = held_open();
    let mut tool = Stand::new(None);
    let why = finish(
        &pace,
        &mut tool,
        &receiver,
        (Duration::from_millis(300), KILL),
    )
    .expect_err("killed");
    assert!(why.starts_with("did not finish within"), "{why}");
    assert!(!why.contains("after kill"), "{why}");
    assert_eq!(tool.killed, 1);
    // 300 ms of looks, then the first look after the kill finds it gone.
    assert_eq!(pace.now(), Duration::from_millis(300));
}

/// Catches a tool that does not die from the kill being reported as gone, and a wait
/// after the kill with no bound: the call looks for the kill wait and says it did not
/// exit.
#[test]
fn a_tool_that_survives_the_kill_is_reported_after_the_kill_wait() {
    let pace = Manual::default();
    let (_orphan, receiver) = held_open();
    let mut tool = Stand::new(None);
    tool.unkillable = true;
    let why = finish(
        &pace,
        &mut tool,
        &receiver,
        (Duration::from_millis(300), KILL),
    )
    .expect_err("killed");
    assert!(why.ends_with("did not exit after kill"), "{why}");
    assert_eq!(pace.now(), Duration::from_millis(300) + KILL);
}

/// Catches both streams lost when the tool exits only after a few looks: the status and
/// what was sent come back, and no kill happens.
#[test]
fn a_tool_that_exits_before_the_deadline_gives_its_streams() {
    let pace = Manual::default();
    let (sender, receiver) = held_open();
    sender.send((1, b"err".to_vec())).unwrap();
    sender.send((0, b"out".to_vec())).unwrap();
    let mut tool = Stand::new(Some(4));
    let (status, out, err) =
        finish(&pace, &mut tool, &receiver, (Duration::from_secs(20), KILL)).expect("finished");
    assert!(status.success());
    assert_eq!((out, err), (b"out".to_vec(), b"err".to_vec()));
    assert_eq!(tool.killed, 0);
    assert_eq!(pace.pauses(), 3, "a look every 5 ms until the exit");
}

/// Catches a real tool that is not killed at the deadline: the outcome is the same on a
/// slow host, because the tool never ends by itself.
#[test]
fn real_tools_that_never_finish_are_reported() {
    let why = run_within(&mut tool("/bin/sleep", &["30"]), Duration::from_millis(100))
        .expect_err("killed");
    assert!(why.starts_with("did not finish within"), "{why}");
    assert!(!why.contains("after kill"), "{why}");
}

/// Catches a wait with no bound: a child that has not exited is given up on at the time
/// asked, which is what bounds the wait after a kill.
#[test]
fn waiting_for_a_child_stops_at_its_bound() {
    let pace = Manual::default();
    let mut child = Stand::new(None);
    assert!(poll_exit(&pace, &mut child, Duration::from_millis(150)).is_none());
    assert_eq!(pace.now(), Duration::from_millis(150));
    assert_eq!(
        child.looks, 31,
        "one look every 5 ms, and the last at 150 ms"
    );
    let mut leaving = Stand::new(Some(3));
    assert!(poll_exit(&pace, &mut leaving, Duration::from_secs(20)).is_some());
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
