//! The walk's pieces: parent checks, limit flags, blocking tasks, byte limits.

use super::*;

/// A fresh scratch directory for one unit test, beside the test binary.
fn scratch(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-driver-container-unit")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Catches the walk going back up into a directory other than the one it came
/// from (a parent renamed away and replaced while it was below): `..` must be the
/// directory it left, by device and inode.
#[test]
fn the_walk_goes_back_only_where_it_came_from() {
    let dir = scratch("outputs-parent");
    std::fs::create_dir(dir.join("child")).expect("mkdir");
    let top = rustix::fs::openat(CWD, &dir, DIRECTORY, Mode::empty()).expect("open");
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
/// the same number (mutant: a `blocking` call per job in `collect`: 800 more).
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
/// passes the limit (mutant: the running total not checked, which only a race
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
/// log of 10 is stored (mutant: the type not checked on the descriptor, which would block
/// or read nothing). All three here, so one copy of `collect_log` covers every line.
#[tokio::test]
async fn logs_are_stored_within_their_limit_and_only_if_regular() {
    let dir = scratch("outputs-log");
    let fd = rustix::fs::openat(CWD, &dir, DIRECTORY, Mode::empty()).expect("open");
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
