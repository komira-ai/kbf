use kbf_testkit::dir::TestDir;
use std::fs;

use super::*;

/// A fresh directory for `name`.
fn fresh(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-deploy-log-{name}"))
}

fn lines(dir: &Path, count: usize) -> Vec<String> {
    tail(dir, count)
        .expect("a tail")
        .into_iter()
        .map(|l| String::from_utf8(l).expect("UTF-8"))
        .collect()
}

/// Three files as the server leaves them: `.2` the oldest, the current one the newest.
fn rotated(dir: &Path) {
    fs::write(dir.join("kbf-server.log.2"), "l1\nl2\nl3\n").expect("file");
    fs::write(dir.join("kbf-server.log.1"), "l4\nl5\n").expect("file");
    fs::write(dir.join("kbf-server.log"), "l6\nl7\n").expect("file");
    fs::write(dir.join("unrelated.log"), "zzz\n").expect("file");
}

/// Catches: a tail that reads the rotated files in the wrong order (the lines of `.1`
/// before `.2`'s, or the current file first), one that stops at the current file when
/// it holds fewer lines than asked, one that returns more than `count`, and one that
/// reads a file that is not the server's.
#[test]
fn the_last_lines_come_across_rotated_files_in_order() {
    let dir = fresh("order");
    rotated(&dir);
    assert_eq!(lines(&dir, 100), ["l1", "l2", "l3", "l4", "l5", "l6", "l7"]);
    assert_eq!(lines(&dir, 4), ["l4", "l5", "l6", "l7"]);
    assert_eq!(lines(&dir, 1), ["l7"]);
    assert_eq!(lines(&dir, 7).len(), 7);
}

/// Catches: `.10` sorted before `.2` (a text sort, so the order across ten or more files
/// is scrambled), and a last line without its newline (the server's file after a crash
/// mid-line) lost.
#[test]
fn files_sort_by_number_and_a_torn_last_line_is_kept() {
    let dir = fresh("numbers");
    fs::write(dir.join("kbf-server.log.10"), "oldest\n").expect("file");
    fs::write(dir.join("kbf-server.log.2"), "middle\n").expect("file");
    fs::write(dir.join("kbf-server.log"), "newest\ntorn").expect("file");
    assert_eq!(lines(&dir, 100), ["oldest", "middle", "newest", "torn"]);
}

/// Catches: an empty current file (just rotated) hiding the older files, and `print`
/// writing something other than the lines and their newlines.
#[test]
fn an_empty_current_file_and_the_printed_text() {
    let dir = fresh("empty");
    fs::write(dir.join("kbf-server.log.1"), "a\nb\n").expect("file");
    fs::write(dir.join("kbf-server.log"), "").expect("file");
    let mut out = Vec::new();
    print(&dir, 100, &mut out).expect("print");
    assert_eq!(String::from_utf8(out).expect("UTF-8"), "a\nb\n");
}

/// Catches: a directory with no log (a typo in `--log-dir`) that prints nothing and
/// exits 0, and a missing directory that panics; and a read that creates or changes
/// anything.
#[test]
fn a_directory_without_a_log_is_refused_and_nothing_is_changed() {
    let dir = fresh("none");
    fs::write(dir.join("unrelated.log"), "zzz\n").expect("file");
    let err = tail(&dir, 10).expect_err("no log");
    assert!(matches!(err, LogError::NoLog(_)), "{err}");
    assert!(
        err.to_string().contains("not a kbf-server --log-dir"),
        "{err}"
    );
    let missing = tail(&dir.join("absent"), 10).expect_err("no directory");
    assert!(matches!(missing, LogError::Dir { .. }), "{missing}");
    let names: Vec<_> = fs::read_dir(&dir).expect("dir").collect();
    assert_eq!(names.len(), 1);
}

/// `text` read as `file` would be, with `block`-byte blocks, as the last `count` lines.
fn tail_text(name: &str, text: &str, count: usize, block: u64) -> Vec<String> {
    let dir = fresh(name);
    fs::write(dir.join(FILE_NAME), text).expect("file");
    tail_of(open_all(&dir).expect("open"), count, block)
        .expect("a tail")
        .into_iter()
        .map(|l| String::from_utf8(l).expect("UTF-8"))
        .collect()
}

/// Catches: a backwards read that gets a block boundary wrong: a line cut by the
/// boundary returned in part, a line lost or doubled across it, a trailing newline
/// counted as an empty last line, a torn last line dropped, or an empty file (or one
/// that is only a newline) giving the wrong lines. Every count from 1 to past the
/// line count is read at block sizes from 1 byte up past the file, against the lines.
#[test]
fn reading_from_the_end_matches_the_lines_at_every_block_size() {
    let all: Vec<String> = (0..12)
        .map(|n| format!("line {n}{}", "y".repeat(n)))
        .collect();
    for ending in ["\n", "", "\n\n"] {
        let text = format!("{}{ending}", all.join("\n"));
        let mut want = all.clone();
        if ending == "\n\n" {
            want.push(String::new());
        }
        for block in [1, 3, 8, 64, 4096] {
            for count in 1..=want.len() + 2 {
                let from = want.len().saturating_sub(count);
                assert_eq!(
                    tail_text("blocks", &text, count, block),
                    want[from..],
                    "{ending:?} block {block} count {count}"
                );
            }
        }
    }
    assert_eq!(tail_text("blocks-empty", "", 5, 4), Vec::<String>::new());
    assert_eq!(tail_text("blocks-nl", "\n", 5, 4), [""]);
}

