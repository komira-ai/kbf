//! The put directory on a plain directory, with a fake store: the store is a hook on `rename`
//! that records each rename, answers a chosen errno for the ones a test refuses, and renames the
//! rest. What is committed is read back from the directory.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::rc::Rc;

use kbf_segments::{SegmentReader, sha256};
use kbf_task_pack::{
    LayoutError, MAX_RECORD_BYTES, OutputFile, Timing, Usage, read_manifest_with, write_pack_with,
};
use kbf_testkit::dir::TestDir;

use super::*;
use crate::outputs::{OutputLimits, collect_outputs};
use crate::pack::{INVALID_PACK, OUTPUT_CHANGED, OUTPUT_TOO_LARGE};
use crate::task::recording_sink::flip_first_byte;
use crate::task::{EXIT_PACK, EXIT_PUT, EXIT_SETUP};

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

/// A large blob and four small ones, with limits of three records a pack: `L0`, `pack1` and
/// `pack`.
fn three_objects() -> (Vec<u8>, Limits) {
    let big = vec![5u8; usize::try_from(MAX_RECORD_BYTES).expect("fits") + 1];
    let limits = Limits {
        pack_records: 3,
        ..Limits::real()
    };
    (big, limits)
}

/// The four small blobs of [`three_objects`].
const SMALL: [&[u8]; 4] = [b"one", b"two", b"three", b"four"];

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

/// The digests of the records of `pack`, in offset order.
fn records(pack: &[u8]) -> Vec<Digest> {
    let reader = SegmentReader::open(pack).expect("segment");
    let mut entries = reader.footer().entries().to_vec();
    entries.sort_by_key(|e| e.offset);
    entries.iter().map(|e| e.digest).collect()
}

/// Asserts the whole of a run of one pack: the directory ends holding `pack` alone, committed by
/// the digest rename then the commit rename; its bytes are those of the in-memory writer for the
/// same manifest and blobs; the manifest read back is the one returned, the Command's exit code
/// 3 in it.
#[test]
fn a_run_of_one_pack_commits_pack_alone_byte_equal_to_the_in_memory_writer() {
    let dir = scratch("one-pack");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    let (alpha, beta): (&[u8], &[u8]) = (b"alpha", b"beta");
    let blobs = Blobs::from_bytes(&[alpha, beta]);
    let mut manifest = manifest(&[alpha, beta]);
    sink.store(&mut manifest, &blobs, &Limits::real())
        .expect("store");

    assert_eq!(names(&out), ["pack"]);
    let pack = committed(&out, &store, "pack");
    assert_eq!(store.commits(), ["pack"]);
    let read = read_manifest_with(&pack, MAX_LARGE_BYTES).expect("read");
    assert_eq!(read, manifest);
    assert_eq!((read.exit_code, read.packs), (3, 1));
    let mut sorted = [alpha, beta];
    sorted.sort_by_key(|b| sha256(b));
    let expected = write_pack_with(&manifest, &sorted, MAX_LARGE_BYTES).expect("in memory");
    assert_eq!(pack, expected);
    assert!(store.pauses().is_empty());
}

/// Asserts that the committed manifest carries the four phase timings, and today's three: `walk`
/// is the collect time the run measured (`pack_ms` on entry), the others are present (their
/// values are not asserted), and `fetch_ms` and `run_ms` are unchanged.
#[test]
fn the_committed_manifest_carries_the_phase_timings() {
    let dir = scratch("phases");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    let blobs = Blobs::from_bytes(&[b"x"]);
    let mut manifest = manifest(&[b"x"]);
    sink.store(&mut manifest, &blobs, &Limits::real())
        .expect("store");
    let timing = read_manifest_with(&committed(&out, &store, "pack"), MAX_LARGE_BYTES)
        .expect("read")
        .timing;
    assert_eq!(timing.walk_ms, Some(7));
    assert!(timing.hash_ms.is_some());
    assert!(timing.write_ms.is_some());
    assert!(timing.commit_ms.is_some());
    assert_eq!((timing.fetch_ms, timing.run_ms), (1, 2));
    // `pack_ms` is rewritten to cover the four phases; a relation, not a duration.
    let phases = [
        timing.walk_ms,
        timing.hash_ms,
        timing.write_ms,
        timing.commit_ms,
    ];
    let sum: u64 = phases.iter().flatten().sum();
    assert!(timing.pack_ms >= sum, "{timing:?}");
}

