//! The argv0 fakes (`kbf_testkit::fake`), run through links as the code under test
//! runs `xcrun`, `xcodebuild` and `sandbox-exec`: which rule answers, what the answer
//! prints and exits with, what the log records, and what `sandbox-exec` runs.

use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ACT: &str = env!("CARGO_BIN_EXE_kbf-act");

/// A fresh directory for one test, under Cargo's per-target temporary directory.
fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("old dir");
    }
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// Links the program as `dir/<tool>` with `rules` beside it.
fn fake(dir: &Path, tool: &str, rules: &str) -> PathBuf {
    let link = dir.join(tool);
    std::os::unix::fs::symlink(ACT, &link).expect("link");
    std::fs::write(dir.join(format!("{tool}.rules")), rules).expect("rules");
    link
}

fn run(program: &Path, args: &[&str], developer_dir: Option<&str>) -> Output {
    let mut command = Command::new(program);
    command.args(args);
    match developer_dir {
        Some(dir) => command.env("DEVELOPER_DIR", dir),
        None => command.env_remove("DEVELOPER_DIR"),
    };
    command.output().expect("the fake runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A run of a fake and its answer: (program, arguments, `DEVELOPER_DIR`, exit status,
/// stdout, stderr).
type Case<'a> = (
    &'a Path,
    &'a [&'a str],
    Option<&'a str>,
    i32,
    String,
    &'a str,
);

const XCRUN_RULES: &str = "# xcrun, as tests/fake.rs uses it\n\
\n\
--find *\tDEVELOPER_DIR=*/Xcode_broken.app/Contents/Developer\techo --stderr You have not agreed. ; exit 69\n\
--find clang\tDEVELOPER_DIR=*/Contents/Developer\techo ${DEVELOPER_DIR}/usr/bin/$2\n\
--find *\tDEVELOPER_DIR=*/Contents/Developer\techo second rule ; exit 5\n\
--version\t-\techo xcrun version 70\n";

/// Catches: a rule matched on part of the arguments or on another variable's value;
/// a later rule winning over the first; `$2` or `${DEVELOPER_DIR}` not replaced; a
/// rule's `exit` not kept, or a matched rule without one not exiting 0; no match not
/// exiting 64 with the arguments named; the rules file looked for only beside the
/// program, not along the link the code under test ran (an app's `usr/bin/xcrun`
/// pointing at the test's one fake); the log missing a run or its argv0.
#[test]
fn xcrun_answers_by_the_first_matching_rule() {
    let dir = dir("fake-xcrun");
    let fake = fake(&dir, "xcrun", XCRUN_RULES);
    let developer = dir.join("Xcode_good.app/Contents/Developer");
    let bin = developer.join("usr/bin");
    std::fs::create_dir_all(&bin).expect("usr/bin");
    let chained = bin.join("xcrun");
    std::os::unix::fs::symlink(&fake, &chained).expect("chain");
    let good = developer.display().to_string();
    let broken = "/A/Xcode_broken.app/Contents/Developer";
    let cases: [Case<'_>; 7] = [
        (
            &chained,
            &["--find", "clang"],
            Some(&good),
            0,
            format!("{good}/usr/bin/clang\n"),
            "",
        ),
        (
            &fake,
            &["--find", "metal"],
            Some(&good),
            5,
            "second rule\n".to_owned(),
            "",
        ),
        (
            &fake,
            &["--find", "clang"],
            Some(broken),
            69,
            String::new(),
            "You have not agreed.\n",
        ),
        (
            &fake,
            &["--find", "clang"],
            None,
            64,
            String::new(),
            "unknown: --find clang\n",
        ),
        (
            &fake,
            &["--find", "clang", "x"],
            Some(&good),
            5,
            "second rule\n".to_owned(),
            "",
        ),
        (
            &fake,
            &["--version"],
            None,
            0,
            "xcrun version 70\n".to_owned(),
            "",
        ),
        (
            &fake,
            &["--version", "x"],
            None,
            64,
            String::new(),
            "unknown: --version x\n",
        ),
    ];
    for (program, args, developer_dir, code, stdout, stderr) in &cases {
        let out = run(program, args, *developer_dir);
        assert_eq!(
            out.status.code(),
            Some(*code),
            "{args:?} {developer_dir:?}: {out:?}"
        );
        assert_eq!(text(&out.stdout), *stdout, "{args:?} {developer_dir:?}");
        assert_eq!(text(&out.stderr), *stderr, "{args:?} {developer_dir:?}");
    }
    let log = std::fs::read_to_string(dir.join("xcrun.log")).expect("log");
    let want: Vec<String> = cases
        .iter()
        .map(|(program, args, ..)| format!("{} {}", program.display(), args.join(" ")))
        .collect();
    assert_eq!(log.lines().collect::<Vec<_>>(), want);
}

