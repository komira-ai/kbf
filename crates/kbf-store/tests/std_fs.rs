//! The store on the real filesystem: what the fault-injecting tests prove over
//! `FaultFs` holds over `StdFs` too.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use kbf_raft::{Entry, HardState, LogId, LogIndex, Payload, ServerId, Term};
use kbf_store::{Options, StdFs, Store, StoreError};

fn entry(index: u64, term: u64) -> Entry {
    Entry {
        id: LogId::new(Term(term), LogIndex(index)),
        payload: Payload::Command(index.to_le_bytes().to_vec()),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("kbf-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Catches: a `StdFs` that does not create its directory, list, read, create, append,
/// rename or remove as the store needs (a reopen would not read back the log, the
/// truncation segment or the hard state), and a torn tail on a real file not cut off.
#[test]
fn persists_reopens_and_cuts_a_torn_tail() {
    let dir = scratch("roundtrip");
    let opts = Options { segment_bytes: 64 };
    let hard = HardState {
        term: Term(4),
        voted_for: Some(ServerId(3)),
    };
    {
        let fs = StdFs::new(&dir).unwrap();
        assert_eq!(fs.dir(), dir);
        let (mut store, rec) = Store::open(fs, opts).unwrap();
        assert!(rec.entries.is_empty());
        store.persist_hard_state(HardState::default()).unwrap();
        store.persist_hard_state(hard).unwrap();
        let first: Vec<Entry> = (1..=5).map(|i| entry(i, 1)).collect();
        store.persist_entries(&first).unwrap();
        store.persist_entries(&[entry(6, 1)]).unwrap();
        store.persist_entries(&[entry(4, 2)]).unwrap();
    }
    let want: Vec<Entry> = (1..=3).map(|i| entry(i, 1)).chain([entry(4, 2)]).collect();
    let segs = |d: &PathBuf| {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".log"))
            .collect();
        v.sort();
        v
    };
    let names = segs(&dir);
    assert_eq!(names.len(), 2, "{names:?}");

    let last = dir.join(names.last().unwrap());
    let len = std::fs::metadata(&last).unwrap().len();
    OpenOptions::new()
        .append(true)
        .open(&last)
        .unwrap()
        .write_all(&[0x2A, 0, 0, 0, 1, 2])
        .unwrap();
    let (mut store, rec) = Store::open(StdFs::new(&dir).unwrap(), opts).unwrap();
    assert_eq!(rec.hard, hard);
    assert_eq!(rec.entries, want);
    assert_eq!(
        std::fs::metadata(&last).unwrap().len(),
        len,
        "torn tail cut"
    );
    store.persist_entries(&[entry(5, 2)]).unwrap();
    drop(store);

    let (_, rec) = Store::open(StdFs::new(&dir).unwrap(), opts).unwrap();
    assert_eq!(rec.entries.len(), 5);

    std::fs::write(dir.join(names.first().unwrap()), b"\x01\0\0\0\0\0\0\0x").unwrap();
    assert!(matches!(
        Store::open(StdFs::new(&dir).unwrap(), opts),
        Err(StoreError::Corrupt { .. })
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Catches: `StdFs` errors that are swallowed: a create over an existing file, an open
/// of a missing one, and a directory that cannot be created.
#[test]
fn std_fs_reports_errors() {
    use kbf_store::{Fs, FsFile};
    let dir = scratch("errors");
    let fs = StdFs::new(&dir).unwrap();
    let mut f = fs.create("a").unwrap();
    f.append(b"abc").unwrap();
    f.truncate(1).unwrap();
    f.sync().unwrap();
    assert_eq!(fs.read("a").unwrap(), b"a");
    assert!(fs.create("a").is_err());
    assert!(fs.open_append("b").is_err());
    assert!(fs.rename("b", "c").is_err());
    assert!(fs.remove("b").is_err());
    assert!(StdFs::new(dir.join("a").join("sub")).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Catches: a `StdFs` block file opened for appending (on Linux a positioned write to
/// it lands at the end, whatever its offset), a positioned write that does not fill a
/// gap with zeros, a ranged read that returns short past the end, a wrong length, and
/// a file of one mode taking the other mode's writes.
#[test]
fn std_fs_positioned_writes_and_ranged_reads() {
    use kbf_store::{Fs, FsFile};
    let dir = scratch("positioned");
    let fs = StdFs::new(&dir).unwrap();
    let f = fs.create_positioned("b").unwrap();
    f.write_at(4, b"wxyz").unwrap();
    f.write_at(0, b"ab").unwrap();
    f.sync().unwrap();
    assert_eq!(fs.read("b").unwrap(), b"ab\0\0wxyz");
    assert_eq!(fs.file_len("b").unwrap(), 8);
    assert_eq!(fs.read_at("b", 3, 3).unwrap(), b"\0wx");
    assert_eq!(
        fs.read_at("b", 6, 3).unwrap_err().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    assert!(fs.read_at("q", 0, 1).is_err());
    assert!(fs.file_len("q").is_err());
    assert!(fs.create_positioned("b").is_err());
    let mut f = f;
    assert_eq!(
        f.append(b"x").unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    let a = fs.open_append("b").unwrap();
    assert_eq!(
        a.write_at(0, b"x").unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(fs.read("b").unwrap(), b"ab\0\0wxyz");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Catches: a directory listing that drops or mangles a file name that is not UTF-8
/// (the store would open as if the file were not there); it must fail instead. Linux
/// only: other filesystems refuse such names.
#[cfg(target_os = "linux")]
#[test]
fn a_non_utf8_name_fails_the_listing() {
    use kbf_store::Fs;
    use std::os::unix::ffi::OsStrExt;
    let dir = scratch("non-utf8");
    let fs = StdFs::new(&dir).unwrap();
    std::fs::write(dir.join(std::ffi::OsStr::from_bytes(b"bad\xff")), b"").unwrap();
    assert_eq!(
        fs.list().unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
