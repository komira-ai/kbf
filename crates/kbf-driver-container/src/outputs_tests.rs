//! The walk's pieces: parent checks, limit flags, blocking tasks, byte limits.

use kbf_daemon::cas::label;

use super::*;

/// A fresh scratch directory for one unit test, beside the test binary.
fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    crate::unit_dir(&format!("outputs-{name}"))
}

/// Catches the walk going back up into a directory other than the one it came
/// from (a parent renamed away and replaced while it was below): `..` must be the
/// directory it left, by device and inode.
#[test]
fn the_walk_goes_back_only_where_it_came_from() {
    let dir = scratch("outputs-parent");
    std::fs::create_dir(dir.join("child")).expect("mkdir");
    let top = rustix::fs::openat(CWD, &*dir, DIRECTORY, Mode::empty()).expect("open");
    let child = rustix::fs::openat(&top, "child", DIRECTORY, Mode::empty()).expect("open");
    let id = identity(&top).expect("identity");
    let back = back_to_parent(&child, id).expect("the same parent");
    assert_eq!(identity(&back).expect("identity"), id);
    let why = back_to_parent(&child, (id.0, id.1 ^ 1)).expect_err("another directory");
    assert!(
        why.to_string().contains("parent directory was replaced"),
        "{why}"
    );
}

/// Catches a limit flag that is missing, misnamed or defaulted to something other
/// than [`OutputLimits::DEFAULT`] in a command line that flattens the limits.
#[test]
fn the_limits_are_flags() {
    #[derive(clap::Parser)]
    struct Line {
        #[command(flatten)]
        limits: OutputLimits,
    }
    let parse = |args: &[&str]| {
        <Line as clap::Parser>::try_parse_from(std::iter::once("kbf").chain(args.iter().copied()))
            .map(|line| line.limits)
    };
    assert_eq!(parse(&[]).expect("defaults"), OutputLimits::default());
    let set = [
        "--output-max-depth=7",
        "--output-max-entries=8",
        "--output-max-bytes=9",
        "--output-max-stdio-bytes=10",
    ];
    let limits = OutputLimits {
        max_depth: 7,
        max_entries: 8,
        max_bytes: 9,
        max_stdio_bytes: 10,
    };
    assert_eq!(parse(&set).expect("set"), limits);
    assert!(parse(&["--output-max-depth=-1"]).is_err());
}

/// Catches the walk's old per-file hand-off to the blocking pool (5.4 s of a 7.8 s
/// walk of 15,189 files): the number of tasks sent to the pool does not grow with
/// the files. A tree of 8 files and one of 800, in the same four directories, send
/// the same number (catches: a `blocking` call per job in `collect`: 800 more).
#[tokio::test]
async fn the_walk_sends_no_task_to_the_blocking_pool_per_file() {
    let mut hops = Vec::new();
    for (name, files) in [("outputs-hops-few", 2), ("outputs-hops-many", 200)] {
        let upper = scratch(name);
        for dir in 0..4 {
            std::fs::create_dir_all(upper.join(format!("out/d{dir}"))).expect("mkdir");
            for n in 0..files {
                std::fs::write(upper.join(format!("out/d{dir}/f{n}")), format!("{dir} {n}"))
                    .expect("write");
            }
        }
        let cas = crate::cas::MemoryCas::new();
        let before = HOPS.with(std::cell::Cell::get);
        let mut result = ActionResult::default();
        collect(
            &cas,
            &upper,
            &CpuPool::new(2),
            "",
            &["out".to_owned()],
            OutputLimits::DEFAULT,
            None,
            &mut result,
        )
        .await
        .expect("collect");
        hops.push(HOPS.with(std::cell::Cell::get) - before);
    }
    assert_eq!(hops[0], hops[1], "tasks sent: {hops:?}");
    assert!(hops[1] <= 3, "tasks sent: {hops:?}");
}

