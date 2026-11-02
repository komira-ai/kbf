//! The fake service's hook (`root::test_hook`), run in this process the way the tests of the root
//! part run it as a program.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::test_hook::{EXIT_BROKEN, RECORD_DIR_FILE, exit_code};
use crate::testutil;

fn args(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// A program path in a fresh release directory whose `record-dir` names `dir`.
fn program_for(dir: &Path) -> PathBuf {
    let release = testutil::scratch("test-hook-release");
    fs::write(
        release.join(RECORD_DIR_FILE),
        format!("{}\n", dir.display()),
    )
    .unwrap();
    release.join("hook")
}

/// The hook run with `words` over the record directory `dir`: its exit code and its standard
/// output.
fn hook(words: &[&str], dir: &Path) -> (u8, String) {
    let mut out = Vec::new();
    let code = exit_code(&args(words), Some(program_for(dir).as_path()), &mut out);
    (code, String::from_utf8(out).unwrap())
}

/// Catches a run the root part's tests cannot read back: the word (after `update-hook`) goes to
/// `steps.log`, the arguments to `args.log`, and with no `exit-<word>` the run exits 0 with `{}`
/// as its last line, after a line of noise.
#[test]
fn a_run_logs_its_word_and_arguments_and_succeeds_unless_told_otherwise() {
    let dir = testutil::scratch("test-hook-log");
    let ran = hook(
        &["update-hook", "prepare", "/r/2.0.0", "--params", "/p"],
        &dir,
    );
    assert_eq!(
        ran,
        (
            0,
            "prepare: a line of noise before the report\n{}\n".to_owned()
        )
    );
    assert_eq!(hook(&["restore"], &dir).0, 0);
    assert_eq!(
        fs::read_to_string(dir.join("steps.log")).unwrap(),
        "prepare\nrestore\n"
    );
    assert_eq!(
        fs::read_to_string(dir.join("args.log")).unwrap(),
        "prepare /r/2.0.0 --params /p\nrestore\n"
    );
}

/// Catches a hook that does not do as its test tells it: `exit-<word>` gives the exit code and
/// the last line, a code alone keeps `{}`, and a code that is not one is 0.
#[test]
fn a_run_exits_and_reports_as_its_exit_file_says() {
    let dir = testutil::scratch("test-hook-told");
    fs::write(dir.join("exit-verify"), "3 {\"retry_after_secs\":5}\n").unwrap();
    fs::write(dir.join("exit-cleanup"), "7").unwrap();
    fs::write(dir.join("exit-cutover"), "never 9").unwrap();
    let (code, out) = hook(&["verify"], &dir);
    assert_eq!(code, 3);
    assert_eq!(out.lines().last(), Some("{\"retry_after_secs\":5}"));
    assert_eq!(
        hook(&["cleanup"], &dir),
        (
            7,
            "cleanup: a line of noise before the report\n{}\n".to_owned()
        )
    );
    assert_eq!(hook(&["cutover"], &dir).0, 0);
}

/// A standard output that refuses every write.
struct Closed;

impl Write for Closed {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Catches a broken run reported as a step's outcome: an unknown program path, no `record-dir`
/// beside it, a record directory that cannot be written, or a standard output that cannot be, is
/// the hook's own exit code, never 0.
#[test]
fn a_run_with_no_place_to_log_or_report_is_broken() {
    let mut out = Vec::new();
    assert_eq!(exit_code(&args(&["prepare"]), None, &mut out), EXIT_BROKEN);
    assert!(out.is_empty());
    let bare = testutil::scratch("test-hook-bare").join("hook");
    assert_eq!(
        exit_code(&args(&["prepare"]), Some(bare.as_path()), &mut out),
        EXIT_BROKEN
    );
    let gone = testutil::scratch("test-hook-gone").join("missing");
    assert_eq!(hook(&["prepare"], &gone).0, EXIT_BROKEN);
    let dir = testutil::scratch("test-hook-closed");
    assert_eq!(
        exit_code(
            &args(&["prepare"]),
            Some(program_for(&dir).as_path()),
            &mut Closed
        ),
        EXIT_BROKEN
    );
}
