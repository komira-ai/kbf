//! The durable log file (`kbf_server::log_file`): what it keeps across restarts, how it
//! rotates and what it bounds, and that a file that cannot be written never costs the
//! stderr copy of an event.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_server::log_file::{FILE_NAME, LogFile, RETRY_BYTES, subscriber};
use tracing_subscriber::fmt::MakeWriter;

/// A fresh, absent directory for `name`, under the target directory.
fn fresh_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-server-log-file")
        .join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// The reports a log file made (what it would have told stderr).
#[derive(Clone, Default)]
struct Reports(Arc<Mutex<Vec<String>>>);

impl Reports {
    fn sink(&self) -> impl Fn(&str) + Send + 'static {
        let reports = Arc::clone(&self.0);
        move |line| {
            reports
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(line.to_owned());
        }
    }

    fn all(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// `log` opened over `dir`.
fn open(dir: &Path, max_bytes: u64, files: u32) -> LogFile {
    LogFile::open_reporting(dir, max_bytes, files, |_| {})
}

/// Writes each of `lines` (plus a newline) as one event.
fn write(log: &LogFile, lines: impl IntoIterator<Item = String>) {
    for line in lines {
        let mut w = log.make_writer();
        w.write_all(format!("{line}\n").as_bytes())
            .expect("a write");
        w.flush().expect("a flush");
    }
}

/// `line 0007`-style lines, ten bytes each with the newline.
fn numbered(range: std::ops::Range<u32>) -> Vec<String> {
    range.map(|n| format!("line {n:04}")).collect()
}

/// The log files in `dir`, newest first: `kbf-server.log`, `.1`, `.2`, ... with their
/// lengths.
fn files(dir: &Path) -> Vec<(String, u64)> {
    let mut found: Vec<(u32, String, u64)> = fs::read_dir(dir)
        .expect("the log directory")
        .map(|e| e.expect("an entry"))
        .map(|e| {
            let name = e.file_name().into_string().expect("UTF-8 name");
            let suffix = name.strip_prefix(FILE_NAME).unwrap_or("?");
            let n = match suffix.strip_prefix('.') {
                None => {
                    assert_eq!(suffix, "", "{name} is not a log file");
                    0
                }
                Some(n) => n.parse().expect("a file number"),
            };
            (n, name, e.metadata().expect("metadata").len())
        })
        .collect();
    found.sort();
    found
        .into_iter()
        .map(|(_, name, len)| (name, len))
        .collect()
}

/// Every line kept, oldest first: the oldest file first, the current one last.
fn read_all(dir: &Path) -> Vec<String> {
    files(dir)
        .iter()
        .rev()
        .flat_map(|(name, _)| {
            fs::read_to_string(dir.join(name))
                .expect("a log file")
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Catches: a log that is truncated when the server starts again (the entries before
/// a restart, the ones that explain a crash, gone), or that restarts its numbering of
/// rotated files so the order across files is lost: lines written, the writer dropped,
/// the directory reopened and more written must all be there in order across files.
#[test]
fn a_restart_keeps_the_entries_before_it() {
    let dir = fresh_dir("restart");
    // 40 bytes: four lines to a file, so the 20 lines span files on both sides.
    let log = open(&dir, 40, 8);
    write(&log, numbered(0..10));
    drop(log);
    assert!(files(&dir).len() > 1, "the first run rotated");

    let log = open(&dir, 40, 8);
    write(&log, numbered(10..20));
    assert_eq!(read_all(&dir), numbered(0..20));
    assert_eq!(files(&dir).len(), 5, "{:?}", files(&dir));
}

/// Catches: a cap that is not kept: a file over `max-bytes`, more files than
/// `--log-files`, the oldest lines kept over the newest, or a gap in the lines that
/// remain (a rotation that loses a file in the middle). 100 lines of 10 bytes go
/// through 50-byte files, three of them.
#[test]
fn the_cap_is_enforced_and_the_newest_lines_survive() {
    let dir = fresh_dir("cap");
    let log = open(&dir, 50, 3);
    write(&log, numbered(0..100));

    let found = files(&dir);
    let names: Vec<&str> = found.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, [FILE_NAME, "kbf-server.log.1", "kbf-server.log.2"]);
    for (name, len) in &found {
        assert!(*len <= 50, "{name} holds {len} bytes");
    }
    let kept = read_all(&dir);
    let first = 100 - u32::try_from(kept.len()).expect("a count");
    assert_eq!(kept, numbered(first..100), "contiguous, to the end");
    assert_eq!(kept.len(), 15, "three full files");
    assert!(
        !kept.contains(&"line 0000".to_owned()),
        "the oldest is dropped"
    );
}

/// Catches: a line longer than the cap that is split, dropped, or written into a file
/// that already holds lines (the cap is then exceeded by more than the one line), and
/// the line after it landing in the oversize file.
#[test]
fn a_line_over_the_cap_is_written_alone_to_a_fresh_file() {
    let dir = fresh_dir("oversize");
    let log = open(&dir, 50, 4);
    let big = "x".repeat(120);
    write(
        &log,
        ["a".to_owned(), big.clone(), "b".to_owned(), "c".to_owned()],
    );

    let found = files(&dir);
    let over: Vec<&(String, u64)> = found.iter().filter(|(_, len)| *len > 50).collect();
    assert_eq!(over.len(), 1, "{found:?}");
    let alone = fs::read_to_string(dir.join(&over[0].0)).expect("the oversize file");
    assert_eq!(alone, format!("{big}\n"));
    assert_eq!(read_all(&dir), ["a", big.as_str(), "b", "c"]);
}

/// Catches: `--log-files 1` keeping the rotated file too (two files on disk), or
/// failing to rotate at all (the one file growing past the cap).
#[test]
fn one_file_is_started_again_at_the_cap() {
    let dir = fresh_dir("one-file");
    let log = open(&dir, 50, 1);
    write(&log, numbered(0..12));

    let found = files(&dir);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].1 <= 50, "{found:?}");
    assert_eq!(read_all(&dir), numbered(10..12));
}

/// Catches: files created readable by everyone (the log names operations, nodes and
/// action digests), and a log directory made with the process's default mode.
#[test]
fn the_files_are_private_to_the_server_user_and_its_group() {
    let dir = fresh_dir("modes");
    let log = open(&dir.join("nested").join("log"), 50, 2);
    write(&log, numbered(0..8));

    let mode = |p: &Path| fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
    let log_dir = dir.join("nested").join("log");
    assert_eq!(mode(&log_dir), 0o750);
    for (name, _) in files(&log_dir) {
        assert_eq!(mode(&log_dir.join(&name)), 0o640, "{name}");
    }
}

/// Catches: files left over from a run that kept more (`--log-files` lowered) staying
/// on disk forever past the cap; and a sweep that removes files it did not write.
#[test]
fn files_past_the_count_are_removed_at_start_and_others_are_left() {
    let dir = fresh_dir("past-count");
    fs::create_dir_all(&dir).expect("dir");
    for name in [
        "kbf-server.log.2",
        "kbf-server.log.3",
        "kbf-server.log.9",
        "kbf-server.log.x",
        "other.log.7",
    ] {
        fs::write(dir.join(name), "old\n").expect("a file");
    }
    let _log = open(&dir, 50, 3);

    let mut left: Vec<String> = fs::read_dir(&dir)
        .expect("dir")
        .map(|e| e.expect("entry").file_name().into_string().expect("UTF-8"))
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            "kbf-server.log",
            "kbf-server.log.2",
            "kbf-server.log.x",
            "other.log.7"
        ]
    );
}

