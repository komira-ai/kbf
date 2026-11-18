//! Two `kbf frontend` processes on one `file://` history directory, as a start-first
//! update makes them: the service manager starts the new frontend while the old one is
//! still running, and stops the old one only once the new one is healthy. The history
//! directory is opened shared, so the new frontend starts and serves beside the old one,
//! which is never signalled before that; and it starts beside a previous release's
//! frontend, which holds the directory's `LOCK`. Each frontend has a SQLite state store
//! of its own, so only the history directory is shared.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use kbf_objstore::{Floor, FsStore};
use kbf_testkit::dir::TestDir;
use rustix::process::{Pid, Signal, kill_process};

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

/// How long a test waits for a line or an exit before it fails rather than hang. Not a
/// bound on the frontend: every wait ends on an event of its own.
const HANG: Duration = Duration::from_secs(60);

/// A `kbf frontend` child, its stdout's first line and its stderr's lines as they come;
/// killed and reaped when dropped.
struct Front {
    child: Child,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
    /// Every stderr line [`Front::rest_of_stderr`] has read.
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

    /// The start line, once printed. A frontend that never starts fails the test here
    /// instead of hanging it.
    fn start_line(&self) -> String {
        match self.stdout.recv_timeout(HANG) {
            Ok(line) => line,
            Err(e) => panic!("no start line ({e})"),
        }
    }

    /// The operator API's address, from the start line.
    fn api(line: &str) -> String {
        line.split_whitespace()
            .find_map(|word| word.strip_prefix("api="))
            .unwrap_or_else(|| panic!("no api= in the start line {line:?}"))
            .to_owned()
    }

    /// Every stderr line from here to the exit.
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

    /// Whether the process is still running.
    fn running(&mut self) -> bool {
        self.child.try_wait().expect("try_wait").is_none()
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

/// The status code of one `GET` on the operator API at `api`.
fn status_of(api: &str, path: &str) -> u16 {
    let mut stream = std::net::TcpStream::connect(api).expect("connect to the API");
    let bound = Some(HANG);
    stream.set_read_timeout(bound).expect("a read timeout");
    stream.set_write_timeout(bound).expect("a write timeout");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n"
    )
    .expect("send");
    let mut text = String::new();
    stream.read_to_string(&mut text).expect("read");
    let (head, _) = text.split_once("\r\n\r\n").expect("a head and a body");
    head.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status code")
}

/// The update a start-first rollout makes: the first frontend runs and is never
/// signalled; the second starts on the same history directory, prints its start line and
/// answers `/healthz` 200 beside it; only then is the first stopped (exit 0), and the
/// second goes on answering until it is stopped in turn. Catches the second frontend
/// waiting for a directory the first holds, which start-first never releases (Swarm stops
/// the old task only once the new one is healthy), and one that exits 2 on it.
#[test]
fn a_second_frontend_starts_and_serves_while_the_first_still_runs_on_the_history_directory() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-history-overlap");
    let history = scratch.join("history");
    let mut first = Front::spawn(&scratch.join("first"), &history);
    let first_line = first.start_line();
    assert!(first_line.starts_with("kbf-frontend "), "{first_line}");
    let first_api = Front::api(&first_line);

    let mut second = Front::spawn(&scratch.join("second"), &history);
    let second_line = second.start_line();
    assert!(second_line.starts_with("kbf-frontend "), "{second_line}");
    let second_api = Front::api(&second_line);
    assert_eq!(status_of(&second_api, "/healthz"), 200);
    assert_eq!(status_of(&first_api, "/healthz"), 200);
    assert!(first.running(), "the first stopped before it was told to");

    first.signal(Signal::TERM);
    assert!(first.exit().success());

    assert_eq!(status_of(&second_api, "/healthz"), 200);
    assert!(second.running());
    second.signal(Signal::TERM);
    assert!(second.exit().success());
}

/// A frontend started while another process holds the history directory's `LOCK` (what a
/// previous release's frontend holds for as long as it runs, and a new frontend is
/// started beside it in the very update that ships this) starts and serves, and its stop
/// leaves that holder's lock alone. The holder is a store opened exclusively in this
/// process, as the previous release opens the directory. Catches a start that takes or
/// waits for `LOCK`, and one that exits 2 with "is another server using this directory?".
#[test]
fn a_frontend_starts_on_a_history_directory_whose_lock_a_previous_release_holds() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-history-old-lock");
    let history = scratch.join("history");
    let previous = FsStore::open(&history, Floor::KeepFree).expect("the previous release's open");

    let mut front = Front::spawn(&scratch.join("new"), &history);
    let line = front.start_line();
    assert!(line.starts_with("kbf-frontend "), "{line}");
    assert_eq!(status_of(&Front::api(&line), "/healthz"), 200);
    front.signal(Signal::TERM);
    assert!(front.exit().success());

    assert!(
        matches!(
            FsStore::open(&history, Floor::KeepFree),
            Err(kbf_objstore::FsStoreError::Locked { .. })
        ),
        "the previous release's lock is still held"
    );
    drop(previous);
}

/// A history directory that cannot be made (a file is in the way) exits 2 at once with the
/// reason and no start line. Catches a start that retries a failure that time does not
/// clear, which would leave it hanging where it should say what is wrong.
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
    assert!(front.stdout.recv_timeout(HANG).is_err(), "no start line");
}
