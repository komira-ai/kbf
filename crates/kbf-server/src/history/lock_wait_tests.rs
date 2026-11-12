//! The wait for a `file://` history directory another process holds (`lock_wait`): the
//! attempts and pauses it makes over a real lock and over scripted failures, where it
//! ends, what a pause does inside and outside a runtime, and the frontend's open over
//! the flags. The stop signal itself is sent to a `kbf frontend` process
//! (kbf-cli's `tests/frontend_history_lock.rs`): a signal sent to this test binary would
//! reach every other test in it.

use std::cell::RefCell;
use std::fs::TryLockError;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kbf_objstore::{Floor, FsStore, FsStoreError};
use kbf_state::ManualClock;
use kbf_state::fake::{Fake, Flaw};

use super::{Ended, Pause, Stop, StoppedWaiting, open_within, with_history_waiting};
use crate::StateStore;
use crate::farm::EpochPolicy;
use crate::history::{HistoryError, HistoryFlags};
use crate::stores::StoreUri;
use crate::test_dir::scratch_dir;
use crate::test_log::capture_at;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn open(dir: &Path) -> Result<FsStore, FsStoreError> {
    FsStore::open(dir, Floor::Bytes(1))
}

/// The error a held lock gives, as [`FsStore::open`] makes it.
fn held(path: &Path) -> FsStoreError {
    FsStoreError::Locked {
        path: path.to_path_buf(),
        source: TryLockError::WouldBlock,
    }
}

fn state() -> StateStore {
    StateStore {
        store: Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        )),
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }
}

fn flags(dir: &Path) -> HistoryFlags {
    HistoryFlags {
        history_objects: Some(StoreUri::File(dir.to_path_buf())),
        history_index_root: None,
    }
}

/// A directory another store holds is tried again after each pause, and opened by the
/// first attempt after the holder lets go: here the third pause drops it, so the fourth
/// attempt opens. The pauses double from 250 ms. Catches a held lock that ends the start
/// (exit 2, the release whose second frontend never started), and a wait that does not
/// try again once the lock is free.
#[test]
fn a_held_directory_is_waited_for_and_opened_once_it_is_free() {
    let dir = scratch_dir("history-lock-held");
    let mut holder = Some(open(&dir).expect("the first store opens"));
    let pauses = RefCell::new(Vec::new());
    let log = capture_at(tracing::Level::INFO);
    let opened = open_within(&mut || open(&dir), &mut |wait| {
        pauses.borrow_mut().push(wait);
        if pauses.borrow().len() == 3 {
            holder = None;
        }
        Pause::Again
    });
    assert!(opened.is_ok(), "{:?}", opened.err());
    assert_eq!(*pauses.borrow(), [ms(250), ms(500), ms(1000)]);
    assert!(holder.is_none());
    let text = log.text();
    let lock = dir.join("LOCK");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 5, "{text}");
    assert!(
        lines[0].contains("the history directory is locked by another process"),
        "{text}"
    );
    assert!(
        lines[0].ends_with(&format!("path={}", lock.display())),
        "{text}"
    );
    for (n, line) in lines[1..4].iter().enumerate() {
        assert!(
            line.contains("the history directory is still locked, waiting to try again"),
            "{text}"
        );
        assert!(line.contains(&format!("attempt={} ", n + 1)), "{text}");
        assert!(line.contains("reason=\"locked\""), "{text}");
    }
    assert!(
        lines[3].contains("waited_secs=0.75 retry_in_secs=1.0"),
        "{text}"
    );
    assert!(
        lines[4].ends_with("the history directory is free, opened attempt=4 waited_secs=1.75"),
        "{text}"
    );
}

/// A directory that is free opens on the first attempt with no pause and nothing logged
/// about a lock. Catches a wait that always pauses once, which would slow every start.
#[test]
fn a_free_directory_opens_at_once() {
    let dir = scratch_dir("history-lock-free");
    let log = capture_at(tracing::Level::INFO);
    let mut pauses = 0_u32;
    let opened = open_within(&mut || open(&dir), &mut |_| {
        pauses += 1;
        Pause::Again
    });
    assert!(opened.is_ok());
    assert_eq!(pauses, 0);
    assert!(!log.text().contains("history directory"), "{}", log.text());
}

/// The pauses double up to 5 s and stay there, with no deadline: eleven held attempts
/// ask for 250 ms, 500 ms, 1 s, 2 s, 4 s and then 5 s each, and the twelfth opens.
/// Catches a wait that gives up after some time (a drain has no deadline), and one whose
/// pauses grow without a cap.
#[test]
fn the_pauses_double_up_to_five_seconds_and_never_give_up() {
    let dir = scratch_dir("history-lock-pauses");
    let lock = dir.join("LOCK");
    let mut attempts = 0_u32;
    let mut pauses = Vec::new();
    let opened = open_within(
        &mut || {
            attempts += 1;
            if attempts <= 11 {
                Err(held(&lock))
            } else {
                open(&dir)
            }
        },
        &mut |wait| {
            pauses.push(wait);
            Pause::Again
        },
    );
    assert!(opened.is_ok());
    assert_eq!(attempts, 12);
    let five = Duration::from_secs(5);
    assert_eq!(
        pauses,
        [
            ms(250),
            ms(500),
            ms(1000),
            ms(2000),
            ms(4000),
            five,
            five,
            five,
            five,
            five,
            five
        ]
    );
}