/// Asserts the order and contents of a run of two packs and a large object: the large object is
/// written and committed first as `L0` with the blob's bytes, then `pack1`, then `pack` last; the
/// manifest says two packs and the large slot; the two packs hold the small blobs, each once,
/// and pack 1 has no manifest.
#[test]
fn a_large_object_then_pack1_then_pack_are_committed_in_that_order() {
    let dir = scratch("two-packs-and-large");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    let big = vec![5u8; usize::try_from(MAX_RECORD_BYTES).expect("fits") + 1];
    let small: [&[u8]; 4] = [b"one", b"two", b"three", b"four"];
    let all: Vec<&[u8]> = small.iter().copied().chain([big.as_slice()]).collect();
    let blobs = Blobs::from_bytes(&all);
    let mut manifest = manifest(&all);
    // Three records a pack: the manifest and four blobs need two.
    let limits = Limits {
        pack_records: 3,
        ..Limits::real()
    };
    sink.store(&mut manifest, &blobs, &limits).expect("store");

    assert_eq!(store.commits(), ["L0", "pack1", "pack"]);
    assert_eq!(names(&out), ["L0", "pack", "pack1"]);
    assert_eq!(committed(&out, &store, "L0"), big);
    let pack = committed(&out, &store, "pack");
    let pack1 = committed(&out, &store, "pack1");
    let read = read_manifest_with(&pack, MAX_LARGE_BYTES).expect("read");
    assert_eq!(read, manifest);
    assert_eq!(read.packs, 2);
    assert_eq!(read.large.len(), 1);
    assert_eq!(
        (read.large[0].slot, read.large[0].digest),
        (0, sha256(&big))
    );
    let json = sha256(&manifest.to_json_with(MAX_LARGE_BYTES).expect("json"));
    let in_pack = records(&pack);
    assert_eq!(in_pack[0], json);
    let mut held: Vec<Digest> = in_pack[1..]
        .iter()
        .chain(&records(&pack1))
        .copied()
        .collect();
    held.sort();
    let mut expected: Vec<Digest> = small.iter().map(|b| sha256(b)).collect();
    expected.sort();
    assert_eq!(held, expected);
    assert!(!records(&pack1).contains(&json));
}