/// Catches: a tail that lists the files and then reads them by path one at a time, so a
/// rotation between two reads prints the lines of the current file again as `.1` and
/// skips the old `.1`. The files are opened, the server rotates (every file one older,
/// a new current file), and the lines read are still the ones before the rotation, once.
#[test]
fn a_rotation_after_the_files_are_opened_changes_nothing_read() {
    let dir = fresh("snapshot");
    rotated(&dir);
    let opened = open_all(&dir).expect("open");

    fs::rename(dir.join("kbf-server.log.1"), dir.join("kbf-server.log.2")).expect("rotate");
    fs::rename(dir.join("kbf-server.log"), dir.join("kbf-server.log.1")).expect("rotate");
    fs::write(dir.join("kbf-server.log"), "new\n").expect("new current");

    let read: Vec<String> = tail_of(opened, 100, BLOCK)
        .expect("a tail")
        .into_iter()
        .map(|l| String::from_utf8(l).expect("UTF-8"))
        .collect();
    assert_eq!(read, ["l1", "l2", "l3", "l4", "l5", "l6", "l7"]);
    assert_eq!(lines(&dir, 100).last().map(String::as_str), Some("new"));
}

/// Catches: a file the server's rotation removed since the listing failing the whole
/// read (it is skipped), a file that cannot be opened for another reason skipped (it
/// must be named in the error), and a file that cannot be read (a directory named like
/// a log file) read as empty.
#[test]
fn a_vanished_file_is_skipped_and_an_unreadable_one_is_named() {
    let dir = fresh("unreadable");
    fs::write(dir.join("kbf-server.log"), "a\n").expect("file");
    let gone = dir.join("kbf-server.log.1");
    let opened = open_paths(vec![dir.join(FILE_NAME), gone]).expect("open");
    assert_eq!(opened.len(), 1);

    let blocked = dir.join("kbf-server.log").join("inside");
    let err = open_paths(vec![blocked]).err().expect("not openable");
    assert!(matches!(err, LogError::Read { .. }), "{err}");
    assert!(err.to_string().contains("inside"), "{err}");

    let odd = fresh("directory-log");
    fs::create_dir(odd.join(FILE_NAME)).expect("dir");
    let err = tail(&odd, 5).expect_err("a directory is not a log");
    assert!(matches!(err, LogError::Read { .. }), "{err}");
}

/// The command line over `args`: exit status, stdout and stderr.
fn cli(args: &[&str]) -> (std::process::ExitCode, String, String) {
    let mut exec = kbf_host::testkit::FakeExec::new(501);
    let args = std::iter::once("kbf-deploy")
        .chain(args.iter().copied())
        .map(std::ffi::OsString::from)
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::cli::run(args, &mut exec, &|_| None, &mut out, &mut err);
    let text = |b: Vec<u8>| String::from_utf8(b).expect("UTF-8");
    (code, text(out), text(err))
}

/// Catches: `kbf-deploy log` printing the lines in another order than the file's, not
/// honouring `--lines` or defaulting to another count than 100, `--hot-store` not
/// naming `<dir>/log`, and a bad directory exiting 0 or not saying why.
#[test]
fn the_log_verb_prints_the_last_lines_and_refuses_what_it_cannot_read() {
    let dir = fresh("verb");
    rotated(&dir);
    let path = dir.to_str().expect("UTF-8 path");
    let (code, out, err) = cli(&["log", "--log-dir", path]);
    assert_eq!((code, err.as_str()), (std::process::ExitCode::SUCCESS, ""));
    assert_eq!(out, "l1\nl2\nl3\nl4\nl5\nl6\nl7\n");
    let (_, out, _) = cli(&["log", "--log-dir", path, "--lines", "3"]);
    assert_eq!(out, "l5\nl6\nl7\n");

    let hot = fresh("verb-hot");
    fs::create_dir(hot.join("log")).expect("log dir");
    fs::write(hot.join("log").join(FILE_NAME), "from the hot store\n").expect("file");
    let (code, out, _) = cli(&["log", "--hot-store", hot.to_str().expect("UTF-8 path")]);
    assert_eq!(
        (code, out.as_str()),
        (std::process::ExitCode::SUCCESS, "from the hot store\n")
    );

    let missing = dir.join("absent");
    let (code, out, err) = cli(&["log", "--log-dir", missing.to_str().expect("UTF-8 path")]);
    assert_eq!(code, std::process::ExitCode::from(2));
    assert_eq!(out, "");
    assert!(err.starts_with("REFUSED: "), "{err}");
}

/// Catches: `log` accepted with no directory, with both, or with a count of 0.
#[test]
fn the_log_verb_needs_exactly_one_place_and_a_positive_count() {
    for args in [
        &["log"][..],
        &["log", "--log-dir", "/a", "--hot-store", "/b"],
        &["log", "--log-dir", "/a", "--lines", "0"],
    ] {
        let (code, out, _) = cli(args);
        assert_eq!(code, std::process::ExitCode::from(2), "{args:?}");
        assert_eq!(out, "", "{args:?}");
    }
}