/// A pause that says stop ends the wait there, with what it waited and the lock it waited
/// for, and the directory is not opened: the holder still has it. The message is the
/// whole line the frontend prints. Catches a stop that is only acted on once the lock is
/// free (a frontend that a stop never ends while another holds its directory).
#[test]
fn a_stop_during_the_wait_ends_it_with_the_directory_still_held() {
    let dir = scratch_dir("history-lock-stop");
    let _holder = open(&dir).expect("the first store opens");
    let mut pauses = 0_u32;
    let log = capture_at(tracing::Level::INFO);
    let ended = open_within(&mut || open(&dir), &mut |_| {
        pauses += 1;
        if pauses == 2 {
            Pause::Stop
        } else {
            Pause::Again
        }
    });
    let Err(Ended::Stopped(why, last)) = ended else {
        panic!("stopped: {:?}", ended.err());
    };
    let lock = dir.join("LOCK");
    assert_eq!(why.path, lock);
    assert_eq!(why.attempts, 2);
    assert_eq!(why.waited, ms(250));
    assert_eq!(
        why.to_string(),
        format!(
            "--history-objects: stopped by a signal while {} was still locked by another \
             process (tried 2 times over 0 s)",
            lock.display()
        )
    );
    assert!(
        matches!(last, FsStoreError::Locked { ref path, source: TryLockError::WouldBlock } if *path == lock),
        "{last:?}"
    );
    assert!(
        log.text()
            .contains("a stop signal came while waiting for the history directory, stopping"),
        "{}",
        log.text()
    );
    assert!(open(&dir).is_err(), "the holder still has the directory");
}

/// A failure other than a lock another process holds is the result at once, with no
/// pause: a directory that cannot be made, and a lock call that failed (not "would
/// block"). Catches a wait on an error that time does not clear, which would hang the
/// start where it used to exit 2 with the reason.
#[test]
fn a_failure_other_than_a_held_lock_is_not_waited_for() {
    let path = PathBuf::from("/history/LOCK");
    let failures: [fn(&Path) -> FsStoreError; 2] = [
        |path| FsStoreError::Io {
            op: "create",
            path: path.to_path_buf(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        },
        |path| FsStoreError::Locked {
            path: path.to_path_buf(),
            source: TryLockError::Error(io::Error::from(io::ErrorKind::Unsupported)),
        },
    ];
    for failure in failures {
        let mut pauses = 0_u32;
        let ended = open_within(&mut || Err(failure(&path)), &mut |_| {
            pauses += 1;
            Pause::Again
        });
        let Err(Ended::Failed(e)) = ended else {
            panic!("failed at once: {:?}", ended.err());
        };
        assert_eq!(pauses, 0, "{e}");
        assert_eq!(e.to_string(), failure(&path).to_string());
    }
}

/// Outside a Tokio runtime nothing is watched, and a pause only sleeps and says try
/// again (a zero pause too). Catches a watch that panics with no runtime (Tokio's
/// `signal` does), and a pause that stops with nothing to stop it.
#[test]
fn outside_a_runtime_a_pause_only_sleeps() {
    let mut stop = Stop::watch();
    assert!(!stop.watching());
    assert_eq!(stop.pause(ms(1)), Pause::Again);
    assert_eq!(stop.pause(Duration::ZERO), Pause::Again);
}

/// Inside a runtime the signals are watched, and a pause of more than one slice with no
/// signal sleeps through its slices and says try again. Catches a watch that is not
/// installed in a server's start, and a pause that says stop with no signal.
#[test]
fn inside_a_runtime_a_pause_with_no_signal_says_try_again() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let _inside = runtime.enter();
    let mut stop = Stop::watch();
    assert!(stop.watching());
    assert_eq!(stop.pause(super::SLICE + ms(1)), Pause::Again);
}

/// The frontend's open over the flags: with no `--history-objects` the state store is
/// handed back as it was; a free `file://` directory opens; a directory that cannot be
/// made is refused at once with the flags' own error; and a `--history-objects` with no
/// state store is refused as before. (A held one, and a stop during its wait, are the
/// process tests' named in the module docs.) Catches a
/// frontend that no longer opens its history, and an error that loses its type (the
/// exit code is chosen by it).
#[test]
fn the_frontend_opens_its_history_over_the_flags() {
    let none = with_history_waiting(Some(state()), &HistoryFlags::default()).expect("no history");
    assert!(none.expect("the state store").history.is_none());

    let dir = scratch_dir("history-lock-flags");
    let free = with_history_waiting(Some(state()), &flags(&dir)).expect("opens");
    assert!(free.expect("the state store").history.is_some());
    // The first frontend's history is dropped with it: its directory is free again.

    let file = dir.join("a-file");
    std::fs::write(&file, b"not a directory").expect("write");
    let refused = with_history_waiting(Some(state()), &flags(&file.join("history")))
        .err()
        .expect("a file is in the way");
    let said = refused.to_string();
    assert!(said.starts_with("--history-objects: create "), "{said}");
    assert!(
        matches!(
            refused.downcast_ref::<HistoryError>(),
            Some(HistoryError::Open(FsStoreError::Io { .. }))
        ),
        "{refused:?}"
    );
    assert!(refused.downcast_ref::<StoppedWaiting>().is_none());

    let needs = with_history_waiting(None, &flags(&dir))
        .err()
        .expect("no state store");
    assert!(
        matches!(
            needs.downcast_ref::<HistoryError>(),
            Some(HistoryError::NeedsStateStore)
        ),
        "{needs:?}"
    );
}
