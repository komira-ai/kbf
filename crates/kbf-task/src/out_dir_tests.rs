//! The put directory on a plain directory, with a fake store: the store is a hook on `rename`
//! that records each rename, answers a chosen errno for the ones a test refuses, and renames the
//! rest. What is committed is read back from the directory.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::rc::Rc;

use kbf_segments::sha256;
use kbf_task_pack::{OutputFile, Timing, Usage};
use kbf_testkit::dir::TestDir;

use super::*;
use crate::digest::to_reapi;
use crate::task::{EXIT_PUT, EXIT_SETUP};

/// A fresh scratch directory for one test, removed when the test ends.
fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-task-out-dir-{name}"))
}

/// A put directory `out` under `dir`, made empty.
fn out_dir(dir: &Path) -> PathBuf {
    let out = dir.join("out");
    std::fs::create_dir(&out).expect("out");
    out
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
        .collect();
    names.sort();
    names
}

/// A manifest whose outputs are files of `bytes`, with a Command that exited 3, and `pack_ms` 7
/// (the time to collect, as the run gives it).
fn manifest(bytes: &[&[u8]]) -> TaskManifest {
    TaskManifest {
        lease: "lease-1".to_owned(),
        action: sha256(b"the action"),
        exit_code: 3,
        stdout: None,
        stderr: None,
        output_files: (bytes.iter().enumerate())
            .map(|(i, b)| OutputFile {
                path: format!("out/{i}"),
                digest: sha256(b),
                is_executable: false,
            })
            .collect(),
        output_directories: Vec::new(),
        output_symlinks: Vec::new(),
        timing: Timing {
            fetch_ms: 1,
            run_ms: 2,
            pack_ms: 7,
            ..Timing::default()
        },
        usage: Usage::default(),
        packs: 1,
        large: Vec::new(),
        inputs: Some(crate::tree::input_stats()),
        objects: None,
    }
}

/// The fake store: every rename it is asked for, by file name, and the errnos it answers.
#[derive(Clone, Default)]
struct Store {
    renames: Rc<RefCell<Vec<(String, String)>>>,
    /// By the name renamed to, the digest rename's as `<name>.sha256`: the errnos to answer, in
    /// order, before renaming.
    refusals: Rc<RefCell<BTreeMap<String, VecDeque<i32>>>>,
    pauses: Rc<RefCell<Vec<Duration>>>,
}

impl Store {
    /// Answers the renames to `to` with `errnos`, one each, then renames.
    fn refuse(&self, to: &str, errnos: &[i32]) {
        (self.refusals.borrow_mut()).insert(to.to_owned(), errnos.iter().copied().collect());
    }

    /// `out` renaming and pausing through this store.
    fn serving(&self, out: OutDir) -> OutDir {
        let store = self.clone();
        let pauses = Rc::clone(&self.pauses);
        out.with_rename(move |from, to| {
            let name = |p: &Path| {
                p.file_name()
                    .expect("a name")
                    .to_string_lossy()
                    .into_owned()
            };
            let (from_name, to_name) = (name(from), name(to));
            let key = match to_name.find(".sha256-") {
                Some(at) => format!("{}.sha256", &to_name[..at]),
                None => to_name.clone(),
            };
            store.renames.borrow_mut().push((from_name, to_name));
            let refused = (store.refusals.borrow_mut().get_mut(&key)).and_then(VecDeque::pop_front);
            match refused {
                Some(errno) => Err(io::Error::from_raw_os_error(errno)),
                None => std::fs::rename(from, to),
            }
        })
        .with_pause(move |wait| pauses.borrow_mut().push(wait))
    }

    /// The commit renames asked for, by the name committed, in order.
    fn commits(&self) -> Vec<String> {
        (self.renames.borrow().iter())
            .filter(|(_, to)| !to.ends_with(".part"))
            .map(|(_, to)| to.clone())
            .collect()
    }

