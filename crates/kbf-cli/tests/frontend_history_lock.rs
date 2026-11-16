//! Two `kbf frontend` processes on one `file://` history directory, as a release update
//! makes them when the service manager starts the new frontend before the old one has
//! exited (Docker Swarm does, one minute after it told the old task to stop, whatever
//! the update's order): the second waits for the directory while the first holds it,
//! starts once the first has stopped, and a stop signal ends its wait with exit 0.
//! Each frontend has a SQLite state store of its own, so only the history directory is
//! shared.

use std::io::{BufRead as _, BufReader};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use kbf_testkit::dir::TestDir;
use rustix::process::{Pid, Signal, kill_process};

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

/// How long a test waits for a line or an exit before it fails rather than hang. Not a
/// bound on the frontend: every wait ends on an event of its own.
const HANG: Duration = Duration::from_secs(60);

/// What the second frontend logs at each attempt on the held directory.
const WAITING: &str = "the history directory is still locked, waiting to try again";

/// A `kbf frontend` child, its stdout's first line and its stderr's lines as they come;
/// killed and reaped when dropped.
struct Front {
    child: Child,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
    /// Every stderr line [`Front::wait_for`] has read.
    seen: Vec<String>,
}

impl Front {
    /// `kbf frontend` on loopback ports with no storage node (nothing here is executed),
    /// its settings file (empty, so no host file is read) and state store in `dir` and its
    /// history in `history`.
    fn spawn(dir: &Path, history: &Path) -> Self {
        std::fs::create_dir_all(dir).expect("the frontend's directory");
        let config = kbf_testkit::config::flag(dir);
        let state = format!("--state-store=sqlite://{}", dir.join("state.db").display());
        let history = format!("--history-objects=file://{}", history.display());
        let mut child = Command::new(BIN)
            .args([
                "frontend",
                &config,
                "--listen=127.0.0.1:0",
                "--worker-listen=127.0.0.1:0",
                "--api-listen=127.0.0.1:0",
                "--drain-lb-wait=0",
                "--storage=http://127.0.0.1:1",
                &state,
                &history,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kbf frontend");
        let stdout = lines_of(child.stdout.take().expect("stdout"));
        let stderr = lines_of(child.stderr.take().expect("stderr"));
        Self {
            child,
            stdout,
            stderr,
            seen: Vec::new(),
        }
    }

    /// The start line, once printed.
    fn start_line(&self) -> String {
        match self.stdout.recv_timeout(HANG) {
            Ok(line) => line,
            Err(e) => panic!("no start line ({e})"),
        }
    }

    /// Reads stderr until a line holds `text`.
    fn wait_for(&mut self, text: &str) {
        loop {
            match self.stderr.recv_timeout(HANG) {
                Ok(line) => {
                    let found = line.contains(text);
                    self.seen.push(line);
                    if found {
                        return;
                    }
                }
                Err(e) => panic!("no line holding {text:?} ({e}): {:#?}", self.seen),
            }
        }
    }

    /// Every stderr line from here to the exit, after the ones [`Front::wait_for`] read.
    fn rest_of_stderr(&mut self) -> String {
        while let Ok(line) = self.stderr.recv_timeout(HANG) {
            self.seen.push(line);
        }
        self.seen.join("\n")
    }

    fn signal(&self, signal: Signal) {
        let pid = Pid::from_raw(self.child.id().cast_signed()).expect("pid");
        kill_process(pid, signal).expect("signal the frontend");
    }

    /// The exit status, once the process has exited.
    fn exit(&mut self) -> ExitStatus {
        for _ in 0..HANG.as_millis() / 50 {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the frontend did not exit within {HANG:?}");
    }
}

impl Drop for Front {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The lines `out` yields, on a channel, read on a thread of their own; the channel
/// closes when the pipe does.
fn lines_of(out: impl std::io::Read + Send + 'static) -> Receiver<String> {
    let (sent, received) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if sent.send(line).is_err() {
                return;
            }
        }
    });
    received
}

/// The release update the deploy failed on: the first frontend serves over the history
/// directory, and a second on the same directory neither exits nor serves while the
/// first holds it; it logs the wait at each attempt. Once the first has stopped (exit 0),
/// the second opens the directory and prints its start line, and stops with exit 0 in
/// turn. Catches the second frontend exiting 2 on the lock ("is another server using
/// this directory?"), which made Swarm fail and roll back the update, and a wait that
/// never tries again.
#[test]
fn a_second_frontend_waits_for_the_history_directory_and_starts_once_the_first_stops() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-history-lock");
    let history = scratch.join("history");
    let mut first = Front::spawn(&scratch.join("first"), &history);
    assert!(first.start_line().starts_with("kbf-frontend "));