/// Catches: a fake run by its bare name (as `Command::new("xcodebuild")` runs it,
/// through `PATH`) not finding its rules on `PATH`; `xcodebuild` not told apart from
/// `xcrun` (its rules file is its own); a question no rule answers not exiting 64.
#[test]
fn xcodebuild_found_through_path_reads_its_own_rules() {
    let dir = dir("fake-xcodebuild");
    fake(
        &dir,
        "xcodebuild",
        "-version\t-\techo Xcode 16 ; echo Build version 16C5032a\n",
    );
    fake(&dir, "xcrun", "*\t-\texit 9\n");
    let out = Command::new("xcodebuild")
        .arg("-version")
        .env("PATH", &dir)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), "Xcode 16\nBuild version 16C5032a\n");
    let out = Command::new("xcodebuild")
        .args(["-license", "check"])
        .env("PATH", &dir)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(64), "{out:?}");
    assert_eq!(text(&out.stderr), "unknown: -license check\n");
    let log = std::fs::read_to_string(dir.join("xcodebuild.log")).expect("log");
    assert_eq!(log, "xcodebuild -version\nxcodebuild -license check\n");
}

/// Catches: `sandbox-exec` running its own options as the command, not running the
/// command, a matched rule's steps skipped before the command, no command not
/// refused, and a command that cannot be run not reported (exit 1).
#[test]
fn sandbox_exec_runs_its_rule_then_becomes_the_command() {
    let dir = dir("fake-sandbox-exec");
    let note = dir.join("note");
    let rules = format!("*\t-\tappend {} TMPDIR=${{TMPDIR}}\n", note.display());
    let fake = fake(&dir, "sandbox-exec", &rules);
    let pid_file = dir.join("pid").display().to_string();
    let out = Command::new(&fake)
        .args([
            "-p",
            "(version 1)",
            "-D",
            "LEASE=/l",
            "-f",
            "/p",
            "-n",
            "n",
            ACT,
        ])
        .args(["echo", "ran", ";", "append", &pid_file, "x"])
        .env("TMPDIR", "/tmp/lease")
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), "ran\n");
    assert_eq!(
        std::fs::read_to_string(&note).expect("note"),
        "TMPDIR=/tmp/lease\n"
    );
    assert!(Path::new(&pid_file).exists());
    let out = Command::new(&fake)
        .args(["-p", "(version 1)"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(text(&out.stderr).contains("usage: sandbox-exec"), "{out:?}");
    let missing = dir.join("missing").display().to_string();
    for command in [vec![missing.as_str()], vec!["-p", "x", &missing, "-p"]] {
        let out = Command::new(&fake).args(&command).output().expect("runs");
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(text(&out.stderr).contains("exec "), "{out:?}");
    }
}

/// Catches: `sandbox-exec` running the command in a child, under a different process
/// id (kbf kills and accounts for the process it started).
#[test]
fn sandbox_exec_keeps_its_process_id() {
    let dir = dir("fake-sandbox-pid");
    let fake = fake(&dir, "sandbox-exec", "");
    let mut child = Command::new(&fake)
        .args(["-p", "x", ACT, "setsid", ";", "sleep", "3"])
        .spawn()
        .expect("runs");
    let pid = rustix::process::Pid::from_child(&child);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    // `setsid` makes the command lead a session whose id is its process id; under
    // another process id the session would be someone else's.
    while rustix::process::getsid(Some(pid)).ok() != Some(pid) {
        assert!(
            std::time::Instant::now() < until,
            "the command never ran as the fake's pid"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Let it exit by itself: a killed process writes no coverage data.
    let status = child.wait().expect("reaped");
    assert!(status.success(), "{status}");
}

/// Catches: a fake with no rules answering anyway (a test with a misplaced rules file
/// would pass on defaults); a link loop followed forever; a rule with a missing or an
/// extra field, or a malformed condition, skipped instead of refused; a malformed
/// step run.
#[test]
fn missing_and_malformed_rules_are_refused() {
    let dir = dir("fake-bad");
    let lonely = dir.join("lonely/xcrun");
    std::fs::create_dir_all(lonely.parent().expect("parent")).expect("dir");
    std::os::unix::fs::symlink(ACT, &lonely).expect("link");
    let looped = dir.join("loop/xcrun");
    std::fs::create_dir_all(looped.parent().expect("parent")).expect("dir");
    std::os::unix::fs::symlink("other", &looped).expect("link");
    std::os::unix::fs::symlink("xcrun", dir.join("loop/other")).expect("link");
    // A loop cannot be run, so the loop is argv0 only: the fake must still stop.
    for program in [&lonely, &looped] {
        let out = Command::new(ACT)
            .arg0(program)
            .args(["--find", "clang"])
            .output()
            .expect("runs");
        assert_eq!(out.status.code(), Some(1), "{program:?}: {out:?}");
        assert!(
            text(&out.stderr).contains("no .rules file beside"),
            "{out:?}"
        );
    }
    // Run by its bare name with no `PATH` entry holding it: no rules anywhere.
    let out = Command::new(ACT)
        .arg0("xcrun")
        .env("PATH", dir.join("empty"))
        .current_dir(&dir)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    for (name, rules, said) in [
        ("two-fields", "--version\t-\n", "bad rule"),
        ("four-fields", "--version\t-\texit 0\textra\n", "bad rule"),
        (
            "condition",
            "--version\tDEVELOPER_DIR\texit 0\n",
            "bad environment condition",
        ),
        ("step", "--version\t-\tnope\n", "bad step"),
    ] {
        let dir = dir.join(name);
        std::fs::create_dir_all(&dir).expect("dir");
        let fake = fake(&dir, "xcrun", rules);
        let out = run(&fake, &["--version"], None);
        assert_eq!(out.status.code(), Some(2), "{name}: {out:?}");
        assert!(text(&out.stderr).contains(said), "{name}: {out:?}");
    }
}
