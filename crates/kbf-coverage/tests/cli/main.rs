//! The `kbf-coverage` binary end to end, on a small workspace built in the test's
//! temporary directory (inside the target directory).
//!
//! Catches: the binary passing a crate under 99% (or under its floor) or failing one at
//! it, branches left out of the gate, a crate without branches failing, a floors file
//! that `raise` writes and `check` cannot read back, `raise` adding or lowering a
//! floor, a lowered floor passing `no-lowering`, a bad input exiting 1 instead of 2, a
//! directory under `crates/` without `Cargo.toml` taken for a crate, and a
//! `--summary-file` truncated, left without the table or not written.
//!
//! Each test builds its own workspace in a fresh directory named for the test and the
//! process, so no test reads a file an earlier run (or a concurrent one) left behind.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod rename;
mod replace;

/// A fixture workspace, removed when dropped.
struct Workspace(PathBuf);

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Workspace {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

/// A workspace with crates `a`, `b` and `empty` (no records), plus a loose file and a
/// directory without `Cargo.toml` under `crates/`, which are not crates. It lives in
/// `<name>-<pid>`, emptied first: a directory left by a run that was killed, or by an
/// earlier process with the same pid, never leaks files into this one. The tracefile
/// gives `a` the given line and branch counts (found, hit) and `b` 10 of 10 lines
/// and no branches.
fn workspace(name: &str, a_lines: (u64, u64), a_branches: (u64, u64)) -> Workspace {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for krate in ["a", "b", "empty"] {
        let dir = root.join("crates").join(krate);
        std::fs::create_dir_all(&dir).expect("create crate dir");
        std::fs::write(dir.join("Cargo.toml"), "").expect("write Cargo.toml");
    }
    std::fs::create_dir_all(root.join("crates/notacrate")).expect("create dir");
    std::fs::write(root.join("crates/notes.txt"), "").expect("write file");
    let r = root.display();
    let lcov = format!(
        "SF:{r}/crates/a/src/lib.rs\nLF:{}\nLH:{}\nBRF:{}\nBRH:{}\nend_of_record\n\
         SF:{r}/crates/b/src/main.rs\nLF:10\nLH:10\nend_of_record\n",
        a_lines.0, a_lines.1, a_branches.0, a_branches.1
    );
    std::fs::write(root.join("lcov.info"), lcov).expect("write lcov");
    Workspace(root)
}

fn kbf_coverage(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-coverage"))
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run kbf-coverage")
}

/// `check` with `floors` as the floors file.
fn run(root: &Path, floors: &str, extra: &[&str]) -> Output {
    std::fs::write(root.join("coverage-floors"), floors).expect("write floors");
    Command::new(env!("CARGO_BIN_EXE_kbf-coverage"))
        .arg("--lcov")
        .arg(root.join("lcov.info"))
        .arg("--root")
        .arg(root)
        .arg("--floors")
        .arg(root.join("coverage-floors"))
        .args(extra)
        .output()
        .expect("run kbf-coverage")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn code(o: &Output) -> Option<i32> {
    o.status.code()
}

/// Catches: the default threshold wrong at its edge, through the binary: 98.99% of
/// lines with no floor exits 1 and names the crate; exactly 99.00% exits 0.
#[test]
fn a_crate_at_98_99_fails_and_at_99_00_passes() {
    let w = workspace("edge-lines", (10_000, 9_899), (0, 0));
    let o = run(&w, "", &[]);
    assert_eq!(code(&o), Some(1), "{}", stdout(&o));
    assert!(
        stdout(&o).contains(
            "| a | 9899/10000 | 98.99 | 99.00 | - | - | 99.00 | BELOW THRESHOLD (lines) |"
        ),
        "{}",
        stdout(&o)
    );
    assert!(stdout(&o).contains("coverage gate: FAIL (1 crate(s)"));

    let w = workspace("edge-lines-99", (10_000, 9_900), (0, 0));
    let o = run(&w, "", &[]);
    assert_eq!(code(&o), Some(0), "{}", stdout(&o));
    assert!(stdout(&o).contains("coverage gate: PASS"), "{}", stdout(&o));
}

/// Catches: branches left out of the binary's gate, or a crate without branches
/// failing it.
#[test]
fn branches_are_gated_and_none_passes() {
    let w = workspace("edge-branches", (10, 10), (10_000, 9_899));
    let o = run(&w, "", &[]);
    assert_eq!(code(&o), Some(1), "{}", stdout(&o));
    assert!(
        stdout(&o).contains("BELOW THRESHOLD (branches)"),
        "{}",
        stdout(&o)
    );

    let w = workspace("edge-branches-99", (10, 10), (10_000, 9_900));
    assert_eq!(code(&run(&w, "", &[])), Some(0));

    // `b` and `empty` have no branches at all, and pass.
    let w = workspace("no-branches", (10, 10), (0, 0));
    assert_eq!(code(&run(&w, "", &[])), Some(0));
}

/// Catches: a floor ignored or off by one through the binary: below its floor fails,
/// at its floor passes, above passes; and a floor listing a crate the workspace lacks,
/// or one with nothing measured, fails.
#[test]
fn a_floor_is_the_threshold_for_its_crate() {
    let w = workspace("floor-at", (10_000, 9_000), (0, 0));
    assert_eq!(code(&run(&w, "a 90.00 -\n", &[])), Some(0));
    assert_eq!(code(&run(&w, "a 89.99 -\n", &[])), Some(0));
    let o = run(&w, "a 90.01 -\n", &[]);
    assert_eq!(code(&o), Some(1), "{}", stdout(&o));
    assert!(stdout(&o).contains("BELOW THRESHOLD (lines)"));
    let o = run(&w, "a 90.00 -\ngone 10.00 -\n", &[]);
    assert_eq!(code(&o), Some(1));
    assert!(stdout(&o).contains("| gone |"), "{}", stdout(&o));
    assert!(stdout(&o).contains("NOT IN WORKSPACE"));
    let o = run(&w, "a 90.00 -\nempty 10.00 -\n", &[]);
    assert_eq!(code(&o), Some(1));
    assert!(stdout(&o).contains("FLOOR BUT NOTHING MEASURED (lines)"));
}

/// Catches: floors that could rise going unnoticed (the run says so, without failing),
/// and the advice naming a command that is not `raise`.
#[test]
fn a_floor_below_the_measurement_is_noted_not_failed() {
    let w = workspace("note", (10_000, 9_500), (0, 0));
    let o = run(&w, "a 90.00 -\n", &[]);
    assert_eq!(code(&o), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    assert!(out.contains("a lines: 90.00 -> 95.00"), "{out}");
    assert!(
        out.contains("raise them with: cargo run -p kbf-coverage -- raise --lcov"),
        "{out}"
    );
    let w = workspace("note-none", (10_000, 9_000), (0, 0));
    assert!(!stdout(&run(&w, "a 90.00 -\n", &[])).contains("raise them"));
}

/// Catches: `raise` not lifting the floor to the measurement (floored), adding a floor,
/// writing something `check` rejects, or a second run changing the file.
#[test]
fn raise_lifts_floors_and_then_check_passes() {
    let w = workspace("raise", (10_001, 9_500), (0, 0));
    std::fs::write(w.join("coverage-floors"), "a 90.00 -\n").expect("write");
    let o = kbf_coverage(&w, &["raise", "--lcov", "lcov.info", "--root", "."]);
    assert_eq!(code(&o), Some(0), "{}", stderr(&o));
    assert!(
        stdout(&o).contains("a lines: 90.00 -> 94.99"),
        "{}",
        stdout(&o)
    );
    let after = std::fs::read_to_string(w.join("coverage-floors")).expect("read");
    assert!(
        after.contains("\na  94.99      -\n") || after.ends_with("a  94.99      -\n"),
        "{after}"
    );
    // Passes at the raised floor, with nothing left to raise.
    let o = run(&w, &after, &[]);
    assert_eq!(code(&o), Some(0), "{}", stdout(&o));
    assert!(!stdout(&o).contains("raise them"));
    let o = kbf_coverage(
        &w,
        &[
            "raise",
            "--lcov",
            "lcov.info",
            "--floors",
            "coverage-floors",
        ],
    );
    assert_eq!(code(&o), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("nothing to change"), "{}", stdout(&o));
    assert_eq!(
        std::fs::read_to_string(w.join("coverage-floors")).expect("read"),
        after
    );
}

/// Catches: a raise run from a subdirectory (`--root ..`) not resolving the root the
/// way cargo does, and a floor added for a crate that has none.
#[test]
fn raise_resolves_a_relative_root_and_adds_nothing() {
    let w = workspace("raise-sub", (10_000, 9_500), (0, 0));
    // The tracefile spells its paths as cargo does: absolute, links resolved.
    let r = std::fs::canonicalize(&*w).expect("canonicalize");
    let r = r.display();
    std::fs::write(
        w.join("lcov.info"),
        format!("SF:{r}/crates/a/src/lib.rs\nLF:10000\nLH:9500\nend_of_record\n"),
    )
    .expect("write");
    std::fs::write(w.join("coverage-floors"), "a 90.00 -\n").expect("write");
    let sub = w.join("crates");
    let o = kbf_coverage(
        &sub,
        &[
            "raise",
            "--lcov",
            "../lcov.info",
            "--root",
            "..",
            "--floors",
            "../coverage-floors",
        ],
    );
    assert_eq!(code(&o), Some(0), "{}", stderr(&o));
    let after = std::fs::read_to_string(w.join("coverage-floors")).expect("read");
    assert!(after.contains("a  95.00      -"), "{after}");
    assert!(!after.contains("\nb "), "no floor is added for b");
    // A floors file that cannot be replaced (its temporary name is a directory) is an
    // input error, and the file keeps its contents.
    std::fs::write(w.join("coverage-floors"), "a 90.00 -\n").expect("write");
    std::fs::create_dir(w.join("coverage-floors.raise-tmp")).expect("mkdir");
    let o = kbf_coverage(
        &sub,
        &[
            "raise",
            "--lcov",
            "../lcov.info",
            "--root",
            "..",
            "--floors",
            "../coverage-floors",
        ],
    );
    assert_eq!(code(&o), Some(2), "{}", stdout(&o));
    assert!(
        stderr(&o).contains("write ../coverage-floors:"),
        "{}",
        stderr(&o)
    );
    assert_eq!(
        std::fs::read_to_string(w.join("coverage-floors")).expect("read"),
        "a 90.00 -\n"
    );
    // A root that does not exist is an input error.
    let o = kbf_coverage(
        &sub,
        &["raise", "--lcov", "../lcov.info", "--root", "no-such"],
    );
    assert_eq!(code(&o), Some(2));
    assert!(
        stderr(&o).contains("resolve --root no-such:"),
        "{}",
        stderr(&o)
    );
}

/// Catches: `raise` of a floor that reached 99% leaving a line that weakens the
/// default, or a stale line surviving.
#[test]
fn raise_drops_a_floor_at_99_and_a_stale_line() {
    let w = workspace("raise-drop", (10_000, 9_900), (0, 0));
    std::fs::write(w.join("coverage-floors"), "a 90.00 -\ngone 1.00 -\n").expect("write");
    let o = kbf_coverage(&w, &["raise", "--lcov", "lcov.info", "--root", "."]);
    assert_eq!(code(&o), Some(0), "{}", stderr(&o));
    let after = std::fs::read_to_string(w.join("coverage-floors")).expect("read");
    assert!(
        !after
            .lines()
            .any(|l| l.starts_with("a ") || l.starts_with("gone")),
        "{after}"
    );
    assert!(
        stdout(&o).contains("a lines: 90.00 -> removed"),
        "{}",
        stdout(&o)
    );
}

/// Catches: a lowered floor, or a newly added one, passing `no-lowering`; a raise or
/// removal failing it; and a missing base file passing silently.
#[test]
fn no_lowering_fails_a_lower_threshold() {
    let w = workspace("nolow", (10, 10), (0, 0));
    let check = |base: &str, head: &str| {
        std::fs::write(w.join("base"), base).expect("write");
        std::fs::write(w.join("head"), head).expect("write");
        kbf_coverage(&w, &["no-lowering", "--base", "base", "--floors", "head"])
    };
    let o = check("a 90.00 80.00\n", "a 90.00 80.00\n");
    assert_eq!(code(&o), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("lowers no threshold"));
    assert_eq!(code(&check("a 90.00 80.00\n", "a 91.00 80.00\n")), Some(0));
    assert_eq!(code(&check("a 90.00 80.00\n", "")), Some(0));
    let o = check("a 90.00 80.00\n", "a 89.99 80.00\n");
    assert_eq!(code(&o), Some(1));
    assert!(
        stderr(&o).contains("a lines: threshold 90.00 -> 89.99 (a floor may only rise)"),
        "{}",
        stderr(&o)
    );
    let o = check("", "b - 10.00\n");
    assert_eq!(code(&o), Some(1));
    assert!(stderr(&o).contains("a floor was added"), "{}", stderr(&o));
    let o = kbf_coverage(
        &w,
        &["no-lowering", "--base", "missing", "--floors", "head"],
    );
    assert_eq!(code(&o), Some(2));
    assert!(stderr(&o).contains("read missing:"), "{}", stderr(&o));
    // With the flag, a base that does not exist passes and says why; one that is
    // there is still read, and an unreadable head is still an error.
    let flagged = |base: &str, floors: &str| {
        kbf_coverage(
            &w,
            &[
                "no-lowering",
                "--allow-missing-base",
                "--base",
                base,
                "--floors",
                floors,
            ],
        )
    };
    let o = flagged("missing", "head");
    assert_eq!(code(&o), Some(0), "{}", stderr(&o));
    assert!(
        stdout(&o).contains("the base has no floors to lower"),
        "{}",
        stdout(&o)
    );
    std::fs::write(w.join("base"), "a 90.00 -\n").expect("write");
    std::fs::write(w.join("head"), "a 80.00 -\n").expect("write");
    assert_eq!(code(&flagged("base", "head")), Some(1));
    let o = flagged("base", "missing-head");
    assert_eq!(code(&o), Some(2));
    assert!(stderr(&o).contains("read missing-head:"), "{}", stderr(&o));
}

/// Catches: a bad input reported as a gate failure (exit 1) instead of 2, or without
/// its reason.
#[test]
fn bad_inputs_exit_2_with_a_reason() {
    let w = workspace("bad", (10, 10), (0, 0));
    let o = run(&w, "a 90.00\n", &[]);
    assert_eq!(code(&o), Some(2));
    assert!(
        stderr(&o).contains("coverage-floors: line 1: expected"),
        "{}",
        stderr(&o)
    );
    let o = run(&w, "a 99.00 -\n", &[]);
    assert_eq!(code(&o), Some(2));
    assert!(stderr(&o).contains("not below 99.00"), "{}", stderr(&o));

    std::fs::write(w.join("lcov.info"), "").expect("write");
    let o = run(&w, "", &[]);
    assert_eq!(code(&o), Some(2));
    assert!(stderr(&o).contains("holds no records"), "{}", stderr(&o));

    std::fs::write(
        w.join("lcov.info"),
        "SF:/elsewhere/x.rs\nLF:1\nLH:1\nend_of_record\n",
    )
    .expect("write");
    let o = run(&w, "", &[]);
    assert_eq!(code(&o), Some(2));
    assert!(
        stderr(&o).contains("is not under <root>/crates/<crate>/"),
        "{}",
        stderr(&o)
    );

    let o = kbf_coverage(
        &w,
        &[
            "--lcov",
            "nope",
            "--root",
            ".",
            "--floors",
            "coverage-floors",
        ],
    );
    assert_eq!(code(&o), Some(2));
    assert!(stderr(&o).contains("read nope:"), "{}", stderr(&o));

    let o = kbf_coverage(
        &w,
        &[
            "--lcov",
            "lcov.info",
            "--root",
            "no-such-root",
            "--floors",
            "coverage-floors",
        ],
    );
    assert_eq!(code(&o), Some(2));
    assert!(
        stderr(&o).contains("resolve --root no-such-root:"),
        "{}",
        stderr(&o)
    );

    // A root that resolves but has no crates directory.
    let o = kbf_coverage(
        &w,
        &[
            "--lcov",
            "lcov.info",
            "--root",
            "crates/a",
            "--floors",
            "coverage-floors",
        ],
    );
    assert_eq!(code(&o), Some(2));
    assert!(
        stderr(&o).contains("list crates under crates/a:"),
        "{}",
        stderr(&o)
    );
}

/// Catches: a summary that replaces earlier content, is missing on a failing run, or
/// an unwritable one that is ignored.
#[test]
fn summary_file_gets_the_table_appended_on_pass_and_fail() {
    let w = workspace("summary", (10_000, 9_899), (0, 0));
    let summary = w.join("summary.md");
    std::fs::write(&summary, "earlier\n").expect("write");
    let s = summary.display().to_string();
    let o = run(&w, "", &["--summary-file", &s]);
    assert_eq!(code(&o), Some(1));
    let got = std::fs::read_to_string(&summary).expect("read");
    assert!(got.starts_with("earlier\n| crate |"), "{got}");
    assert!(got.contains("BELOW THRESHOLD"), "{got}");
    let o = run(&w, "a 98.00 -\n", &["--summary-file", &s]);
    assert_eq!(code(&o), Some(0));
    let again = std::fs::read_to_string(&summary).expect("read");
    assert!(again.starts_with(&got) && again.len() > got.len());
    // A summary in a directory that does not exist: exit 2.
    let o = run(
        &w,
        "",
        &[
            "--summary-file",
            &w.join("no/dir/s.md").display().to_string(),
        ],
    );
    assert_eq!(code(&o), Some(2));
    assert!(stderr(&o).contains("append to"), "{}", stderr(&o));
}

/// Catches: the binary run with no arguments doing something instead of saying what
/// it needs, and `check` named explicitly behaving unlike the default.
#[test]
fn subcommands_and_usage_errors() {
    let w = workspace("usage", (10, 10), (0, 0));
    let o = kbf_coverage(&w, &[]);
    assert_eq!(code(&o), Some(2));
    assert!(
        stderr(&o).contains("required arguments were not provided"),
        "{}",
        stderr(&o)
    );
    std::fs::write(w.join("coverage-floors"), "").expect("write");
    let o = kbf_coverage(
        &w,
        &[
            "check",
            "--lcov",
            "lcov.info",
            "--root",
            ".",
            "--floors",
            "coverage-floors",
        ],
    );
    assert_eq!(code(&o), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("coverage gate: PASS"));
    let o = kbf_coverage(&w, &["raise"]);
    assert_eq!(code(&o), Some(2));
    assert!(stderr(&o).contains("--lcov"), "{}", stderr(&o));
}

/// Catches: a hand edit of `coverage-floors` that `check` would refuse (a floor at 99%,
/// a duplicate, a bad number), or that `raise` would rewrite differently, which would
/// put noise in the next pull request that raises a floor.
#[test]
fn the_committed_floors_file_parses_and_is_in_raise_format() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../coverage-floors");
    let text = std::fs::read_to_string(&path).expect("read coverage-floors");
    let floors = kbf_coverage::floors::parse(&text).expect("parse");
    assert_eq!(kbf_coverage::floors::render(&floors), text);
}
