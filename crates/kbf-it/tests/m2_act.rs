//! The `kbf-m2-act` binary, run as an action runs it: each verb's output file and its
//! exit code (0 when the verb ran, 1 when it failed, 2 on a usage error, each failure
//! with a line on stderr). The code is tested only through the binary, so coverage
//! counts one build of it.

use kbf_testkit::dir::TestDir;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn act(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-m2-act"))
        .args(args)
        .output()
        .expect("kbf-m2-act runs")
}

/// A fresh directory for one test, under Cargo's per-target temporary directory.
fn dir(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

fn path(dir: &Path, name: &str) -> String {
    dir.join(name).display().to_string()
}

/// Runs `args`, requires exit 0, and returns what it wrote to `out`.
fn ran(args: &[&str], out: &str) -> Vec<u8> {
    let output = act(args);
    assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
    std::fs::read(out).expect("the output file")
}

/// Catches: a sort that keeps input order, drops the last line when it has no
/// newline, or keeps empty lines; `upper` not applied; `join` that drops or reorders
/// an input; `pause` or `hog` that writes other than its salt or size (the sample's
/// outputs, and run.sh's check of `//:hog`'s, would differ).
#[test]
fn every_verb_writes_its_output() {
    let dir = dir("m2-act-verbs");
    let (words, other, out) = (path(&dir, "words"), path(&dir, "other"), path(&dir, "out"));
    std::fs::write(&words, "pear\napple\n\nfig").expect("words");
    std::fs::write(&other, "two\n").expect("other");
    assert_eq!(ran(&["sort", &out, &words], &out), b"apple\nfig\npear\n");
    assert_eq!(ran(&["upper", &out, &other], &out), b"TWO\n");
    assert_eq!(
        ran(&["join", &out, &other, &words], &out),
        b"two\npear\napple\n\nfig"
    );
    assert_eq!(ran(&["pause", &out, "0", "a-salt"], &out), b"a-salt\n");
    assert_eq!(ran(&["hog", &out, "32"], &out), b"32\n");
}

/// Catches: a failure that exits 0 (buck2 would cache a broken output as the
/// action's), a usage error not told apart from a failed verb, a missing argument or
/// a non-number run with a default instead of refused, a failed read, create or write
/// not reported, and a failure with nothing on stderr (an action's stderr is all a
/// user sees of why).
#[test]
fn failures_and_usage_errors_exit_non_zero_with_a_reason() {
    let dir = dir("m2-act-failures");
    let (words, out) = (path(&dir, "words"), path(&dir, "out"));
    std::fs::write(&words, "a\n").expect("words");
    let missing = path(&dir, "missing");
    let no_dir = path(&dir, "no/such/out");
    let cases: [(&[&str], i32, &str); 8] = [
        (&["sort", &out, &missing], 1, "read "),
        (&["sort", &no_dir, &words], 1, "create "),
        (&["sort", "/dev/full", &words], 1, "write "),
        (&["sort"], 2, "usage"),
        (&["hog", &out], 2, "bad arguments"),
        (&["pause", &out, "x", "salt"], 2, "not a whole number"),
        (&["join", &out], 2, "bad arguments"),
        (&["nope", &out], 2, "bad arguments"),
    ];
    for (args, code, said) in cases {
        let output = act(args);
        assert_eq!(output.status.code(), Some(code), "{args:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(said), "{args:?}: {stderr}");
    }
}