/// Catches: a log directory that cannot be made stopping the server (the open panics
/// or returns an error), a failure reported on every event (stderr flooded), and a file
/// that never tries again: after [`RETRY_BYTES`] more of events the next write opens
/// the file and it works again, even under a cap far larger than that (a retry tied to
/// `--log-max-bytes` would drop a whole file's worth, hours of logs).
#[test]
fn a_file_that_cannot_be_opened_is_reported_once_and_tried_again_after_a_small_window() {
    let dir = fresh_dir("blocked");
    fs::create_dir_all(&dir).expect("dir");
    // A directory where the current file goes: opening it for append fails.
    fs::create_dir(dir.join(FILE_NAME)).expect("blocker");
    let reports = Reports::default();
    let log = LogFile::open_reporting(&dir, 1 << 30, 3, reports.sink());
    assert_eq!(reports.all().len(), 1, "{:?}", reports.all());
    assert!(reports.all()[0].contains(FILE_NAME), "{:?}", reports.all());

    // 1 KiB lines: 63 are dropped, the 64th reaches the window, so the file is made and
    // holds that line and the 36 after it.
    let line = |n: u32| format!("{n:04}{}", "x".repeat(1019));
    assert_eq!(RETRY_BYTES, 64 * 1024);
    write(&log, (0..100).map(line));
    assert_eq!(reports.all().len(), 1, "reported once: {:?}", reports.all());
    let kept = fs::read_to_string(dir.join(FILE_NAME)).expect("the new file");
    let want: String = (63..100).map(|n| format!("{}\n", line(n))).collect();
    assert_eq!(kept, want);
}