    let mut second = Front::spawn(&scratch.join("second"), &history);
    second.wait_for(WAITING);
    second.wait_for(WAITING);
    assert_eq!(
        second.stdout.try_recv().err(),
        Some(std::sync::mpsc::TryRecvError::Empty),
        "no start line while the first holds the directory"
    );
    assert!(second.child.try_wait().expect("try_wait").is_none());

    first.signal(Signal::TERM);
    assert!(first.exit().success());

    assert!(second.start_line().starts_with("kbf-frontend "));
    second.wait_for("the history directory is free, opened");
    second.signal(Signal::TERM);
    assert!(second.exit().success());
}

/// A frontend whose history directory is held stops on a SIGTERM or SIGINT sent before
/// its wait has paused once, with exit 0, without a start line, and says why on stderr;
/// the first frontend goes on serving and holding the directory. The SIGTERM is sent
/// once the start has logged where its history is, before the directory is tried at all;
/// the SIGINT once the first attempt has found it locked, before the first pause. Each
/// line is written after the frontend's handlers are installed, so the signal never meets
/// the default action. Catches a wait that a stop does not end (the wait has no deadline:
/// Swarm's stop of the waiting task, a rollback for one, would wait out the stop grace
/// period), and a stop that exits 2 as a refused start. (That the watch is made before the
/// start, so that no signal the handlers take is missed, is not something a test can force
/// from outside: a signal reaches a listener through the runtime's driver thread.)
#[test]
fn a_frontend_waiting_for_the_history_directory_stops_on_sigterm_and_sigint() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-history-lock-stop");
    let history = scratch.join("history");
    let mut first = Front::spawn(&scratch.join("first"), &history);
    assert!(first.start_line().starts_with("kbf-frontend "));
    let lock = history.join("LOCK");

    let before = [
        (Signal::TERM, "history: objects in file://"),
        (
            Signal::INT,
            "the history directory is locked by another process",
        ),
    ];
    for (n, (signal, line)) in before.into_iter().enumerate() {
        let mut second = Front::spawn(&scratch.join(format!("second-{n}")), &history);
        second.wait_for(line);
        second.signal(signal);
        let status = second.exit();
        assert_eq!(status.code(), Some(0), "{signal:?}: {status}");
        let said = second.rest_of_stderr();
        assert!(
            said.contains(&format!(
                "kbf frontend: --history-objects: stopped by a signal while {} was still \
                 locked by another process",
                lock.display()
            )),
            "{signal:?}: {said}"
        );
        assert!(
            said.contains("a stop signal came while waiting for the history directory"),
            "{signal:?}: {said}"
        );
        assert!(
            second.stdout.recv_timeout(HANG).is_err(),
            "{signal:?}: no start line"
        );
        assert!(first.child.try_wait().expect("try_wait").is_none());
    }

    first.signal(Signal::TERM);
    assert!(first.exit().success());
}

/// A history directory that cannot be made (a file is in the way) is not waited for: the
/// frontend exits 2 at once with the reason and no start line, as before the wait. Catches
/// a wait on a failure that time does not clear, which would leave the start hanging where
/// it used to say what is wrong.
#[test]
fn a_history_directory_that_cannot_be_made_exits_two_at_once() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-history-unmade");
    let file = scratch.join("a-file");
    std::fs::write(&file, b"not a directory").expect("write");
    let mut front = Front::spawn(&scratch.join("front"), &file.join("history"));
    let status = front.exit();
    assert_eq!(status.code(), Some(2), "{status}");
    let said = front.rest_of_stderr();
    assert!(
        said.contains(&format!(
            "kbf frontend: --history-objects: create {}",
            file.join("history").display()
        )),
        "{said}"
    );
    assert!(!said.contains(WAITING), "{said}");
    assert!(front.stdout.recv_timeout(HANG).is_err(), "no start line");
}