/// Asserts that a commit the store refuses for good fails the run with exit 13 and a line naming
/// the object, the attempt, the errno and what the store means by it, with exact text, is not
/// tried again, and leaves no `.part` file: one case per errno of the store's mapping that is
/// final, and one it does not name.
#[test]
fn a_refused_commit_is_exit_13_with_its_errno_and_leaves_no_part_file() {
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
        let failure = sink
            .store(
                &mut manifest(&[b"x"]),
                &Blobs::from_bytes(&[b"x"]),
                &Limits::real(),
            )
            .expect_err("refused");
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
#[test]
fn a_busy_failed_or_interrupted_commit_is_tried_again_after_each_pause() {
    let dir = scratch("retried");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    store.refuse("pack", &[libc::EBUSY, libc::EIO, libc::EINTR]);
    sink.store(
        &mut manifest(&[b"x"]),
        &Blobs::from_bytes(&[b"x"]),
        &Limits::real(),
    )
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
#[test]
fn a_commit_that_keeps_failing_stops_after_the_last_attempt() {
    let cases = [
        (libc::EIO, "the store did not take it before its deadline"),
        (libc::EBUSY, "the store still sees the file open"),
    ];
    for (errno, meaning) in cases {
        let dir = scratch(&format!("gave-up-{errno}"));
        let out = out_dir(&dir);
        let (sink, store) = served(&out);
        store.refuse("pack", &[errno; COMMIT_ATTEMPTS]);
        let failure = sink
            .store(
                &mut manifest(&[b"x"]),
                &Blobs::from_bytes(&[b"x"]),
                &Limits::real(),
            )
            .expect_err("gave up");
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

/// Asserts that a run whose last commit fails keeps the objects it committed before it (they are
/// the store's now) and removes the `.part` file of the one that failed.
#[test]
fn a_failed_commit_keeps_the_objects_committed_before_it() {
    let dir = scratch("kept");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    store.refuse("pack", &[libc::EEXIST]);
    let small: [&[u8]; 4] = [b"one", b"two", b"three", b"four"];
    let limits = Limits {
        pack_records: 3,
        ..Limits::real()
    };
    let failure = sink
        .store(&mut manifest(&small), &Blobs::from_bytes(&small), &limits)
        .expect_err("refused");
    assert_eq!(failure.exit_code(), EXIT_PUT);
    assert_eq!(store.commits(), ["pack1", "pack"]);
    assert_eq!(names(&out), ["pack1"]);
}

/// Asserts that a digest rename the store refuses is exit 13 with its line, no commit is asked
/// for, and the `.part` file is removed.
#[test]
fn a_refused_digest_rename_is_exit_13_and_removes_the_part_file() {
    let dir = scratch("digest-rename");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    store.refuse("pack.sha256", &[libc::EPERM]);
    let failure = sink
        .store(
            &mut manifest(&[b"x"]),
            &Blobs::from_bytes(&[b"x"]),
            &Limits::real(),
        )
        .expect_err("refused");
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
#[test]
fn a_part_name_already_there_is_refused_and_left_alone() {
    let dir = scratch("taken");
    let out = out_dir(&dir);
    std::fs::write(out.join("pack.part"), b"theirs").expect("theirs");
    let (sink, store) = served(&out);
    let failure = sink
        .store(
            &mut manifest(&[b"x"]),
            &Blobs::from_bytes(&[b"x"]),
            &Limits::real(),
        )
        .expect_err("refused");
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
/// its `.part` file is removed: a large object (streamed from its blob) and a pack bigger than
/// the write buffer (refused while the pack is written).
#[test]
fn a_refused_write_is_exit_13_and_removes_the_part_file() {
    let big = vec![5u8; usize::try_from(MAX_RECORD_BYTES).expect("fits") + 1];
    let two_mib = vec![6u8; 2 << 20];
    let cases: [(&str, &[u8]); 2] = [("L0", &big), ("pack", &two_mib)];
    for (name, blob) in cases {
        let dir = scratch(&format!("full-{name}"));
        let out = out_dir(&dir);
        let (sink, store) = served(&out);
        let sink = sink.with_create(|path| {
            let file = File::create_new(path)?;
            Ok(Box::new(Full { _file: file }))
        });
        let failure = sink
            .store(
                &mut manifest(&[blob]),
                &Blobs::from_bytes(&[blob]),
                &Limits::real(),
            )
            .expect_err("refused");
        assert_eq!(failure.exit_code(), EXIT_PUT);
        assert_eq!(
            failure.to_string(),
            format!(
                "put: {name}: cannot write it to the put directory: {}",
                io::Error::from_raw_os_error(libc::ENOSPC)
            )
        );
        assert!(store.renames.borrow().is_empty());
        assert!(names(&out).is_empty(), "{name}: {:?}", names(&out));
    }
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
/// is renamed and no `.part` file is left: a pack smaller than the write buffer, and a large
/// object.
#[test]
fn a_refused_flush_is_exit_13_and_nothing_is_committed() {
    let (big, _) = three_objects();
    let cases: [(&str, &[u8]); 2] = [("pack", b"small"), ("L0", &big)];
    for (name, blob) in cases {
        let dir = scratch(&format!("flush-{name}"));
        let out = out_dir(&dir);
        let (sink, store) = served(&out);
        let sink = sink.with_create(|path| {
            let file = File::create_new(path)?;
            Ok(Box::new(FullAtFlush { file }))
        });
        let failure = sink
            .store(
                &mut manifest(&[blob]),
                &Blobs::from_bytes(&[blob]),
                &Limits::real(),
            )
            .expect_err("refused");
        assert_eq!(failure.exit_code(), EXIT_PUT);
        assert_eq!(
            failure.to_string(),
            format!(
                "put: {name}: cannot write it to the put directory: {}",
                io::Error::from_raw_os_error(libc::ENOSPC)
            )
        );
        assert!(store.renames.borrow().is_empty(), "{name}");
        assert!(names(&out).is_empty(), "{name}: {:?}", names(&out));
    }
}

/// A case of a run that fails part way: its name, the commit refused, the `.part` whose writes
/// are refused, and the names that stay.
type PartWay = (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    &'static [&'static str],
);

/// Asserts the clean-up of a run of three objects (`L0`, `pack1`, `pack`) that fails part way:
/// every `.part` file it wrote and did not commit is removed, whichever object failed, and only
/// what it committed stays. The cases: the write of `pack1` refused (`L0.part` written before
/// it), the commit of `L0` refused (`pack1.part` written and waiting), and the commit of `pack1`
/// refused (`L0` committed before it).
#[test]
fn a_run_that_fails_part_way_removes_every_part_file_and_keeps_what_it_committed() {
    let (big, limits) = three_objects();
    let all: Vec<&[u8]> = SMALL.iter().copied().chain([big.as_slice()]).collect();
    // (case, the commit refused or none, the `.part` whose writes are refused or none, what stays)
    let cases: [PartWay; 3] = [
        ("write-pack1", None, Some("pack1.part"), &[]),
        ("commit-l0", Some("L0"), None, &[]),
        ("commit-pack1", Some("pack1"), None, &["L0"]),
    ];
    for (case, commit, write, stays) in cases {
        let dir = scratch(&format!("part-way-{case}"));
        let out = out_dir(&dir);
        let (mut sink, store) = served(&out);
        if let Some(name) = commit {
            store.refuse(name, &[libc::EEXIST]);
        }
        if let Some(part) = write {
            sink = sink.with_create(move |path| {
                let file = File::create_new(path)?;
                if path.ends_with(part) {
                    Ok(Box::new(Full { _file: file }))
                } else {
                    Ok(Box::new(file))
                }
            });
        }
        let failure = sink
            .store(&mut manifest(&all), &Blobs::from_bytes(&all), &limits)
            .expect_err(case);
        assert_eq!(failure.exit_code(), EXIT_PUT, "{case}");
        assert_eq!(names(&out), stays, "{case}");
    }
}

/// Outputs collected from files under `dir`: `big` (held open, being over one chunk) and `small`.
async fn file_blobs(dir: &Path, big: &[u8]) -> (TaskManifest, Blobs) {
    let work = dir.join("work");
    std::fs::create_dir(&work).expect("work");
    std::fs::write(work.join("big"), big).expect("big");
    std::fs::write(work.join("small"), b"small").expect("small");
    std::fs::write(dir.join("stdout"), b"").expect("stdout");
    std::fs::write(dir.join("stderr"), b"").expect("stderr");
    let paths = ["big".to_owned(), "small".to_owned()];
    let collected = collect_outputs(
        &work,
        "",
        &paths,
        &dir.join("stdout"),
        &dir.join("stderr"),
        OutputLimits::default(),
        u64::MAX,
    )
    .await
    .expect("collect");
    let mut manifest = manifest(&[big, b"small"]);
    manifest.output_files[0].path = "big".to_owned();
    manifest.output_files[1].path = "small".to_owned();
    (manifest, collected.blobs)
}

/// Asserts that an output whose bytes change after they were hashed is exit 12
/// (`output-changed`), whenever the change is seen, and leaves no `.part` file: changed before
/// the run is stored (seen while its checksum is taken, before anything is written), changed
/// while pack 0 is being written (seen by the pack's write), and a large object changed, or cut
/// to one byte, before it is written (seen once it is written).
#[tokio::test]
async fn an_output_changed_after_it_was_hashed_is_exit_12() {
    let chunk = vec![7u8; kbf_outputs::CHUNK_BYTES + 1];
    let big = vec![8u8; usize::try_from(MAX_RECORD_BYTES).expect("fits") + 1];
    // (case, the big output, the `.part` file whose create changes it, none: changed before;
    // whether it is cut short rather than changed in place).
    let cases: [(&str, &[u8], Option<&str>, bool); 4] = [
        ("before", &chunk, None, false),
        ("pack", &chunk, Some("pack.part"), false),
        ("large", &big, Some("L0.part"), false),
        ("large-cut", &big, Some("L0.part"), true),
    ];
    for (case, bytes, when, cut) in cases {
        let dir = scratch(&format!("changed-{case}"));
        let out = out_dir(&dir);
        let (mut manifest, blobs) = file_blobs(&dir, bytes).await;
        let path = dir.join("work/big");
        let (sink, store) = served(&out);
        let sink = match when {
            None => {
                flip_first_byte(&path);
                sink
            }
            Some(at) => {
                let at = at.to_owned();
                sink.with_create(move |part| {
                    if part.ends_with(&at) && cut {
                        File::options().write(true).open(&path)?.set_len(1)?;
                    } else if part.ends_with(&at) {
                        flip_first_byte(&path);
                    }
                    Ok(Box::new(File::create_new(part)?))
                })
            }
        };
        let failure = sink
            .store(&mut manifest, &blobs, &Limits::real())
            .expect_err("changed");
        assert_eq!(failure.exit_code(), EXIT_PACK, "{case}");
        let Failure::Pack(e) = &failure else {
            panic!("{case}: {failure}")
        };
        assert_eq!(e.detail, OUTPUT_CHANGED, "{case}");
        assert!(store.commits().is_empty(), "{case}");
        assert!(names(&out).is_empty(), "{case}: {:?}", names(&out));
    }
}

/// Asserts that outputs that do not fit the run's slots are refused (exit 12,
/// `output-too-large`) before anything is written: nothing is created or renamed.
#[test]
fn outputs_that_do_not_fit_are_refused_before_anything_is_written() {
    let dir = scratch("too-large");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    let small: [&[u8]; 4] = [b"one", b"two", b"three", b"four"];
    let limits = Limits {
        pack_records: 3,
        packs: 1,
        ..Limits::real()
    };
    let failure = sink
        .store(&mut manifest(&small), &Blobs::from_bytes(&small), &limits)
        .expect_err("too large");
    let Failure::Pack(e) = &failure else {
        panic!("{failure}")
    };
    assert_eq!(e.detail, OUTPUT_TOO_LARGE);
    assert!(names(&out).is_empty());
    assert!(store.renames.borrow().is_empty());
}

/// Asserts that a manifest the frontend would refuse (a lease that is not a build id) is
/// refused before anything is written.
#[test]
fn a_manifest_that_would_be_refused_is_refused_before_anything_is_written() {
    let dir = scratch("bad-manifest");
    let out = out_dir(&dir);
    let (sink, store) = served(&out);
    let mut manifest = manifest(&[b"x"]);
    manifest.lease = "not/a/build".to_owned();
    let failure = sink
        .store(&mut manifest, &Blobs::from_bytes(&[b"x"]), &Limits::real())
        .expect_err("refused");
    assert_eq!(failure.exit_code(), EXIT_PACK);
    assert!(names(&out).is_empty());
    assert!(store.renames.borrow().is_empty());
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

/// Asserts the names of the slots and of their files, as the frontend mints them.
#[test]
fn the_names_are_the_slots() {
    assert_eq!(pack_name(0), "pack");
    assert_eq!(pack_name(7), "pack7");
    assert_eq!(large_name(0), "L0");
    assert_eq!(part_name("L3"), "L3.part");
    let digest = sha256(b"x");
    assert_eq!(
        digest_part_name("pack", &digest),
        format!("pack.sha256-{}.part", hex::encode(digest.hash))
    );
}

/// Asserts how a layout refusal reads as a pack refusal: a cap passed is `output-too-large`, a
/// pack of no record is `invalid-pack`, each with the layout's own text.
#[test]
fn a_layout_refusal_is_a_pack_refusal_with_its_detail() {
    let cases = [
        (LayoutError::NoRecords, INVALID_PACK),
        (LayoutError::RecordTooLarge { size: 1 }, OUTPUT_TOO_LARGE),
        (LayoutError::TooManyRecords { count: 1 }, OUTPUT_TOO_LARGE),
        (LayoutError::TooLarge { len: 1 }, OUTPUT_TOO_LARGE),
    ];
    for (layout, detail) in cases {
        let text = layout.to_string();
        let pack = PackError::from(layout);
        assert_eq!(pack.detail, detail);
        assert_eq!(pack.to_string(), format!("{detail}: {text}"));
    }
}