/// Catches: the plain `LogFile::open` (what the binary uses) losing its failure report:
/// it reports to stderr, where journald keeps it, and the server goes on. A directory
/// blocks the current file, so the open fails and the default reporter runs.
#[test]
fn the_default_open_reports_a_failure_and_does_not_panic() {
    let dir = fresh_dir("blocked-default");
    fs::create_dir_all(dir.join(FILE_NAME)).expect("blocker");
    let log = LogFile::open(&dir, 1 << 30, 3);
    write(&log, ["dropped".to_owned()]);
    assert!(
        dir.join(FILE_NAME).is_dir(),
        "the blocker is left as it was"
    );
}

/// Catches: a second failure while the file is already failing being reported again
/// (stderr flooded while a disk stays broken). With one file kept, a retry removes the
/// current file instead of renaming it; a directory there cannot be removed that way,
/// so the retry fails as the open did, and nothing more is reported.
#[test]
fn a_retry_that_fails_again_is_not_reported_again() {
    let dir = fresh_dir("blocked-one");
    fs::create_dir_all(&dir).expect("dir");
    fs::create_dir(dir.join(FILE_NAME)).expect("blocker");
    let reports = Reports::default();
    let log = LogFile::open_reporting(&dir, 1 << 30, 1, reports.sink());
    let line = |n: u32| format!("{n:04}{}", "x".repeat(1019));
    write(&log, (0..200).map(line));
    assert_eq!(reports.all().len(), 1, "reported once: {:?}", reports.all());
    assert!(
        dir.join(FILE_NAME).is_dir(),
        "the blocker is left as it was"
    );
}

/// Catches: a rotation that fails, or loses the new line, when the current file was
/// removed by hand while the server ran (no current file to rename to `.1`).
#[test]
fn a_current_file_removed_by_hand_is_started_again_at_the_next_rotation() {
    let dir = fresh_dir("removed");
    let log = open(&dir, 100, 3);
    write(&log, ["first".to_owned()]);
    fs::remove_file(dir.join(FILE_NAME)).expect("remove the current file");
    write(&log, ["x".repeat(120)]);
    assert_eq!(
        fs::read_to_string(dir.join(FILE_NAME)).expect("current"),
        format!("{}\n", "x".repeat(120))
    );
    assert!(
        !dir.join(format!("{FILE_NAME}.1")).exists(),
        "nothing to shift"
    );
}