    fn pauses(&self) -> Vec<Duration> {
        self.pauses.borrow().clone()
    }
}

/// The put directory at `out`, served by a new fake store.
fn served(out: &Path) -> (OutDir, Store) {
    let store = Store::default();
    let dir = store.serving(OutDir::open(out).expect("open"));
    (dir, store)
}

/// The committed object `name` under `out`, after checking it was committed by the two renames
/// the design names, the first to the name of its own SHA-256.
fn committed(out: &Path, store: &Store, name: &str) -> Vec<u8> {
    let bytes = std::fs::read(out.join(name)).expect("committed");
    let digest_name = digest_part_name(name, &sha256(&bytes));
    let renames = store.renames.borrow();
    let mine: Vec<_> = (renames.iter())
        .filter(|(from, _)| from.starts_with(&format!("{name}.")))
        .cloned()
        .collect();
    assert_eq!(
        mine,
        [
            (part_name(name), digest_name.clone()),
            (digest_name, name.to_owned())
        ],
        "{name}"
    );
    bytes
}

/// The run's one object holding `bytes`, put through `sink` as a run does: the object created, the
/// blob put, the manifest ended and the object committed. Returns the manifest it ended with.
async fn store_one(sink: &OutDir, bytes: &[u8]) -> Result<TaskManifest, Failure> {
    let object = sink.object()?;
    // A put the file refuses fails the run as a write to the put directory (exit 13).
    (object.spool())
        .put(&to_reapi(&sha256(bytes)), bytes.to_vec())
        .await
        .map_err(write_error(OBJECT_NAME))?;
    let mut manifest = manifest(&[bytes]);
    object.finish(&mut manifest).await?;
    Ok(manifest)
}

/// Asserts the whole of a run: the directory ends holding `pack` alone, committed by the digest
/// rename then the commit rename; its bytes are the one-pass object, which holds the blob; the
/// manifest returned is the one the object ends with, the Command's exit code 3 in it.
#[tokio::test]
async fn a_run_commits_pack_alone_by_the_digest_rename_then_the_commit_rename() {
    let dir = scratch("one");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    let manifest = store_one(&sink, b"payload").await.expect("stored");
    assert_eq!(manifest.exit_code, 3);
    assert_eq!(manifest.timing.walk_ms, Some(7));
    assert_eq!(names(&out), ["pack"]);
    let object = committed(&out, &store, "pack");
    assert!(
        object.windows(7).any(|w| w == b"payload"),
        "the object holds the blob"
    );
    assert_eq!(store.commits(), ["pack"]);
    assert!(store.pauses().is_empty());
}

/// Asserts that a commit the store refuses for good is exit 13 with the object, the attempt, the
/// errno and what the store means by it, with exact text, is not tried again, and leaves no
/// `.part` file: one case per errno of the store's mapping that is final, and one it does not
/// name.
#[tokio::test]
async fn a_refused_commit_is_exit_13_with_its_errno_and_leaves_no_part_file() {
    let cases = [
        (libc::EEXIST, "; the key holds another object"),
        (
            libc::EBADMSG,
            "; the bytes are not those of the digest the name gives",
        ),
        (libc::EFBIG, "; the object is larger than the store takes"),
        (libc::ENOSPC, ""),
    ];
    for (errno, meaning) in cases {
        let dir = scratch(&format!("refused-{errno}"));
        let out = out_dir(&dir);
        let (sink, store) = served(&out);
        store.refuse("pack", &[errno]);
        let failure = store_one(&sink, b"x").await.expect_err("refused");
        assert_eq!(failure.exit_code(), EXIT_PUT);
        assert_eq!(
            failure.to_string(),
            format!(
                "put: pack: the commit failed on attempt 1 of 4: {}{meaning}",
                io::Error::from_raw_os_error(errno)
            )
        );
        assert_eq!(store.commits(), ["pack"]);
        assert!(names(&out).is_empty(), "{errno}: {:?}", names(&out));
        assert!(store.pauses().is_empty());
    }
}

