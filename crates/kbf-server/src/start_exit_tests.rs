//! The class of each start failure ([`StartFailure`]).

use std::error::Error;
use std::fmt;
use std::io;
use std::num::NonZeroU64;
use std::path::PathBuf;

use kbf_front::CacheError;
use kbf_meta::{NodeWriter, Retention, StaleWriter};
use kbf_objstore::FsStoreError;

use super::{NOT_READY, REFUSED, StartFailure};
use crate::RaftLogError;
use crate::RaftMetaLog;
use crate::write_tier::WriteTierError;

fn class(e: impl Error + 'static) -> StartFailure {
    let boxed: Box<dyn Error> = Box::new(e);
    StartFailure::of(boxed.as_ref())
}

fn stale() -> StaleWriter {
    let one = NonZeroU64::MIN;
    StaleWriter {
        writer: NodeWriter::new(one, one),
        current: one,
    }
}

/// An error that names `source` and nothing else, as a wrapper around a start's failure.
#[derive(Debug)]
struct Wrapped(Box<dyn Error + 'static>);

impl fmt::Display for Wrapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "wrapped")
    }
}

impl Error for Wrapped {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// Catches a start that is refused for a reason a restart cannot clear but exits as
/// not ready (it would be restarted for ever), or a refusal that moves with its text
/// (mutant: [`StartFailure::of`] answering not ready for every error): flags, files
/// given wrongly, a directory that is not a log, a permission.
#[test]
fn what_a_restart_cannot_clear_is_refused() {
    let refused = [
        class(io::Error::other("--cold-store is required")),
        class(RaftLogError::NotEmpty),
        class(RaftLogError::Record),
        class(RaftLogError::Io(io::Error::from(
            io::ErrorKind::PermissionDenied,
        ))),
        class(RaftLogError::Io(io::Error::from(io::ErrorKind::NotFound))),
        class(FsStoreError::Io {
            op: "create",
            path: PathBuf::from("/proc/x"),
            source: io::Error::from(io::ErrorKind::NotFound),
        }),
        class(WriteTierError::Dir(io::Error::from(
            io::ErrorKind::PermissionDenied,
        ))),
    ];
    for got in refused {
        assert_eq!(got, StartFailure::Refused);
    }
    let text: Box<dyn Error> = "the frontend registered this node as member 0".into();
    assert_eq!(StartFailure::of(text.as_ref()), StartFailure::Refused);
}

/// Catches a stale boot (the frontend's state store restored from an older copy) that
/// exits 2, which `RestartPreventExitStatus=2` leaves down for good (mutant: the
/// `StaleWriter` arm removed), whether the cache's error is matched itself or found
/// under a wrapper by the source chain.
#[test]
fn a_stale_writer_is_not_ready() {
    assert_eq!(
        class(CacheError::StaleWriter(stale())),
        StartFailure::NotReady
    );
    assert_eq!(class(stale()), StartFailure::NotReady);
    let under = Wrapped(Box::new(CacheError::StaleWriter(stale())));
    assert_eq!(class(under), StartFailure::NotReady);
}

/// Catches a directory held by the process that is stopping that exits 2 (mutants: the
/// `Locked` arms removed), through the real second open of one directory, and an I/O
/// failure a later try can clear (a busy, read-only or full device) that exits 2.
#[tokio::test]
async fn a_held_directory_and_a_device_not_ready_are_not_ready() {
    let dir = std::env::temp_dir().join(format!("kbf-start-exit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _first = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
        .await
        .expect("open the log");
    let second = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/").await;
    let Err(second) = second else {
        panic!("the directory is held; a second open must fail");
    };
    assert!(matches!(second, RaftLogError::Locked { .. }), "{second}");
    assert_eq!(class(second), StartFailure::NotReady);
    let _ = std::fs::remove_dir_all(dir);
    let locked = FsStoreError::Locked {
        path: PathBuf::from("/x/LOCK"),
        source: std::fs::TryLockError::WouldBlock,
    };
    assert_eq!(class(locked), StartFailure::NotReady);
    for kind in [
        io::ErrorKind::ResourceBusy,
        io::ErrorKind::ReadOnlyFilesystem,
        io::ErrorKind::StorageFull,
    ] {
        assert_eq!(
            class(RaftLogError::Io(io::Error::from(kind))),
            StartFailure::NotReady,
            "{kind}"
        );
        assert_eq!(
            class(WriteTierError::Dir(io::Error::from(kind))),
            StartFailure::NotReady,
            "{kind}"
        );
    }
}

/// Catches a class that exits with another status than the units name: 2 is what
/// `RestartPreventExitStatus=2` never restarts, 75 is what it does (mutant: either
/// constant changed, or both 2).
#[test]
fn each_class_has_its_status() {
    assert_eq!(StartFailure::Refused.status(), 2);
    assert_eq!(StartFailure::NotReady.status(), 75);
    assert_eq!((REFUSED, NOT_READY), (2, 75));
    assert_ne!(
        StartFailure::Refused.status(),
        StartFailure::NotReady.status()
    );
}

/// Catches a restore that restarts every node silently (with exit 75 and a restart delay,
/// a node behind on boots restarts until they catch up, which must not read as an ordinary
/// crash loop): a `StaleWriter`, bare, inside the cache's variant or wrapped, carries the
/// line naming the restored state store and the wait; any other failure carries none.
/// Mutant: `hint` always `None`.
#[test]
fn a_stale_writer_says_the_state_store_is_older() {
    let hint = |e: Box<dyn Error>| super::hint(e.as_ref());
    assert_eq!(hint(Box::new(stale())), Some(super::STALE_WRITER_HINT));
    assert_eq!(
        hint(Box::new(CacheError::StaleWriter(stale()))),
        Some(super::STALE_WRITER_HINT)
    );
    assert_eq!(
        hint(Box::new(Wrapped(Box::new(stale())))),
        Some(super::STALE_WRITER_HINT)
    );
    assert!(super::STALE_WRITER_HINT.contains("restored from a backup"));
    assert!(super::STALE_WRITER_HINT.contains("do not wipe --meta-dir"));
    assert_eq!(
        hint(Box::new(io::Error::from(io::ErrorKind::ResourceBusy))),
        None
    );
}