/// Catches: a rotation error other than a missing file (here a directory where `.2`
/// goes) being ignored as if the file were absent, so `.1` would be overwritten; it is
/// reported, and the server goes on.
#[test]
fn a_rotation_blocked_by_a_directory_is_reported_not_ignored() {
    let dir = fresh_dir("rot-blocked");
    fs::create_dir_all(dir.join(format!("{FILE_NAME}.2")).join("inside")).expect("blocker");
    fs::write(dir.join(format!("{FILE_NAME}.1")), "older\n").expect(".1");
    let reports = Reports::default();
    let log = LogFile::open_reporting(&dir, 100, 3, reports.sink());
    write(&log, ["x".repeat(60), "y".repeat(60)]);
    assert_eq!(reports.all().len(), 1, "{:?}", reports.all());
    assert_eq!(
        fs::read_to_string(dir.join(format!("{FILE_NAME}.1"))).expect(".1"),
        "older\n"
    );
}

/// Catches: a write error (disk full) that panics, or stops later events from being
/// written once the disk has room; and one that is reported on every line.
/// `/dev/full` fails every write with ENOSPC.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_write_is_reported_once_and_does_not_panic() {
    let dir = fresh_dir("full");
    fs::create_dir_all(&dir).expect("dir");
    std::os::unix::fs::symlink("/dev/full", dir.join(FILE_NAME)).expect("symlink");
    let reports = Reports::default();
    let log = LogFile::open_reporting(&dir, 1000, 3, reports.sink());

    write(&log, numbered(0..5));
    let all = reports.all();
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(all[0].contains("No space left"), "{all:?}");
}

/// A `MakeWriter` into a shared buffer, standing for stderr.
#[derive(Clone, Default)]
struct Stderr(Arc<Mutex<Vec<u8>>>);

struct StderrWriter(Stderr);

impl Write for StderrWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (self.0)
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Stderr {
    type Writer = StderrWriter;

    fn make_writer(&'a self) -> StderrWriter {
        StderrWriter(self.clone())
    }
}

impl Stderr {
    fn text(&self) -> String {
        StderrWriter(self.clone()).flush().expect("a flush");
        String::from_utf8(
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
        .expect("UTF-8")
    }
}

/// `text` without ANSI colour sequences, each line without its leading timestamp.
fn plain(text: &str) -> Vec<String> {
    let mut stripped = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for skipped in chars.by_ref() {
                if skipped == 'm' {
                    break;
                }
            }
        } else {
            stripped.push(c);
        }
    }
    stripped
        .lines()
        .map(|l| {
            let rest = l.split_once(' ').map_or(l, |(_, rest)| rest);
            rest.trim_start().to_owned()
        })
        .collect()
}

/// Catches: the file layer and the stderr layer formatting an event differently (the
/// file must read like journald's copy), debug events reaching either, and a file that
/// is broken costing stderr its copy of the event.
#[test]
fn both_logs_get_the_same_events_and_a_broken_file_never_costs_stderr_its_copy() {
    let dir = fresh_dir("both");
    let stderr = Stderr::default();
    let good = open(&dir, 10_000, 2);
    tracing::subscriber::with_default(subscriber(stderr.clone(), Some(good)), || {
        tracing::info!(operation = "op-1", lease = "1.2", "the log file test event");
        tracing::debug!("never written");
    });
    let file = fs::read_to_string(dir.join(FILE_NAME)).expect("the log");
    assert_eq!(plain(&file), plain(&stderr.text()));
    assert_eq!(plain(&file).len(), 1, "{file}");
    assert!(
        plain(&file)[0].starts_with("INFO log_file: the log file test event"),
        "{file}"
    );
    assert!(file.contains("operation=\"op-1\""), "{file}");

    // A file whose directory cannot be made: its parent is a regular file.
    let blocked = fresh_dir("both-blocked");
    fs::create_dir_all(&blocked).expect("dir");
    fs::write(blocked.join("plain"), "x").expect("file");
    let reports = Reports::default();
    let broken =
        LogFile::open_reporting(&blocked.join("plain").join("log"), 1000, 2, reports.sink());
    let stderr = Stderr::default();
    tracing::subscriber::with_default(subscriber(stderr.clone(), Some(broken)), || {
        tracing::info!("an event while the file is broken");
    });
    assert!(
        stderr.text().contains("an event while the file is broken"),
        "{}",
        stderr.text()
    );
    assert_eq!(reports.all().len(), 1, "{:?}", reports.all());
}
