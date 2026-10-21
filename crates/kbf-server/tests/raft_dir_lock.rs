//! One open log per raft directory (kbf#690): `RaftMetaLog::open_dir` holds an
//! exclusive lock on a file in the directory for as long as the log is open, so a
//! second server started on the same `--meta-dir` is refused instead of writing the
//! same log, and a start after the first log is dropped opens it.

use std::io::{self, PipeWriter, Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread::JoinHandle;

use kbf_meta::Retention;
use kbf_server::RaftMetaLog;

/// A directory of its own for `name`, removed when it drops; the raft directory is
/// `d` in it, not yet made.
struct Dir {
    path: PathBuf,
    _dir: kbf_testkit::dir::TestDir,
}

impl std::ops::Deref for Dir {
    type Target = std::path::Path;

    fn deref(&self) -> &std::path::Path {
        &self.path
    }
}

impl AsRef<std::path::Path> for Dir {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

fn dir(name: &str) -> Dir {
    let dir = kbf_testkit::dir::TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("kbf-server-raft-dir-lock-{name}"),
    );
    Dir {
        path: dir.join("d"),
        _dir: dir,
    }
}

/// Catches: a raft directory two open logs write at once (no lock, or a lock taken
/// after the store is opened), a refusal that does not name the directory, and a lock
/// the drop does not release (the restart is refused).
#[tokio::test]
async fn a_second_open_of_one_raft_dir_is_refused_until_the_first_is_dropped() {
    let dir = dir("second-open");
    let first = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
        .await
        .expect("the first open");
    let second = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/").await;
    let refused = second.expect_err("a second open of a raft dir in use was not refused");
    let text = refused.to_string();
    assert!(
        text.contains(&dir.display().to_string()),
        "the refusal does not name the directory: {text}"
    );
    drop(first);
    let again = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/").await;
    let again = again.expect("an open after the first log was dropped");
    assert_eq!(again.bootstraps(), 1);
    drop(again);
}

/// A child process caught between `fork` and `exec`. It holds a copy of every
/// descriptor its parent had open when it forked, the raft directory's lock file
/// among them, until it is let go and execs (which closes the descriptors opened
/// close-on-exec). Another test thread spawning a process in the same test binary is
/// this child for the few milliseconds that takes; here it stays until [`Self::finish`].
struct ForkedChild {
    /// Written to let the child go; `None` once it has been.
    go: Option<PipeWriter>,
    /// The thread inside `Command::spawn`, which returns only when the child has exec'd.
    spawner: Option<JoinHandle<io::Result<Child>>>,
}

impl ForkedChild {
    /// Forks a child that waits for [`Self::finish`] before it execs, and returns once
    /// the child exists.
    fn before_exec() -> Self {
        let (go_rx, go_tx) = io::pipe().expect("a pipe");
        let (forked_rx, forked_tx) = io::pipe().expect("a pipe");
        let mut command = Command::new("true");
        // SAFETY: the closure runs in the child between fork and exec. It does one
        // `write` and one `read` on pipes it owns and allocates nothing.
        unsafe {
            command.pre_exec(move || {
                (&forked_tx).write_all(&[1])?;
                (&go_rx).read_exact(&mut [0])
            });
        }
        let spawner = std::thread::spawn(move || command.spawn());
        // The child writes once it exists. If `spawn` fails instead, the spawner thread
        // ends, its `command` drops the last writer, and this read errors, not waits.
        (&forked_rx)
            .read_exact(&mut [0])
            .expect("the child was forked");
        Self {
            go: Some(go_tx),
            spawner: Some(spawner),
        }
    }

    fn let_go(&mut self) {
        if let Some(go) = self.go.take() {
            (&go).write_all(&[1]).expect("let the child go");
        }
    }

    /// Lets the child exec and waits for it to end.
    fn finish(mut self) {
        self.let_go();
        let spawner = self.spawner.take().expect("the spawner thread");
        let mut child = spawner.join().expect("spawner").expect("the child execs");
        assert!(child.wait().expect("wait").success());
    }
}

impl Drop for ForkedChild {
    /// A test that fails before [`Self::finish`] still lets the child go.
    fn drop(&mut self) {
        if let Some(go) = self.go.take() {
            let _ = (&go).write_all(&[1]);
        }
    }
}

/// Catches: a lock the drop releases only by closing its descriptor, which a process
/// forked while the log was open keeps held until it execs. A process that spawns
/// children from one thread while another reopens a directory (the tests of one test
/// binary run on threads of one process) is then refused its own reopen with
/// `WouldBlock` for as long as a child is between fork and exec (seen on kbf#1018's CI:
/// `Locked` from the reopen in a storage node test, at the instant another test ran
/// `git`). The child here is held before exec until the reopen has been tried, so the
/// interleaving is the failing one every time. The release must be an explicit unlock,
/// which no copy of the descriptor outlives.
#[tokio::test]
async fn a_child_forked_while_the_log_was_open_does_not_keep_the_dir_locked_after_the_drop() {
    let dir = dir("forked-child");
    let first = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
        .await
        .expect("the first open");
    let child = ForkedChild::before_exec();
    drop(first);
    let again = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/").await;
    let again = again.expect("an open after the drop, with the forked child still alive");
    assert_eq!(again.bootstraps(), 1);
    drop(again);
    child.finish();
}
