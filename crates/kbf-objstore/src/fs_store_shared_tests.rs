use kbf_testkit::dir::TestDir;

use super::*;

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).expect("a valid key")
}

fn dir(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

async fn put(store: &FsStore, name: &str, body: &'static [u8]) -> Result<(), ObjectStoreError> {
    store
        .put_new(&key(name), Bytes::from_static(body), None)
        .await
}

async fn read(store: &FsStore, name: &str) -> Result<Bytes, ObjectStoreError> {
    let all = ByteRange::new(0, 1024).expect("a range");
    store.get_range(&key(name), all).await
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = entries(dir)
        .expect("list")
        .iter()
        .map(|p| file_name(p))
        .collect();
    names.sort();
    names
}

/// A file named as a temporary file of `store` is: its prefix and `n`.
fn write_tmp(store: &FsStore, n: u64) -> PathBuf {
    let path = store
        .inner
        .tmp
        .join(format!("{}{n}", store.inner.tmp_prefix));
    fs::write(&path, b"in flight").expect("write a temporary file");
    path
}

/// Two shared stores open on one directory at once, in one process as in two, each write
/// the other's reads see.
#[tokio::test]
async fn shared_stores_on_one_directory_read_each_others_objects() {
    let dir = dir("kbf-objstore-shared-both");
    let a = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("open a");
    let b = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("open b");
    put(&a, "from/a", b"alpha").await.expect("put a");
    put(&b, "from/b", b"beta").await.expect("put b");
    assert_eq!(read(&a, "from/b").await.expect("a reads b"), "beta");
    assert_eq!(read(&b, "from/a").await.expect("b reads a"), "alpha");
    assert_ne!(a.inner.tmp_prefix, b.inner.tmp_prefix);
}

/// The rollout case: the previous release's exclusive store holds `LOCK`, and the new
/// release's shared store opens beside it and writes.
#[tokio::test]
async fn a_shared_open_succeeds_while_an_exclusive_store_holds_the_lock() {
    let dir = dir("kbf-objstore-shared-beside-exclusive");
    let old = FsStore::open(&dir, Floor::Bytes(1)).expect("exclusive");
    let new = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("shared beside it");
    put(&new, "k", b"v").await.expect("put");
    assert_eq!(read(&old, "k").await.expect("old reads"), "v");
    assert!(matches!(
        FsStore::open(&dir, Floor::Bytes(1)),
        Err(FsStoreError::Locked { .. })
    ));
}

/// An exclusive open is refused while another exclusive store is open, shared stores or
/// not, and a shared store's open does not take `LOCK`.
#[tokio::test]
async fn an_exclusive_open_is_refused_while_another_exclusive_store_is_open() {
    let dir = dir("kbf-objstore-shared-locked");
    let first = FsStore::open(&dir, Floor::Bytes(1)).expect("first");
    let err = FsStore::open(&dir, Floor::Bytes(1))
        .map(drop)
        .expect_err("refused");
    assert!(matches!(err, FsStoreError::Locked { .. }), "{err}");
    drop(first);
    let shared = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("shared");
    let second = FsStore::open(&dir, Floor::Bytes(1)).expect("a shared store holds no LOCK");
    drop((shared, second));
}

/// A shared open leaves the temporary file of a running instance, whose name is that
/// instance's, where it is.
#[tokio::test]
async fn a_shared_open_keeps_a_running_instances_temporary_file() {
    let dir = dir("kbf-objstore-shared-inflight");
    let first = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("first");
    let mine = write_tmp(&first, 0);
    let second = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("second");
    assert!(mine.exists(), "the first's in-flight file was removed");
    put(&second, "k", b"v").await.expect("put");
    assert!(mine.exists());
}

/// A shared open removes what an instance whose process is gone left (its temporary
/// files in `tmp-shared/` and its instance file) and the temporary files that name no
/// instance, and keeps
/// a running instance's, which another store of this process stands for here.
#[tokio::test]
async fn a_shared_open_removes_a_stopped_instances_leftovers() {
    let dir = dir("kbf-objstore-shared-stopped");
    let live = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("live");
    let kept = write_tmp(&live, 3);
    let live_name = file_name(&kept)
        .split_once('.')
        .expect("a dot")
        .0
        .to_owned();
    let tmp = dir.join("tmp-shared");
    let instances = dir.join("instances");
    // An instance whose process is gone: its file is not locked by anyone.
    fs::write(instances.join("gone-1-0"), b"").expect("instance");
    fs::write(tmp.join("gone-1-0.0"), b"x").expect("tmp");
    fs::write(tmp.join("gone-1-0.7"), b"x").expect("tmp");
    // Temporary files that name no instance.
    fs::write(tmp.join("ghost-2-0.1"), b"x").expect("tmp");
    // A directory a shared open cannot remove with `remove_file` is left, not an error.
    fs::create_dir(tmp.join("stuck-3-0.0")).expect("dir");

    let fresh = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("fresh");

    assert!(kept.exists(), "the running instance's file was removed");
    let mut want = vec![
        live_name,
        file_name(&fresh.inner.instance.clone().expect("path")),
    ];
    want.sort();
    assert_eq!(names(&instances), want);
    let left: Vec<String> = names(&tmp);
    assert_eq!(left, [file_name(&kept), "stuck-3-0.0".to_owned()]);
}

/// Every file in `tmp/` (only exclusive mode writes there, named by a number) is removed
/// by a shared open when `LOCK` is free, and left while an exclusive store holds it,
/// since that store's writes are in flight.
#[tokio::test]
async fn a_shared_open_removes_exclusive_temporary_files_only_when_no_exclusive_store_is_open() {
    let dir = dir("kbf-objstore-shared-legacy");
    let exclusive = FsStore::open(&dir, Floor::Bytes(1)).expect("exclusive");
    let tmp = dir.join("tmp");
    fs::write(tmp.join("0"), b"in flight").expect("write a temporary file");
    fs::write(tmp.join("old-1-0.5"), b"in flight").expect("write a temporary file");
    let beside = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("shared");
    assert!(tmp.join("0").exists(), "removed under a held LOCK");
    assert!(tmp.join("old-1-0.5").exists(), "removed under a held LOCK");
    drop((beside, exclusive));

    let after = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("shared");
    assert!(!tmp.join("0").exists(), "left with LOCK free");
    assert!(!tmp.join("old-1-0.5").exists(), "left with LOCK free");
    // The lock the open took is released: an exclusive store opens at once.
    drop(after);
    FsStore::open(&dir, Floor::Bytes(1)).expect("LOCK was left held");
}

/// Two shared stores that write one key: one write is answered, the other
/// `AlreadyExists`, and the object is the first's.
#[tokio::test]
async fn two_shared_stores_writing_one_key_leave_the_first_writers_object() {
    let dir = dir("kbf-objstore-shared-same-key");
    let a = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("a");
    let b = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("b");
    put(&a, "k", b"first").await.expect("first");
    let second = put(&b, "k", b"second").await;
    assert!(
        matches!(&second, Err(ObjectStoreError::AlreadyExists(k)) if k.as_str() == "k"),
        "{second:?}"
    );
    assert_eq!(read(&b, "k").await.expect("read"), "first");
    assert_eq!(names(&dir.join("tmp-shared")), Vec::<String>::new());
    assert_eq!(names(&dir.join("tmp")), Vec::<String>::new());
}

/// Previous release on new data: an exclusive open of a directory shared stores wrote
/// reads their objects. New release on old data: a shared open of a directory an
/// exclusive store wrote reads its objects.
#[tokio::test]
async fn exclusive_and_shared_opens_read_what_the_other_wrote() {
    let dir = dir("kbf-objstore-shared-formats");
    let shared = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("shared");
    put(&shared, "new/data", b"from shared").await.expect("put");
    let exclusive = FsStore::open(&dir, Floor::Bytes(1)).expect("exclusive");
    assert_eq!(
        read(&exclusive, "new/data").await.expect("read"),
        "from shared"
    );
    put(&exclusive, "old/data", b"from exclusive")
        .await
        .expect("put");
    drop((shared, exclusive));
    let again = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("shared");
    assert_eq!(
        read(&again, "old/data").await.expect("read"),
        "from exclusive"
    );
}

/// A shared store keeps its temporary files in `tmp-shared/` and none in `tmp/`.
#[tokio::test]
async fn a_shared_store_writes_its_temporary_files_under_tmp_shared_only() {
    let dir = dir("kbf-objstore-shared-tmp-dir");
    let shared = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("shared");
    let written = write_tmp(&shared, 0);
    assert_eq!(written.parent(), Some(dir.join("tmp-shared").as_path()));
    assert_eq!(names(&dir.join("tmp")), Vec::<String>::new());
}

/// An exclusive open (the previous release's, started beside a shared store) leaves a
/// shared store's write in flight alone: the open succeeds, the file stays, and the
/// write, a streamed put whose body is not finished when the open happens, completes.
#[tokio::test]
async fn an_exclusive_open_beside_a_shared_stores_write_in_flight_cuts_nothing() {
    let dir = dir("kbf-objstore-shared-cut");
    let shared = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("shared");
    let (send, recv) = futures::channel::mpsc::unbounded::<Result<Bytes, ObjectStoreError>>();
    let writer = shared.clone();
    let put = tokio::spawn(async move { writer.put_stream(&key("k"), 10, recv.boxed()).await });
    let tmp = dir.join("tmp-shared");
    while names(&tmp).is_empty() {
        tokio::task::yield_now().await;
    }
    let in_flight = names(&tmp);
    send.unbounded_send(Ok(Bytes::from_static(b"hello")))
        .expect("first piece");

    let exclusive = FsStore::open(&dir, Floor::Bytes(1)).expect("exclusive");

    assert_eq!(names(&tmp), in_flight, "the in-flight file was touched");
    send.unbounded_send(Ok(Bytes::from_static(b"world")))
        .expect("second piece");
    drop(send);
    put.await
        .expect("join")
        .expect("the write in flight completes");
    assert_eq!(read(&exclusive, "k").await.expect("read"), "helloworld");
    assert_eq!(names(&tmp), Vec::<String>::new());
}

/// An exclusive open succeeds with `tmp-shared/` holding files, those of a running
/// instance and of a stopped one, and leaves every one of them where it is.
#[tokio::test]
async fn an_exclusive_open_leaves_a_non_empty_tmp_shared_untouched() {
    let dir = dir("kbf-objstore-shared-exclusive-open");
    let shared = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("shared");
    let mine = write_tmp(&shared, 900);
    let stopped = dir.join("tmp-shared").join("gone-1-0.0");
    fs::write(&stopped, b"x").expect("write a temporary file");
    let exclusive = FsStore::open(&dir, Floor::Bytes(1)).expect("exclusive");
    assert!(mine.exists() && stopped.exists());
    put(&shared, "k", b"v").await.expect("put");
    assert_eq!(read(&exclusive, "k").await.expect("read"), "v");
}

/// An exclusive open empties `tmp/` whatever it holds, a dotted name included.
#[tokio::test]
async fn an_exclusive_open_empties_tmp() {
    let dir = dir("kbf-objstore-shared-exclusive-empties");
    let shared = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("shared");
    let tmp = dir.join("tmp");
    fs::write(tmp.join("0"), b"x").expect("write a temporary file");
    fs::write(tmp.join("a-1-0.2"), b"x").expect("write a temporary file");
    FsStore::open(&dir, Floor::Bytes(1)).expect("exclusive");
    assert_eq!(names(&tmp), Vec::<String>::new());
    drop(shared);
}

/// A leftover already gone counts as removed, so a removal that raced the open does not
/// fail it.
#[test]
fn a_leftover_that_is_already_gone_counts_as_removed() {
    let dir = dir("kbf-objstore-shared-leftover-gone");
    remove_leftover(&dir.join("absent")).expect("a missing file is removed");
    let present = dir.join("present");
    fs::write(&present, b"x").expect("file");
    remove_leftover(&present).expect("removed");
    assert!(!present.exists());
}

/// A leftover that cannot be removed for another reason (a directory) is an error.
#[test]
fn a_leftover_that_cannot_be_removed_is_an_error() {
    let dir = dir("kbf-objstore-shared-leftover-stuck");
    let stuck = dir.join("stuck");
    fs::create_dir(&stuck).expect("dir");
    let err = remove_leftover(&stuck).expect_err("a directory is not removed");
    assert_ne!(err.kind(), io::ErrorKind::NotFound);
    assert!(stuck.exists());
}

/// Dropping the last handle of a shared store removes its instance file, and a clone
/// keeps it until it is dropped too.
#[tokio::test]
async fn dropping_the_last_handle_of_a_shared_store_removes_its_instance_file() {
    let dir = dir("kbf-objstore-shared-drop");
    let store = FsStore::open_shared(&dir, Floor::Bytes(1)).expect("open");
    let clone = store.clone();
    assert_eq!(names(&dir.join("instances")).len(), 1);
    drop(store);
    assert_eq!(names(&dir.join("instances")).len(), 1);
    drop(clone);
    assert_eq!(names(&dir.join("instances")), Vec::<String>::new());
}

/// An exclusive store has no instance file and names its temporary files by number
/// alone, as before.
#[tokio::test]
async fn an_exclusive_store_names_temporary_files_by_number_only() {
    let dir = dir("kbf-objstore-shared-exclusive-names");
    let store = FsStore::open(&dir, Floor::Bytes(1)).expect("open");
    assert_eq!(store.inner.tmp_prefix, "");
    assert!(store.inner.instance.is_none());
    assert!(!dir.join("instances").exists());
}

/// A path under a regular file cannot be opened, shared or not: an error naming it.
#[tokio::test]
async fn a_shared_open_that_cannot_create_its_files_names_the_path() {
    let dir = dir("kbf-objstore-shared-refused");
    let file = dir.join("a-file");
    fs::write(&file, b"").expect("file");
    let err = FsStore::open_shared(&file.join("sub"), Floor::Bytes(1))
        .map(drop)
        .expect_err("refused");
    assert!(
        matches!(&err, FsStoreError::Io { op: "create", path, .. } if path == &file.join("sub")),
        "{err}"
    );
    // `instances` a file: its directory cannot be made.
    let store = dir.join("store");
    fs::create_dir(&store).expect("dir");
    fs::write(store.join("instances"), b"").expect("file");
    let err = FsStore::open_shared(&store, Floor::Bytes(1))
        .map(drop)
        .expect_err("refused");
    assert!(
        matches!(&err, FsStoreError::Io { op: "create", path, .. } if path == &store.join("instances")),
        "{err}"
    );
}

/// A store that finds its instance file removed after it locked it (a sweep took the
/// lock first) starts again under a new name, and the file it keeps is named by its path.
#[test]
fn a_claim_whose_file_was_removed_under_it_starts_again_with_a_new_name() {
    let dir = dir("kbf-objstore-shared-claim");
    let mut next = 0;
    let mut removed = 0;
    let (name, path, file) = claim(
        &dir,
        || {
            next += 1;
            format!("n{next}")
        },
        |path| {
            if removed == 0 {
                removed += 1;
                fs::remove_file(path).expect("a sweep removes it");
            }
        },
    )
    .expect("claim");
    assert_eq!(name, "n2");
    assert_eq!(names(&dir), ["n2"]);
    assert!(names_file(&file, &path));
    // A name taken already is an error, not a loop.
    let err = claim(&dir, || "n2".to_owned(), |_| {})
        .map(drop)
        .expect_err("taken");
    assert!(
        matches!(err, FsStoreError::Io { op: "create", .. }),
        "{err}"
    );
}

/// Names differ between stores opened in one process.
#[test]
fn instance_names_differ_and_hold_no_dot() {
    let names: BTreeSet<String> = (0..50).map(|_| instance_name()).collect();
    assert_eq!(names.len(), 50);
    assert!(names.iter().all(|n| !n.contains('.')));
}