/// Asserts that a commit answered `EBUSY`, `EIO` or `EINTR` is tried again, the same rename,
/// after each pause in turn, and the run succeeds when the store then takes it.
#[tokio::test]
async fn a_busy_failed_or_interrupted_commit_is_tried_again_after_each_pause() {
    let dir = scratch("retried");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    store.refuse("pack", &[libc::EBUSY, libc::EIO, libc::EINTR]);
    store_one(&sink, b"x")
        .await
        .expect("taken on the fourth attempt");
    assert_eq!(store.commits(), ["pack"; 4]);
    assert_eq!(store.pauses(), PAUSES);
    assert_eq!(names(&out), ["pack"]);
    let renames = store.renames.borrow();
    assert!(renames[1..].iter().all(|r| *r == renames[1]), "{renames:?}");
}

/// Asserts that a commit that keeps failing with `EIO`, or keeps finding the file open
/// (`EBUSY`), stops after the last attempt: exit 13, a line naming attempt 4 of 4 and what the
/// store means by the errno, every pause taken, and no `.part` file left.
#[tokio::test]
async fn a_commit_that_keeps_failing_stops_after_the_last_attempt() {
    let cases = [
        (libc::EIO, "the store did not take it before its deadline"),
        (libc::EBUSY, "the store still sees the file open"),
    ];
    for (errno, meaning) in cases {
        let dir = scratch(&format!("gave-up-{errno}"));
        let out = out_dir(&dir);
        let (sink, store) = served(&out);
        store.refuse("pack", &[errno; COMMIT_ATTEMPTS]);
        let failure = store_one(&sink, b"x").await.expect_err("gave up");
        assert_eq!(failure.exit_code(), EXIT_PUT);
        assert_eq!(
            failure.to_string(),
            format!(
                "put: pack: the commit failed on attempt 4 of 4: {}; {meaning}",
                io::Error::from_raw_os_error(errno)
            )
        );
        assert_eq!(store.commits(), ["pack"; COMMIT_ATTEMPTS]);
        assert_eq!(store.pauses(), PAUSES);
        assert!(names(&out).is_empty(), "{errno}: {:?}", names(&out));
    }
}

/// Asserts that a digest rename the store refuses is exit 13 with its line, no commit is asked
/// for, and the `.part` file is removed.
#[tokio::test]
async fn a_refused_digest_rename_is_exit_13_and_removes_the_part_file() {
    let dir = scratch("digest-rename");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    store.refuse("pack.sha256", &[libc::EPERM]);
    let failure = store_one(&sink, b"x").await.expect_err("refused");
    assert_eq!(failure.exit_code(), EXIT_PUT);
    assert_eq!(
        failure.to_string(),
        format!(
            "put: pack: cannot rename it to the name of its digest: {}",
            io::Error::from_raw_os_error(libc::EPERM)
        )
    );
    assert!(store.commits().is_empty());
    assert!(names(&out).is_empty(), "{:?}", names(&out));
}

/// Asserts that a `.part` name already in the directory is refused (the file is created new,
/// never opened again), exit 13 with its line, and the file that was there is left as it was.
#[tokio::test]
async fn a_part_name_already_there_is_refused_and_left_alone() {
    let dir = scratch("taken");
    let out = out_dir(&dir);
    std::fs::write(out.join("pack.part"), b"theirs").expect("theirs");
    let (sink, store) = served(&out);
    let failure = store_one(&sink, b"x").await.expect_err("refused");
    assert_eq!(failure.exit_code(), EXIT_PUT);
    assert_eq!(
        failure.to_string(),
        format!(
            "put: pack: cannot create it in the put directory: {}",
            io::Error::from_raw_os_error(libc::EEXIST)
        )
    );
    assert_eq!(names(&out), ["pack.part"]);
    assert_eq!(
        std::fs::read(out.join("pack.part")).expect("read"),
        b"theirs"
    );
    assert!(store.renames.borrow().is_empty());
}