/// Catches the byte limit held per file but not over the files together, and the
/// wrong file named: the files are added up in job order whatever order they were
/// hashed in. Sizes 5 and 4 fit 9; 5, 5 and 1 do not, and it is the second that
/// passes the limit (catches: the running total not checked, which only a race
/// between threads could show in `collect`). A file that did not fit the bytes left
/// is the limit at its own path, and the first failure in job order ends the check
/// before a job no thread reached.
#[test]
fn the_files_together_are_held_to_the_byte_limit_in_job_order() {
    let hashed = |size: u64| -> Option<Outcome> {
        Some(Ok(Some(Digested {
            digest: kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [0; 32], size),
            crc32c: 0,
            executable: false,
            stamp: kbf_daemon::cas::Stamp {
                dev: 0,
                ino: 0,
                size,
                mtime: (0, 0),
                ctime: (0, 0),
            },
            taken: false,
        })))
    };
    let shown: Vec<PathBuf> = ["a", "b", "c"].map(PathBuf::from).to_vec();
    let fits = check_hashed(vec![hashed(5), hashed(4)], 9, &shown).expect("fits");
    assert_eq!(fits.len(), 2);
    let over = check_hashed(vec![hashed(5), hashed(5), hashed(1)], 9, &shown);
    assert!(
        matches!(&over, Err(TreeError::Limit { path, what: Exceeded::Bytes, limit: 9 }) if path == Path::new("b")),
        "{over:?}"
    );
    let none = check_hashed(vec![hashed(5), Some(Ok(None)), None], 9, &shown);
    assert!(
        matches!(&none, Err(TreeError::Limit { path, what: Exceeded::Bytes, .. }) if path == Path::new("b")),
        "{none:?}"
    );
    let failed = check_hashed(
        vec![hashed(5), Some(Err(std::io::Error::other("gone"))), None],
        9,
        &shown,
    );
    assert!(
        matches!(&failed, Err(TreeError::Io { path, .. }) if path == Path::new("b")),
        "{failed:?}"
    );
    // A file no thread hashed, with no failure before it (its worker panicked): an error
    // naming the file, never a digest or a skipped file.
    let lost = check_hashed(vec![hashed(5), None], 9, &shown);
    assert!(
        matches!(&lost, Err(TreeError::Io { path, source }) if path == Path::new("b") && source.to_string().contains("not hashed")),
        "{lost:?}"
    );
}

/// Catches a log that is no regular file being read or hung on, a log past its limit
/// stored, and a log within it lost: a FIFO where the action's stdout should be fails with
/// the reason, a log of 11 bytes against a limit of 10 fails with the stdio limit, and a
/// log of 10 is stored (catches: the type not checked on the descriptor, which would block
/// or read nothing). All three here, so one copy of `collect_log` covers every line.
#[tokio::test]
async fn logs_are_stored_within_their_limit_and_only_if_regular() {
    let dir = scratch("outputs-log");
    let fd = rustix::fs::openat(CWD, &*dir, DIRECTORY, Mode::empty()).expect("open");
    rustix::fs::mknodat(&fd, "stdout", FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).expect("mkfifo");
    let cas = crate::cas::MemoryCas::new();
    let why = collect_log(&cas, &dir.join("stdout"), 10)
        .await
        .expect_err("a FIFO");
    assert!(
        why.to_string().contains("no longer a regular file"),
        "{why}"
    );
    std::fs::write(dir.join("stderr"), b"0123456789a").expect("write");
    let why = collect_log(&cas, &dir.join("stderr"), 10)
        .await
        .expect_err("too long");
    assert!(
        matches!(
            why,
            TreeError::Limit {
                what: Exceeded::Stdio,
                limit: 10,
                ..
            }
        ),
        "{why}"
    );
    std::fs::write(dir.join("stderr"), b"0123456789").expect("write");
    let digest = collect_log(&cas, &dir.join("stderr"), 10)
        .await
        .expect("within the limit");
    assert_eq!(cas.blob(&digest), Some(b"0123456789".to_vec()));
}

/// Catches a blocking task that panicked being lost or taken for success: it is an
/// I/O error against the path being read.
#[tokio::test]
async fn a_blocking_task_that_panicked_is_an_error() {
    let shown = Path::new("shown/path");
    let outcome: Result<(), TreeError> = blocking(shown, || panic!("walk panicked")).await;
    let why = outcome.expect_err("the task panicked").to_string();
    assert!(why.starts_with("shown/path: "), "{why}");
    assert!(why.contains("walk panicked"), "{why}");
    // The same copy of `blocking`, finishing and failing as usual.
    blocking(shown, || Ok(())).await.expect("finished");
    let outcome: Result<(), TreeError> =
        blocking(shown, || Err(std::io::Error::other("refused"))).await;
    let why = outcome.expect_err("the call failed").to_string();
    assert_eq!(why, "shown/path: refused");
}

