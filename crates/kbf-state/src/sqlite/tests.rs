//! Unit tests for what a test cannot reach from outside: the filesystem check on a path
//! that is on a network filesystem, and the error plumbing.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::guard::{filesystem_type, lock_path, network_filesystem};
use super::{Options, SqliteError, SqliteStore};
use crate::{Clock, StoreError, SystemClock};

fn clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock::default())
}

// Catches a state file on NFS being opened: its locks and syncs are not reliable there.
#[test]
fn a_network_filesystem_is_refused_before_anything_is_created() {
    let path = Path::new("/kbf-state-test-nonexistent/state.db");
    let refused = SqliteStore::open_checked(path, clock(), Options::default(), &|_| Ok(0x6969))
        .expect_err("NFS");
    assert!(
        matches!(&refused, SqliteError::NetworkFs { fs: "NFS", .. }),
        "got {refused}"
    );
    assert!(
        refused.to_string().contains("local disk"),
        "the message says what to do: {refused}"
    );
    assert!(
        !lock_path(path).exists(),
        "the refusal comes before the lock file is created"
    );
}

// Catches the probe being asked about the wrong directory.
#[test]
fn the_probe_is_given_the_directory_of_the_file() {
    let asked: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let probe = |dir: &Path| -> io::Result<u32> {
        asked.lock().expect("lock").push(dir.to_owned());
        Err(io::Error::other("probe stopped"))
    };
    for file in ["/data/kbf/state.db", "state.db"] {
        let refused =
            SqliteStore::open_checked(Path::new(file), clock(), Options::default(), &probe)
                .expect_err("the probe fails");
        assert!(matches!(refused, SqliteError::Io(_)), "got {refused}");
        assert!(
            refused.to_string().contains("probe stopped"),
            "got {refused}"
        );
    }
    assert_eq!(
        *asked.lock().expect("lock"),
        [PathBuf::from("/data/kbf"), PathBuf::from(".")]
    );
}

// Catches a table that misses a network filesystem, or names a local one.
#[test]
fn network_filesystems_are_told_from_local_ones() {
    for (magic, name) in [
        (0x6969, "NFS"),
        (0x517B, "SMB"),
        (0xFF53_4D42, "CIFS"),
        (0xFE53_4D42, "SMB2"),
        (0x00C3_6400, "CephFS"),
        (0x0102_1997, "9p"),
    ] {
        assert_eq!(network_filesystem(magic), Some(name));
    }
    // ext4, xfs, btrfs, tmpfs, overlayfs, zfs, and "unknown".
    for magic in [
        0xEF53,
        0x5846_5342,
        0x9123_683E,
        0x0102_1994,
        0x794C_7630,
        0x2FC1_2FC1,
        0,
    ] {
        assert_eq!(
            network_filesystem(magic),
            None,
            "{magic:#x} is a local filesystem"
        );
    }
}

// Catches a probe that fails on a real directory, which would refuse every start.
#[test]
fn the_real_probe_reads_the_filesystem_of_a_directory() {
    let magic = filesystem_type(&std::env::temp_dir()).expect("statfs of the temp directory");
    assert_eq!(
        network_filesystem(magic),
        None,
        "the test host's temp directory is local"
    );
}

// Catches an error that loses its text on the way to the caller.
#[test]
fn errors_carry_what_failed() {
    let broken = rusqlite::Error::InvalidQuery;
    let message = broken.to_string();
    assert_eq!(
        SqliteError::from(rusqlite::Error::InvalidQuery).to_string(),
        message
    );
    assert_eq!(
        StoreError::from(rusqlite::Error::InvalidQuery),
        StoreError::Failed(message)
    );
    let at = |error: SqliteError| error.at(Path::new("/x/state.db")).to_string();
    assert_eq!(
        at(SqliteError::Io("denied".to_owned())),
        "/x/state.db: denied"
    );
    assert_eq!(
        at(SqliteError::Sqlite("busy".to_owned())),
        "/x/state.db: busy"
    );
    assert_eq!(
        at(SqliteError::NotAStateStore("no".to_owned())),
        "/x/state.db: no"
    );
    let unchanged = SqliteError::NetworkFs {
        path: PathBuf::from("/x/state.db"),
        fs: "NFS",
    };
    let text = unchanged.to_string();
    assert_eq!(
        at(unchanged),
        text,
        "the variants that name their path are left alone"
    );
}

// Catches a journal mode that is not WAL being accepted: a commit would not be one sync.
#[test]
fn a_connection_that_cannot_keep_a_wal_is_refused() {
    let memory = rusqlite::Connection::open_in_memory().expect("open");
    let refused = super::schema::configure(&memory).expect_err("memory databases have no WAL");
    assert!(
        matches!(refused, SqliteError::NotAStateStore(_)),
        "got {refused}"
    );
}
