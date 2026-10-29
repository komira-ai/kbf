//! The directory backend syncs the directory it changed before a call that changes it returns.
//! The syncing is replaced by a recorder that looks at the directory at the moment it is asked
//! to sync, so the tests see what the directory held then and whether the call had returned: no
//! power is lost and no time is measured.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use kbf_testkit::dir::TestDir;

use crate::{Backend, DirBackend, Error, PartBody};

use super::{SyncDir, TAG_LEN};

/// What the directory held when it was synced.
struct Seen {
    dir: PathBuf,
    names: Vec<String>,
    /// The bytes of the watched file, if it existed.
    watched: Option<Vec<u8>>,
}

/// Records each sync with what the directory held at that moment, and fails them all if told to.
struct Recorder {
    seen: Arc<Mutex<Vec<Seen>>>,
    watch: Option<PathBuf>,
    fails: bool,
}

#[async_trait]
impl SyncDir for Recorder {
    async fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        let mut names: Vec<String> = std::fs::read_dir(dir)?
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let watched = self
            .watch
            .as_ref()
            .and_then(|path| std::fs::read(path).ok());
        let dir = dir.to_owned();
        self.seen.lock().unwrap().push(Seen {
            dir,
            names,
            watched,
        });
        if self.fails {
            return Err(std::io::Error::other("the disk refused the sync"));
        }
        Ok(())
    }
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// A backend over a new directory that syncs with a recorder, which also reads the file `watch`
/// (a name in the directory) at each sync.
fn recorded(watch: Option<&str>, fails: bool) -> (TestDir, DirBackend, Log) {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-dirsync");
    let seen = Log::default();
    let recorder = Recorder {
        seen: Arc::clone(&seen),
        watch: watch.map(|name| dir.path().join(name)),
        fails,
    };
    let backend = DirBackend::open_with(dir.path().to_owned(), Box::new(recorder)).unwrap();
    (dir, backend, seen)
}

fn body(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

fn part(text: &'static str) -> PartBody<'static> {
    stream::iter(vec![Ok(body(text))]).boxed()
}

fn count(seen: &Log) -> usize {
    seen.lock().unwrap().len()
}

/// Asserts that a create-only put syncs the directory once, after the name is linked and the
/// temporary file is gone, and that a put refused because the name is taken syncs nothing.
/// It would catch a put that acks with the directory unsynced, and one that syncs the directory
/// before it links (the sync would then see no name).
#[tokio::test]
async fn a_put_syncs_the_directory_after_the_name_is_linked() {
    let (dir, backend, seen) = recorded(None, false);
    backend.put("k", body("one")).await.unwrap();
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].dir, dir.path());
        assert_eq!(seen[0].names, [".uploads".to_owned(), "k".to_owned()]);
    }
    let again = backend.put("k", body("two")).await;
    assert!(matches!(again, Err(Error::AlreadyExists(_))));
    assert_eq!(count(&seen), 1);
}

/// Asserts that a replacement with a matching tag syncs the directory after the new body is
/// renamed over the name, and that one with a stale tag syncs nothing. It would catch a
/// replacement that acks with the rename unsynced, and one that syncs before the rename (the
/// recorder would read the old body).
#[tokio::test]
async fn a_replacement_syncs_the_directory_after_the_new_body_is_in_place() {
    let (dir, backend, seen) = recorded(Some("k"), false);
    backend.put("k", body("old")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;
    backend.put_if_match("k", body("new"), &tag).await.unwrap();
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].dir, dir.path());
        let watched = seen[1].watched.as_ref().unwrap();
        assert_eq!(watched.len(), TAG_LEN + 3);
        assert!(watched.ends_with(b"new"));
    }
    let stale = backend.put_if_match("k", body("newer"), &tag).await;
    assert!(matches!(stale, Err(Error::PreconditionFailed(_))));
    assert_eq!(count(&seen), 2);
}

/// Asserts that a delete syncs the directory after the name is gone, and that deleting a key
/// that is absent syncs nothing. It would catch a delete that acks with the removal unsynced
/// (the object could come back after a power loss), and one that syncs before it removes.
#[tokio::test]
async fn a_delete_syncs_the_directory_after_the_name_is_removed() {
    let (dir, backend, seen) = recorded(None, false);
    backend.put("k", body("one")).await.unwrap();
    backend.delete("k").await.unwrap();
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].dir, dir.path());
        assert_eq!(seen[1].names, [".uploads".to_owned()]);
    }
    backend.delete("k").await.unwrap();
    assert_eq!(count(&seen), 2);
}

/// Asserts that placing a part syncs the upload's directory after the part has its name, and that
/// the completion syncs the bucket's directory after the object is linked. It would catch a part
/// or a completion that acks with its name unsynced.
#[tokio::test]
async fn a_part_and_a_completion_sync_the_directory_they_changed() {
    let (dir, backend, seen) = recorded(None, false);
    let id = backend.create_upload("k").await.unwrap();
    let tag = backend.upload_part("k", &id, 1, part("abc")).await.unwrap();
    let upload = dir.path().join(".uploads").join(&id);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].dir, upload);
        assert!(seen[0].names.contains(&format!("00001-{tag}")));
    }
    backend
        .complete_upload("k", &id, &[(1, tag)])
        .await
        .unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].dir, dir.path());
    assert!(seen[1].names.contains(&"k".to_owned()));
}

/// Asserts that when the directory cannot be synced no call that changes the directory returns
/// success: the put, the replacement, the delete, the part and the completion each fail. It would
/// catch a sync whose error is dropped, which acks a change the disk did not take.
#[tokio::test]
async fn a_directory_that_cannot_be_synced_fails_the_call() {
    let (_dir, backend, _seen) = recorded(None, true);
    let put = backend.put("k", body("one")).await;
    assert!(matches!(put, Err(Error::Other(_))));
    let tag = backend.head("k").await.unwrap().tag;
    let replaced = backend.put_if_match("k", body("two"), &tag).await;
    assert!(matches!(replaced, Err(Error::Other(_))));
    let deleted = backend.delete("k").await;
    assert!(matches!(deleted, Err(Error::Other(_))));
    let id = backend.create_upload("j").await.unwrap();
    let placed = backend.upload_part("j", &id, 1, part("abc")).await;
    assert!(matches!(placed, Err(Error::Other(_))));
}

/// Asserts that a completion whose directory sync fails is an error. The part is put in place by
/// hand, since placing it through the backend would fail the same sync first.
#[tokio::test]
async fn a_completion_whose_directory_cannot_be_synced_fails() {
    let (dir, backend, _seen) = recorded(None, true);
    let id = backend.create_upload("k").await.unwrap();
    let upload = dir.path().join(".uploads").join(&id);
    let tag = "0".repeat(2 * TAG_LEN);
    std::fs::write(upload.join(format!("00001-{tag}")), b"abc").unwrap();
    let done = backend.complete_upload("k", &id, &[(1, tag)]).await;
    assert!(matches!(done, Err(Error::Other(_))));
}

/// Asserts that the real sync of a directory succeeds on a directory and fails on one that is
/// absent.
#[tokio::test]
async fn the_real_sync_of_a_directory_succeeds_and_of_a_missing_one_fails() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-fsync");
    super::FsyncDir.sync_dir(dir.path()).await.unwrap();
    let missing = dir.path().join("absent");
    assert!(super::FsyncDir.sync_dir(&missing).await.is_err());
}