/// A file that refuses every write with `ENOSPC`, as a full stage would.
struct Full {
    /// The `.part` file, so that it exists until the run removes it.
    _file: File,
}

impl Write for Full {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Asserts that a write the put directory refuses is exit 13 with a line naming the object, and
/// its `.part` file is removed: a blob bigger than the write buffer.
#[tokio::test]
async fn a_refused_write_is_exit_13_and_removes_the_part_file() {
    let two_mib = vec![6u8; 2 << 20];
    let dir = scratch("full-pack");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    let sink = sink.with_create(|path| {
        let file = File::create_new(path)?;
        Ok(Box::new(Full { _file: file }))
    });
    let failure = store_one(&sink, &two_mib).await.expect_err("refused");
    assert_eq!(failure.exit_code(), EXIT_PUT);
    assert_eq!(
        failure.to_string(),
        format!(
            "put: pack: cannot write it to the put directory: {}",
            io::Error::from_raw_os_error(libc::ENOSPC)
        )
    );
    assert!(store.renames.borrow().is_empty());
    assert!(names(&out).is_empty(), "{:?}", names(&out));
}

/// A file whose writes are taken and whose flush is refused with `ENOSPC`, as a stage that
/// reports a full disk only when the buffered bytes reach it.
struct FullAtFlush {
    file: File,
}

impl Write for FullAtFlush {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }
}

/// Asserts that a full disk the put directory reports only at the flush is exit 13 too, nothing
/// is renamed and no `.part` file is left.
#[tokio::test]
async fn a_refused_flush_is_exit_13_and_nothing_is_committed() {
    let dir = scratch("flush-pack");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    let sink = sink.with_create(|path| {
        let file = File::create_new(path)?;
        Ok(Box::new(FullAtFlush { file }))
    });
    let failure = store_one(&sink, b"small").await.expect_err("refused");
    assert_eq!(failure.exit_code(), EXIT_PUT);
    assert_eq!(
        failure.to_string(),
        format!(
            "put: pack: cannot write it to the put directory: {}",
            io::Error::from_raw_os_error(libc::ENOSPC)
        )
    );
    assert!(store.renames.borrow().is_empty());
    assert!(names(&out).is_empty(), "{:?}", names(&out));
}

/// Asserts that `--out-dir` must be a directory when the task starts: a file and a missing path
/// are each a setup failure (exit 14) with their line, and a symlink to a directory is followed.
#[test]
fn the_out_dir_must_be_a_directory() {
    let dir = scratch("open");
    let file = dir.join("file");
    std::fs::write(&file, b"").expect("file");
    let missing = dir.join("missing");
    let not_dir = OutDir::open(&file).err().expect("a file");
    assert_eq!(not_dir.exit_code(), EXIT_SETUP);
    assert_eq!(
        not_dir.to_string(),
        format!("setup: --out-dir {}: not a directory", file.display())
    );
    let gone = OutDir::open(&missing).err().expect("missing");
    assert_eq!(gone.exit_code(), EXIT_SETUP);
    assert_eq!(
        gone.to_string(),
        format!(
            "setup: --out-dir {}: {}",
            missing.display(),
            io::Error::from_raw_os_error(libc::ENOENT)
        )
    );
    let out = out_dir(&dir);
    let link = dir.join("link");
    std::os::unix::fs::symlink(&out, &link).expect("symlink");
    assert_eq!(OutDir::open(&link).expect("followed").path(), link);
}

/// Asserts the names of the object and of its files, as the frontend mints them.
#[test]
fn the_names_are_the_objects() {
    assert_eq!(OBJECT_NAME, "pack");
    assert_eq!(part_name("pack"), "pack.part");
    let digest = sha256(b"x");
    assert_eq!(
        digest_part_name("pack", &digest),
        format!("pack.sha256-{}.part", hex::encode(digest.hash))
    );
}