/// Catches the files an intake took being sent again, or the intake not finished
/// before the messages that name them: outputs collected into a store that takes files
/// as they are read hand every file to the intake once, finish it once, and give
/// `put_records` only the Directory messages and the Tree (catches: taken files kept
/// in the records, the intake never finished). Every blob the result names is held.
#[tokio::test]
async fn files_an_intake_took_are_not_sent_again() {
    use crate::intake_tests::Taker;
    let upper = scratch("outputs-intake");
    std::fs::create_dir_all(upper.join("out/dir")).expect("mkdir");
    std::fs::write(upper.join("out/file"), b"a file").expect("write");
    std::fs::write(upper.join("out/dir/nested"), b"nested").expect("write");
    let taker = Taker::new(false);
    let mut result = ActionResult::default();
    collect(
        &taker,
        &upper,
        &CpuPool::new(2),
        "",
        &["out/file".to_owned(), "out/dir".to_owned()],
        OutputLimits::DEFAULT,
        None,
        &mut result,
    )
    .await
    .expect("collect");
    let file = result.output_files[0].digest.clone().expect("a digest");
    let tree = result.output_directories[0].clone();
    let finished = taker.recording.finished();
    let mut finished: Vec<&String> = finished.iter().map(|(label, _)| label).collect();
    finished.sort();
    let nested = kbf_daemon::cas::digest_of(b"nested");
    let mut expected = [label(&file), label(&nested)];
    expected.sort();
    assert_eq!(finished, [&expected[0], &expected[1]]);
    assert_eq!(
        taker
            .recording
            .finishes
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    let put = taker.put();
    assert!(
        !put.contains(&label(&file)) && !put.contains(&label(&nested)),
        "{put:?}"
    );
    let root = tree.root_directory_digest.expect("a root");
    let tree_digest = tree.tree_digest.expect("a tree");
    assert_eq!(put, [label(&root), label(&tree_digest)]);
    for digest in [&file, &nested, &root, &tree_digest] {
        assert!(
            taker.cas.blob(digest).is_some(),
            "{} not held",
            label(digest)
        );
    }
}

/// Catches a log read twice, or offered to the intake past its limit: into a store that
/// takes files, a log is handed over once, the intake finished, and the digest is the
/// log's, with nothing sent through `put_chunks`; a log past the limit is refused and
/// never offered; a log the intake declines is hashed and sent as before (catches: the
/// intake not asked, a log past the limit offered, the intake not finished). All three
/// here, so one copy of `collect_log` covers them.
#[tokio::test]
async fn a_log_goes_to_the_intake_in_the_read_that_hashes_it() {
    use crate::intake_tests::Taker;
    let dir = scratch("outputs-log-intake");
    std::fs::write(dir.join("stdout"), b"0123456789").expect("write");
    let taker = Taker::new(false);
    let digest = collect_log(&taker, &dir.join("stdout"), 10)
        .await
        .expect("within the limit");
    assert_eq!(digest, kbf_daemon::cas::digest_of(b"0123456789"));
    assert_eq!(taker.recording.finished(), [(label(&digest), vec![10])]);
    assert_eq!(
        taker
            .recording
            .finishes
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(taker.cas.blob(&digest), Some(b"0123456789".to_vec()));
    assert!(taker.put().is_empty(), "{:?}", taker.put());

    std::fs::write(dir.join("stderr"), b"0123456789a").expect("write");
    let why = collect_log(&taker, &dir.join("stderr"), 10)
        .await
        .expect_err("too long");
    assert!(
        matches!(
            why,
            TreeError::Limit {
                what: Exceeded::Stdio,
                limit: 10,
                ..
            }
        ),
        "{why}"
    );
    assert_eq!(
        taker.recording.opened(),
        1,
        "a log past the limit is not offered"
    );

    let declining = Taker::new(true);
    let digest = collect_log(&declining, &dir.join("stdout"), 10)
        .await
        .expect("within the limit");
    assert_eq!(declining.put(), [label(&digest)]);
    assert_eq!(declining.cas.blob(&digest), Some(b"0123456789".to_vec()));
}

/// Catches the Directory messages of the outputs being digested on an async worker (it
/// is hashing, and a large tree has many), or on the blocking pool, which has up to 64
/// threads and no regard for the node's cores: they are digested on the CPU pool, the
/// one pool whose size is the node's budget for hashing. The test's runtime runs on the
/// test's own thread, so it is the async worker here.
#[tokio::test]
async fn the_directory_messages_are_digested_on_the_cpu_pool() {
    let upper = scratch("digest-pool");
    std::fs::create_dir_all(upper.join("out/d")).expect("mkdir");
    std::fs::write(upper.join("out/d/f"), "digest me").expect("write");
    let pool = CpuPool::new(1);
    let pool_thread = pool
        .run(|| std::thread::current().id())
        .await
        .expect("pool thread");
    let worker = std::thread::current().id();
    let mut result = ActionResult::default();
    collect(
        &crate::cas::MemoryCas::new(),
        &upper,
        &pool,
        "",
        &["out".to_owned()],
        OutputLimits::DEFAULT,
        None,
        &mut result,
    )
    .await
    .expect("collect");
    assert_eq!(result.output_directories.len(), 1);
    let ran = ASSEMBLED_ON
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(!ran.contains(&worker), "the digest ran on the async worker");
    assert!(
        ran.contains(&pool_thread),
        "the digest did not run on the CPU pool"
    );
}
