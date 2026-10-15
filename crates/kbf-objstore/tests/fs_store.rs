//! `FsStore`, the object store in a local directory: the conformance suite, what it
//! keeps across a reopen, its lock, concurrent writers of one key, and how it reports
//! files that are not what it wrote and failing file system calls.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::{
    ByteRange, FsStore, FsStoreError, KeyPrefix, ObjectKey, ObjectStore, ObjectStoreError,
    PageSize, conformance,
};
use sha2::{Digest, Sha256};

/// A floor every file system the tests run on meets: one byte.
const FLOOR: u64 = 1;

/// A fresh, absent directory for `name`, under the target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-objstore-fs")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).expect("a valid key")
}

/// Where the module docs say `key`'s object file is: `objects/<hh>/<sha256 of key>`.
fn file_of(dir: &Path, key: &ObjectKey) -> PathBuf {
    let name = hex::encode(Sha256::digest(key.as_str().as_bytes()));
    dir.join("objects").join(&name[..2]).join(name)
}

fn all(len: u64) -> ByteRange {
    ByteRange::new(0, len).expect("a non-empty range")
}

/// Catches any break in the base contract or in the conditional writes the store
/// claims (`put_new` refusing an existing key), and a store that claims Object Lock or
/// accepts a retention date it cannot keep. The server trusts a store only once it
/// passes.
#[tokio::test]
async fn passes_conformance_with_conditional_writes() {
    let dir = scratch("conformance");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    let caps = store.capabilities();
    assert!(caps.conditional_put && !caps.object_lock, "{caps:?}");
    let prefix = KeyPrefix::new("conformance/").expect("a prefix");
    let report = conformance::run(&store, &prefix).await;
    assert!(report.passed(), "{report}");
    assert_eq!(report.skipped().count(), 0, "{report}");
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a store that does not outlive its process (objects kept in memory, or under
/// a name the next open does not read), and an open that keeps a file a crash left in
/// `tmp/`: it must be removed, never read or listed as an object.
#[tokio::test]
async fn objects_survive_a_reopen_and_torn_writes_do_not() {
    let dir = scratch("reopen");
    let k = key("kbf/0123456789abcdef/cas/0000000000000001/0000000000000001");
    let body = Bytes::from_static(b"the bytes of one segment");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    store.put_new(&k, body.clone(), None).await.expect("put");
    drop(store);
    let torn = dir.join("tmp").join("0");
    std::fs::write(&torn, b"kbf-obj1 a write cut short").expect("plant");

    let store = FsStore::open(&dir, FLOOR).expect("reopen");
    assert!(!torn.exists(), "a torn temporary file outlived the open");
    let got = store.get_range(&k, all(100)).await.expect("read");
    assert_eq!(got, body);
    let everything = KeyPrefix::new("").expect("the empty prefix");
    let page = store
        .list(&everything, None, PageSize::MAX)
        .await
        .expect("list");
    assert_eq!(page.objects.len(), 1, "{page:?}");
    assert_eq!(page.objects[0].key, k);
    assert_eq!(page.objects[0].size, body.len() as u64);
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches two stores sharing one directory: the second would empty `tmp/` under the
/// first's writes. The open is refused, naming the lock file, until the first store is
/// dropped.
#[test]
fn a_second_open_is_refused_until_the_first_store_closes() {
    let dir = scratch("lock");
    let first = FsStore::open(&dir, FLOOR).expect("open");
    let Err(refused) = FsStore::open(&dir, FLOOR) else {
        panic!("a second open of a locked directory succeeded");
    };
    assert!(
        matches!(refused, FsStoreError::Locked { .. }),
        "{refused:?}"
    );
    let text = refused.to_string();
    assert!(
        text.contains(&format!("lock {}", dir.join("LOCK").display())),
        "{text}"
    );
    drop(first);
    drop(FsStore::open(&dir, FLOOR).expect("open after the first closed"));
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a write that replaces an existing object (a rename where a link belongs):
/// of concurrent writers of one key, exactly one succeeds, every other is answered
/// `AlreadyExists`, and the object holds the winner's bytes, whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_concurrent_writers_of_one_key_exactly_one_wins() {
    let dir = scratch("race");
    let store = Arc::new(FsStore::open(&dir, FLOOR).expect("open"));
    for round in 0..20 {
        let k = key(&format!("race/{round}"));
        let writers: Vec<_> = (0..4u8)
            .map(|w| {
                let (store, k) = (Arc::clone(&store), k.clone());
                let body = Bytes::from(vec![w; 64 << 10]);
                tokio::spawn(async move { (body.clone(), store.put_new(&k, body, None).await) })
            })
            .collect();
        let mut won = Vec::new();
        for writer in writers {
            match writer.await.expect("a writer") {
                (body, Ok(())) => won.push(body),
                (_, Err(ObjectStoreError::AlreadyExists(_))) => {}
                (_, Err(e)) => panic!("round {round}: {e}"),
            }
        }
        assert_eq!(
            won.len(),
            1,
            "round {round}: {} writers succeeded",
            won.len()
        );
        let got = store.get_range(&k, all(64 << 10)).await.expect("read");
        assert_eq!(got, won[0], "round {round}: not the winner's bytes");
    }
    assert_eq!(
        std::fs::read_dir(dir.join("tmp")).expect("tmp").count(),
        0,
        "a temporary file was left behind"
    );
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a read that trusts the file under a key's name: a file holding another
/// key's object (moved, or a hash bug) is refused as a protocol break, never returned
/// as the key's bytes; and a file too short for its header (a final name whose bytes
/// never reached the disk) or of zero bytes is no object: the read is `NotFound`,
/// `may_hold` is false, and a listing skips it. A whole object `may_hold`.
#[tokio::test]
async fn a_file_that_is_not_the_key_s_object_is_refused() {
    let dir = scratch("foreign");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    let (a, b, c, z) = (key("a"), key("b"), key("c"), key("z"));
    store
        .put_new(&a, Bytes::from_static(b"object a"), None)
        .await
        .expect("put a");
    assert!(store.may_hold(&a).await.expect("may_hold a"));
    std::fs::copy(file_of(&dir, &a), file_of(&dir, &b)).expect("copy a's file to b's name");
    let Err(ObjectStoreError::Protocol(why)) = store.get_range(&b, all(8)).await else {
        panic!("a's object read as b's");
    };
    assert!(why.contains("holds object a, not b"), "{why}");
    assert!(!store.may_hold(&b).await.expect("may_hold b"));

    std::fs::write(file_of(&dir, &c), b"kbf-obj1").expect("a header cut short");
    std::fs::write(file_of(&dir, &z), b"").expect("a file of zero bytes");
    for k in [&c, &z] {
        let read = store.get_range(k, all(8)).await;
        assert!(
            matches!(&read, Err(ObjectStoreError::NotFound(nf)) if nf == k),
            "{k}: {read:?}"
        );
        assert!(!store.may_hold(k).await.expect("may_hold"), "{k}");
    }
    assert!(!store.may_hold(&key("never")).await.expect("may_hold"));
    let listed = store
        .list(&KeyPrefix::new("").expect("prefix"), None, PageSize::MAX)
        .await
        .expect("list");
    let keys: Vec<&str> = listed.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["a"], "{listed:?}");
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a write past the free-space floor: with a floor no file system meets the
/// write is refused as a transport error that says how much is free and that nothing
/// is evicted, and nothing is stored; with the tiny floor the same write is stored.
#[tokio::test]
async fn a_write_under_the_free_space_floor_is_refused() {
    let dir = scratch("floor");
    let k = key("k");
    let body = Bytes::from_static(b"body");
    let full = FsStore::open(&dir, u64::MAX).expect("open");
    let Err(e @ ObjectStoreError::Transport(_)) = full.put_new(&k, body.clone(), None).await else {
        panic!("a write under the floor was stored");
    };
    let text = e.to_string();
    assert!(text.contains("bytes free, under the floor of"), "{text}");
    assert!(text.contains("no eviction"), "{text}");
    assert!(!full.may_hold(&k).await.expect("may_hold"));
    drop(full);
    let roomy = FsStore::open(&dir, FLOOR).expect("open");
    roomy.put_new(&k, body.clone(), None).await.expect("put");
    assert_eq!(roomy.get_range(&k, all(4)).await.expect("read"), body);
    drop(roomy);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a shard scan that reports the file's length rather than the object's (the
/// header counted), misses an object, reports one in the wrong shard, or takes a file
/// with no header for an object; and a floor check that disagrees with the write's.
#[tokio::test]
async fn a_scan_reads_each_shard_s_headers() {
    let dir = scratch("scan");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    assert!(!store.under_floor().expect("statvfs"));
    let before = std::time::SystemTime::now() - std::time::Duration::from_secs(2);
    let keys = [key("a"), key("bb"), key("a/longer/key")];
    for (k, len) in keys.iter().zip([0, 5, 300]) {
        store
            .put_new(k, Bytes::from(vec![1; len]), None)
            .await
            .expect("put");
    }
    let headerless = file_of(&dir, &key("never-written"));
    std::fs::write(&headerless, b"kbf").expect("plant a file with no header");

    let mut found = Vec::new();
    let mut unreadable = 0;
    for shard in 0..=u8::MAX {
        let scan = store.scan(shard).await.expect("scan");
        assert_eq!(scan.opened(), scan.objects.len() + scan.unreadable);
        unreadable += scan.unreadable;
        for object in scan.objects {
            let file = file_of(&dir, &object.key);
            assert_eq!(
                file.parent(),
                Some(dir.join("objects").join(format!("{shard:02x}")).as_path())
            );
            assert!(object.mtime >= before, "{object:?}");
            found.push((object.key.as_str().to_owned(), object.len));
        }
    }
    found.sort();
    let want = [("a", 0), ("a/longer/key", 300), ("bb", 5)].map(|(k, n)| (k.to_owned(), n));
    assert_eq!(found, want);
    assert_eq!(unreadable, 1);
    assert_eq!(FsStore::SHARDS, 256);
    drop(store);
    let full = FsStore::open(&dir, u64::MAX).expect("open");
    assert!(full.under_floor().expect("statvfs"));
    drop(full);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a file system failure reported as anything but a transport error that names
/// the operation and the path (a missing object, or a success), on every call: the
/// directory a key's object goes in replaced by a file (so creating, reading,
/// removing and listing there fail), and `tmp/` removed (so a write cannot start).
#[tokio::test]
async fn file_system_failures_name_the_operation_and_the_path() {
    let dir = scratch("failures");
    let store = FsStore::open(&dir, FLOOR).expect("open");
    let k = key("k");
    let object = file_of(&dir, &k);
    let shard = object.parent().expect("a shard").to_owned();
    std::fs::remove_dir(&shard).expect("remove the shard");
    std::fs::write(&shard, b"not a directory").expect("plant a file");
    let transport = |what: &str, r: Result<(), ObjectStoreError>, op: &str, path: &Path| {
        let Err(e @ ObjectStoreError::Transport(_)) = r else {
            panic!("{what}: {r:?}");
        };
        let text = e.to_string();
        let want = format!("request to the store failed: {op} {}: ", path.display());
        assert!(text.starts_with(&want), "{what}: {text}");
    };
    let body = Bytes::from_static(b"body");
    let put = store.put_new(&k, body.clone(), None).await;
    transport("put", put, "link", &object);
    let get = store.get_range(&k, all(4)).await.map(drop);
    transport("get", get, "open", &object);
    transport("delete", store.delete(&k).await, "remove", &object);
    let may_hold = store.may_hold(&k).await.map(drop);
    transport("may_hold", may_hold, "open", &object);
    let everything = KeyPrefix::new("").expect("prefix");
    let list = store.list(&everything, None, PageSize::MAX).await.map(drop);
    transport("list", list, "list", &shard);
    assert_eq!(
        std::fs::read_dir(dir.join("tmp")).expect("tmp").count(),
        0,
        "a failed write left its temporary file"
    );

    std::fs::remove_dir(dir.join("tmp")).expect("remove tmp");
    let put = store.put_new(&key("other"), body, None).await;
    transport("put without tmp", put, "create", &dir.join("tmp").join("1"));
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches an open that reports a directory it cannot create as anything but an I/O
/// error naming the operation and the path.
#[test]
fn an_open_that_cannot_create_the_directory_says_where() {
    let dir = scratch("unopenable");
    std::fs::create_dir_all(&dir).expect("scratch");
    let file = dir.join("a-file");
    std::fs::write(&file, b"not a directory").expect("plant");
    let Err(e) = FsStore::open(&file.join("store"), FLOOR) else {
        panic!("a store opened under a file");
    };
    assert!(matches!(e, FsStoreError::Io { .. }), "{e:?}");
    let text = e.to_string();
    let want = format!("create {}: ", file.join("store").display());
    assert!(text.starts_with(&want), "{text}");
    std::fs::remove_dir_all(&dir).expect("remove");
}
