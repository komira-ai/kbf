//! [`ForkedChild`] and [`spawn_retrying`]: a child held between `fork` and `exec` keeps
//! a copy of every descriptor open when it forked (what the tests of other crates that
//! release a lock, or run a program they have just copied, rely on), and a start that
//! finds its program busy is retried, a bounded number of times.

use std::fs::{File, TryLockError};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kbf_testkit::forked::ForkedChild;
use kbf_testkit::spawn::retry_busy;

/// A fresh directory for one test, under Cargo's per-target temporary directory.
fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("old dir");
    }
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// A new lock file with its lock held by the returned file.
fn locked(path: &Path) -> File {
    let file = File::create(path).expect("lock file");
    file.try_lock().expect("the lock is taken");
    file
}

/// Whether another open of `path` is refused the lock now.
fn refused(path: &Path) -> bool {
    let other = File::open(path).expect("open");
    matches!(other.try_lock(), Err(TryLockError::WouldBlock))
}

/// Catches: a child that is not held (it execs at once, so a copy of the descriptor is
/// gone before the test looks), or one forked before the file was open (it holds no
/// copy). The premise of the tests that release a lock with a child alive: the lock of
/// a file that is closed, not unlocked, stays held while the child lives. (It is free
/// again once the child has gone, unless another test of this binary has forked a
/// child of its own meanwhile, so that is not asserted.)
#[test]
fn a_lock_released_by_closing_stays_held_while_a_forked_child_has_a_copy() {
    let path = dir("forked-close").join("lock");
    let file = locked(&path);
    let child = ForkedChild::before_exec();
    drop(file);
    assert!(refused(&path), "the lock went with the close");
    child.finish();
}

/// Catches: an unlock that a copy of the descriptor outlives. The fix the tests of the
/// other crates pin: a lock unlocked before the close is free whatever copies exist.
#[test]
fn an_unlocked_lock_is_free_while_a_forked_child_has_a_copy() {
    let path = dir("forked-unlock").join("lock");
    let file = locked(&path);
    let child = ForkedChild::before_exec();
    file.unlock().expect("unlock");
    drop(file);
    assert!(!refused(&path), "the lock outlived the unlock");
    child.finish();
}

/// Catches: a held child that is never let go when its test fails before `finish` (it
/// would keep its copy of the lock for ever). Free within seconds, since a child that
/// another test of this binary holds meanwhile goes in its own time.
#[test]
fn a_dropped_forked_child_is_let_go() {
    let path = dir("forked-drop").join("lock");
    let file = locked(&path);
    drop(ForkedChild::before_exec());
    drop(file);
    let until = Instant::now() + Duration::from_secs(10);
    while refused(&path) {
        assert!(Instant::now() < until, "the child outlived its drop");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Catches: a retry that gives up early or goes on for ever. A start that is busy is
/// repeated `attempts` times in all, then its error is returned as it came; one that
/// succeeds is not repeated; a failure that is not "busy" is not repeated; at least one
/// try is made.
#[test]
fn a_busy_start_is_tried_the_number_of_times_given() {
    let busy = |_| Some(ErrorKind::ExecutableFileBusy);
    let (end, tries) = tried(5, busy);
    assert_eq!(
        end.expect_err("still busy").kind(),
        ErrorKind::ExecutableFileBusy
    );
    assert_eq!(tries, 5);

    let (end, tries) = tried(5, |n| (n < 3).then_some(ErrorKind::ExecutableFileBusy));
    end.expect("started on the third try");
    assert_eq!(tries, 3);

    let (end, tries) = tried(5, |_| Some(ErrorKind::NotFound));
    assert_eq!(end.expect_err("not busy").kind(), ErrorKind::NotFound);
    assert_eq!(
        tries, 1,
        "a failure that is not a busy program was repeated"
    );

    let (end, tries) = tried(0, busy);
    assert!(end.is_err());
    assert_eq!(tries, 1, "no try was made");
}

/// Runs `retry_busy` for `attempts` tries, try number `n` (from 1) failing with the kind
/// `fails(n)` gives, or succeeding when it gives `None`. Returns the outcome and the
/// number of tries made.
fn tried(attempts: u32, fails: impl Fn(u32) -> Option<ErrorKind>) -> (io::Result<()>, u32) {
    let mut tries = 0;
    let end = retry_busy(attempts, Duration::ZERO, &mut || {
        tries += 1;
        fails(tries).map_or(Ok(()), |kind| Err(io::Error::from(kind)))
    });
    (end, tries)
}

/// The busy program of the real race, which `ETXTBSY` is on Linux: a copy of a program
/// written while another thread forks is busy until that child execs.
#[cfg(target_os = "linux")]
mod busy {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::process::Command;

    use kbf_testkit::spawn::spawn_retrying;

    use super::*;

    const ACT: &str = env!("CARGO_BIN_EXE_kbf-act");

    /// Copies `kbf-act` to `path` through a descriptor opened for writing, which it
    /// returns, still open.
    fn copy_writing(path: &Path) -> File {
        let mut writer = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(path)
            .expect("the copy");
        io::copy(&mut File::open(ACT).expect("kbf-act"), &mut writer).expect("written");
        writer
    }

    /// Catches: a start of a copied program that fails when a process forked by
    /// another thread holds the copy's writer: plain `spawn` is refused with
    /// `ExecutableFileBusy` (the premise, so this cannot pass for another reason), and
    /// `spawn_retrying` starts the program once the child has gone.
    #[test]
    fn a_copy_written_while_a_child_forked_is_started_once_the_child_has_gone() {
        let copy = dir("forked-busy").join("act");
        let writer = copy_writing(&copy);
        let child = ForkedChild::before_exec();
        drop(writer);
        let mut command = Command::new(&copy);
        command.args(["exit", "0"]);
        let busy = command.spawn().expect_err("the copy is busy");
        assert_eq!(busy.kind(), ErrorKind::ExecutableFileBusy);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            child.finish();
        });
        let mut started = spawn_retrying(&mut command).expect("started after the child went");
        assert!(started.wait().expect("wait").success());
        release.join().expect("the child was let go");
    }

    /// Catches: a retry that hides a program that is not there (not "busy": its error
    /// is returned as it came).
    #[test]
    fn a_start_that_is_not_busy_fails_as_it_is() {
        let mut missing = Command::new(dir("forked-missing").join("nothing"));
        let error = spawn_retrying(&mut missing).expect_err("there is no such program");
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }
}
