//! One open log per raft directory (kbf#690): `RaftMetaLog::open_dir` holds an
//! exclusive lock on a file in the directory for as long as the log is open, so a
//! second server started on the same `--meta-dir` is refused instead of writing the
//! same log, and a start after the first log is dropped opens it.

use std::path::PathBuf;

use kbf_meta::Retention;
use kbf_server::RaftMetaLog;

/// An empty directory of its own for `name`.
fn dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-server-raft-dir-lock")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
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
    let _ = std::fs::remove_dir_all(&dir);
}
