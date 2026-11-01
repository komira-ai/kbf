use kbf_testkit::dir::TestDir;

use super::*;

/// A directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-profiles-complete-unit"), name)
}

fn put(dir: &TestDir, name: &str, bytes: &[u8]) {
    std::fs::write(dir.to_path_buf().join(name), bytes).expect("write");
}

fn args(dir: &TestDir, prefixes: &[&str]) -> Args {
    Args {
        dir: dir.to_path_buf(),
        prefixes: prefixes.iter().map(|p| (*p).to_owned()).collect(),
        extension: "profraw".to_owned(),
    }
}

fn refused(r: Result<Vec<String>, Error>) -> Vec<String> {
    match r {
        Err(Error::Refused(problems)) => problems,
        other => panic!("not refused: {other:?}"),
    }
}

/// Catches: a directory with every shard's profiles refused, or the counts not reported;
/// non-profile files and a subdirectory taken for profiles.
#[test]
fn every_shard_present_passes() {
    let d = scratch("whole");
    put(&d, "kbf-legacy-1-10-0.profraw", b"abc");
    put(&d, "kbf-legacy-1-11-0.profraw", b"de");
    put(&d, "kbf-legacy-2-10-0.profraw", b"f");
    put(&d, "kbf-js-1-10-0.profraw", b"ghij");
    put(&d, "kbf-legacy-9-notes.txt", b"not a profile");
    std::fs::create_dir(d.to_path_buf().join("kbf-legacy-9-dir.profraw")).expect("dir");
    let lines = run(&args(&d, &["kbf-legacy-1-", "kbf-legacy-2-", "kbf-js-1-"])).expect("whole");
    assert_eq!(
        lines,
        [
            "profiles-complete: `kbf-legacy-1-` 2 file(s), 5 byte(s)",
            "profiles-complete: `kbf-legacy-2-` 1 file(s), 1 byte(s)",
            "profiles-complete: `kbf-js-1-` 1 file(s), 4 byte(s)",
        ]
    );
}

/// Catches: a shard whose artifact did not come, or came with empty files only, passing; a
/// shard that has one empty and one real file refused; the problems of every shard reported
/// together.
#[test]
fn a_missing_or_empty_shard_is_refused() {
    let d = scratch("missing");
    put(&d, "kbf-legacy-1-10-0.profraw", b"abc");
    put(&d, "kbf-js-1-10-0.profraw", b"");
    put(&d, "kbf-js-1-11-0.profraw", b"");
    put(&d, "kbf-legacy-3-10-0.profraw", b"");
    put(&d, "kbf-legacy-3-11-0.profraw", b"x");
    let problems = refused(run(&args(
        &d,
        &[
            "kbf-legacy-1-",
            "kbf-legacy-2-",
            "kbf-js-1-",
            "kbf-legacy-3-",
        ],
    )));
    assert_eq!(
        problems,
        [
            format!(
                "no raw profile in {} begins `kbf-legacy-2-`: that shard's profiles did not arrive",
                d.to_path_buf().display()
            ),
            "every raw profile that begins `kbf-js-1-` is empty (2 file(s))".to_owned(),
        ]
    );
}

/// Catches: profiles of a shard the run does not list (a matrix grown without the check)
/// passing, the list of them growing with the number of stray files, and a prefix that
/// matches nothing (the shard's name changed) passing because the stray files are not
/// looked at.
#[test]
fn a_profile_no_shard_owns_is_refused() {
    let d = scratch("stray");
    put(&d, "kbf-legacy-1-10-0.profraw", b"abc");
    for n in 0..7 {
        put(&d, &format!("kbf-other-{n}.profraw"), b"x");
    }
    let problems = refused(run(&args(&d, &["kbf-legacy-1-"])));
    assert_eq!(
        problems,
        [
            "7 raw profile(s) begin with none of the shards' prefixes, e.g. kbf-other-0.profraw, kbf-other-1.profraw, kbf-other-2.profraw, kbf-other-3.profraw, kbf-other-4.profraw"
        ]
    );
    let problems = refused(run(&args(&d, &["kbf-renamed-"])));
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].contains("begins `kbf-renamed-`"));
}

/// Catches: a directory that is not there read as an empty one.
#[test]
fn a_directory_that_cannot_be_read_is_an_error() {
    let d = scratch("unreadable");
    let args = Args {
        dir: d.to_path_buf().join("nowhere"),
        prefixes: vec!["kbf-".to_owned()],
        extension: "profraw".to_owned(),
    };
    let e = run(&args).expect_err("no directory");
    assert!(e.to_string().starts_with("read "), "{e}");
}

/// Catches: `--extension profdata` still reading `.profraw` files (the merged profiles of a shard
/// would then never be found), and a shard's raw profile being counted as its merged one.
#[test]
fn the_extension_picks_which_files_are_checked() {
    let d = scratch("extension");
    put(&d, "kbf-legacy-1-merged.profdata", b"abc");
    put(&d, "kbf-js-1-merged.profdata", b"de");
    let mut a = args(&d, &["kbf-legacy-1-", "kbf-js-1-"]);
    a.extension = "profdata".to_owned();
    assert_eq!(run(&a).map(|l| l.len()).ok(), Some(2));
    let raw = args(&d, &["kbf-legacy-1-", "kbf-js-1-"]);
    let problems = refused(run(&raw));
    assert!(problems[0].contains("kbf-legacy-1-"), "{problems:?}");
    put(&d, "kbf-js-2-merged.profdata", b"x");
    let problems = refused(run(&a));
    assert!(
        problems[0].contains("begin with none of the shards' prefixes"),
        "{problems:?}"
    );
}
