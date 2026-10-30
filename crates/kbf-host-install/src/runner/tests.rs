use std::collections::BTreeSet;
use std::time::Duration;

use super::{Cmd, DEBIAN_FRONTEND, Exec, LANG, PATH, Runner, SystemRunner, WriteCap, fake};

fn cmd(program: &str, args: &[&str]) -> Cmd {
    Cmd {
        program: program.into(),
        args: args.iter().map(|a| (*a).into()).collect(),
        ..Cmd::default()
    }
}

fn within(program: &str, args: &[&str], limit: Duration) -> Cmd {
    Cmd {
        timeout: Some(limit),
        ..cmd(program, args)
    }
}

/// Catches an environment that leaks: the child of the real runner sees `PATH`, `LANG=C` and `DEBIAN_FRONTEND`
/// and nothing else, whatever this process has.
#[test]
fn the_child_environment_is_cleared() {
    // This process has a full environment (cargo and the harness set several variables);
    // none of it may reach the child.
    let out = SystemRunner.run(&cmd("/usr/bin/env", &[])).unwrap();
    assert!(out.success());
    let seen: BTreeSet<String> = out.stdout_text().lines().map(str::to_owned).collect();
    let want: BTreeSet<String> = [
        format!("PATH={PATH}"),
        format!("LANG={LANG}"),
        format!("DEBIAN_FRONTEND={DEBIAN_FRONTEND}"),
    ]
    .into_iter()
    .collect();
    assert_eq!(seen, want);
}

/// Catches an exit status or a stream that is lost: status, stdout and stderr all come back.
#[test]
fn status_and_streams_are_captured() {
    let failed = SystemRunner.run(&cmd("/usr/bin/false", &[])).unwrap();
    assert_eq!(failed.code, Some(1));
    assert!(!failed.success());
    let echoed = SystemRunner
        .run(&cmd("/usr/bin/env", &["/nonexistent-kbf-program"]))
        .unwrap();
    assert_eq!(echoed.code, Some(127));
    assert!(!echoed.stderr_text().is_empty());
    assert!(echoed.stdout.is_empty());
}

/// Catches a runner that swallows a program it could not start.
#[test]
fn a_missing_program_is_an_error() {
    assert!(
        SystemRunner
            .run(&cmd("/nonexistent-kbf-program", &[]))
            .is_err()
    );
}

/// A child with no stdin: `cat` reads end of file at once instead of waiting for a terminal.
#[test]
fn stdin_is_closed() {
    let out = SystemRunner.run(&cmd("/usr/bin/cat", &[])).unwrap();
    assert!(out.success() && out.stdout.is_empty());
}

#[test]
fn exec_text_replaces_invalid_bytes() {
    let exec = Exec {
        code: Some(0),
        signal: None,
        stdout: vec![b'a', 0xff],
        stderr: vec![0xfe, b'b'],
    };
    assert_eq!(exec.stdout_text(), "a\u{fffd}");
    assert_eq!(exec.stderr_text(), "\u{fffd}b");
    assert!(exec.success());
    assert!(
        !Exec {
            code: None,
            ..exec.clone()
        }
        .success(),
        "a signal is not success"
    );
    assert!(
        !Exec {
            code: Some(3),
            ..exec
        }
        .success()
    );
}

/// The fake records what it is given and refuses a call it was not scripted for.
#[test]
fn the_fake_records_and_scripts() {
    let runner = fake::FakeRunner::new(vec![fake::ok("one")]);
    assert_eq!(runner.run(&cmd("/a", &["x"])).unwrap().stdout_text(), "one");
    assert!(runner.run(&cmd("/b", &[])).is_err());
    assert_eq!(runner.calls(), vec![cmd("/a", &["x"]), cmd("/b", &[])]);
}

/// The processes whose command line has `needle` as an argument.
fn processes_with(needle: &str) -> usize {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
        .filter(|line| line.split(|b| *b == 0).any(|arg| arg == needle.as_bytes()))
        .count()
}

/// Catches a limit that is not kept: a child that outlives it is killed, the call ends in
/// a timeout error (not at the child's own end, which is half a minute away, with its own
/// status), and the child is gone afterwards (killed and reaped, not left behind).
#[test]
fn a_child_past_its_limit_is_killed() {
    let marker = "31.4159265";
    let error = SystemRunner
        .run(&within(
            "/usr/bin/sleep",
            &[marker],
            Duration::from_millis(300),
        ))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
    assert_eq!(processes_with(marker), 0, "the sleeper was left running");
}

/// Catches a limit that breaks a child that is within it: status, both streams and a
/// stream far larger than a pipe holds (the child blocks on a full pipe unless it is read
/// while it runs) all come back.
#[test]
fn a_child_within_its_limit_is_unaffected() {
    let limit = Duration::from_secs(60);
    let ok = SystemRunner
        .run(&within("/usr/bin/true", &[], limit))
        .unwrap();
    assert!(ok.success());
    let failed = SystemRunner
        .run(&within("/usr/bin/false", &[], limit))
        .unwrap();
    assert_eq!(failed.code, Some(1));
    let stderr = SystemRunner
        .run(&within(
            "/usr/bin/env",
            &["/nonexistent-kbf-program"],
            limit,
        ))
        .unwrap();
    assert_eq!(stderr.code, Some(127));
    assert!(!stderr.stderr_text().is_empty() && stderr.stdout.is_empty());
    let big = SystemRunner
        .run(&within(
            "/usr/bin/head",
            &["-c", "1000000", "/dev/zero"],
            limit,
        ))
        .unwrap();
    assert!(big.success());
    assert_eq!(big.stdout.len(), 1_000_000);
    let env = SystemRunner
        .run(&within("/usr/bin/env", &[], limit))
        .unwrap();
    assert!(env.stdout_text().contains("LANG=C"));
}

/// A program that cannot start is an error with a limit too, not a timeout.
#[test]
fn a_missing_program_is_an_error_within_a_limit() {
    let error = SystemRunner
        .run(&within(
            "/nonexistent-kbf-program",
            &[],
            Duration::from_secs(5),
        ))
        .unwrap_err();
    assert_ne!(error.kind(), std::io::ErrorKind::TimedOut);
}

/// Variables a command asks for are added to the fixed three, and nothing else.
#[test]
fn a_command_may_add_variables() {
    let mut c = cmd("/usr/bin/env", &[]);
    c.env = vec![("HOME".into(), "/h".into()), ("GH_X".into(), "1".into())];
    let out = SystemRunner.run(&c).unwrap();
    let seen: BTreeSet<String> = out.stdout_text().lines().map(str::to_owned).collect();
    assert!(seen.contains("HOME=/h") && seen.contains("GH_X=1"));
    assert_eq!(seen.len(), 5);
}

/// Bytes given to a command arrive on its standard input, which is then closed; they are
/// written while the command runs, so more than a pipe holds does not block.
#[test]
fn a_command_may_be_given_standard_input() {
    let mut c = cmd("/usr/bin/cat", &[]);
    c.stdin = b"query".to_vec();
    let out = SystemRunner.run(&c).unwrap();
    assert_eq!(out.stdout, b"query");
    c.stdin = vec![b'x'; 1 << 20];
    assert_eq!(SystemRunner.run(&c).unwrap().stdout.len(), 1 << 20);
    // A command that never reads does not hang the writer.
    let mut c = cmd("/usr/bin/true", &[]);
    c.stdin = vec![b'x'; 1 << 20];
    assert!(SystemRunner.run(&c).unwrap().success());
}

/// A command runs as the user and group it is given: the caller's own is allowed, and
/// another one is refused to a caller who is not root.
#[test]
fn a_command_may_run_as_a_user() {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata("/proc/self").unwrap();
    let (uid, gid) = (meta.uid(), meta.gid());
    let mut c = cmd("/usr/bin/id", &["-u"]);
    c.run_as = Some(super::Identity { uid, gid });
    assert_eq!(
        SystemRunner.run(&c).unwrap().stdout_text().trim(),
        uid.to_string()
    );
    if uid != 0 {
        c.run_as = Some(super::Identity { uid: uid + 1, gid });
        assert!(SystemRunner.run(&c).is_err());
    }
}

/// A drop made by this process is refused when its effective uid is root and its real uid is
/// not (the standard library would keep the caller's groups); a plain root, a plain user, a
/// real root with another effective uid and a command that drops nothing pass.
#[test]
fn a_privilege_drop_needs_a_real_uid_of_root() {
    use super::{Identity, refuse_setuid_drop};
    let ids = Some(Identity {
        uid: 65534,
        gid: 65534,
    });
    let kind = |uid, euid| refuse_setuid_drop(ids, uid, euid).map_err(|e| e.kind());
    assert_eq!(kind(1000, 0), Err(std::io::ErrorKind::PermissionDenied));
    assert_eq!(kind(0, 0), Ok(()));
    assert_eq!(kind(1000, 1000), Ok(()));
    assert_eq!(kind(0, 1000), Ok(()));
    // No drop, no matter who runs it.
    assert!(refuse_setuid_drop(None, 1000, 0).is_ok());
}

/// A command that writes `blocks` MiB to `file` with `dd`, held to `max` bytes in `dir`.
fn writes(dir: &std::path::Path, file: &str, blocks: u32, max: u64) -> Cmd {
    Cmd {
        write_cap: Some(WriteCap {
            dir: dir.to_path_buf(),
            max,
        }),
        ..within(
            "/usr/bin/dd",
            &[
                "if=/dev/zero",
                &format!("of={}", dir.join(file).display()),
                "bs=1048576",
                &format!("count={blocks}"),
            ],
            Duration::from_secs(60),
        )
    }
}

/// Catches output that is held in memory whole: a child that prints far more than the cap
/// still succeeds (it is not blocked on a full pipe), the caller gets the cap and one byte
/// more, which shows the output was longer, and a quiet or short child is untouched.
#[test]
fn output_past_its_cap_is_dropped_and_the_child_still_finishes() {
    let big = Cmd {
        stdout_max: Some(1000),
        ..within(
            "/usr/bin/head",
            &["-c", "50000000", "/dev/zero"],
            Duration::from_secs(60),
        )
    };
    let exec = SystemRunner.run(&big).unwrap();
    assert!(exec.success());
    assert_eq!(exec.stdout.len(), 1001);

    let short = Cmd {
        stdout_max: Some(1000),
        ..within(
            "/usr/bin/head",
            &["-c", "1000", "/dev/zero"],
            Duration::from_secs(60),
        )
    };
    assert_eq!(SystemRunner.run(&short).unwrap().stdout.len(), 1000);

    // Standard error is kept up to a fixed amount.
    let noisy = within(
        "/usr/bin/dd",
        &["if=/dev/zero", "of=/dev/stderr", "bs=1048576", "count=20"],
        Duration::from_secs(60),
    );
    let exec = SystemRunner.run(&noisy).unwrap();
    assert!(exec.success());
    assert_eq!(exec.stderr.len(), 1024 * 1024 + 1);
}

fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    std::fs::create_dir_all(&target).unwrap();
    kbf_testkit::dir::TestDir::under(target, name)
}

/// Catches a size cap that does not stop a command: one that would write far more than it
/// is allowed is killed while it runs, with a "too large" error, and writes no more after.
#[test]
fn a_child_that_writes_past_its_cap_is_killed() {
    let dir = scratch("kbf-runner-cap-kill");
    // 100000 MiB if nothing stopped it.
    let error = SystemRunner
        .run(&writes(&dir, "big", 100_000, 1024 * 1024))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge, "{error}");
    // The call returns once the child is killed and reaped, so nothing can write after it:
    // the file is the size it has now however long it is looked at.
    let size = std::fs::metadata(dir.join("big")).unwrap().len();
    assert!(size < 4 * 1024 * 1024 * 1024, "{size}");
    assert_eq!(std::fs::metadata(dir.join("big")).unwrap().len(), size);
    let output = format!("of={}", dir.join("big").display());
    assert_eq!(processes_with(&output), 0, "the writer was left running");
}

/// Catches a cap that a fast writer slips under: a command that is over the cap when it
/// exits is refused too, and one within it is not touched.
#[test]
fn a_child_over_its_cap_at_exit_is_refused_and_one_within_it_is_not() {
    let dir = scratch("kbf-runner-cap-exit");
    let error = SystemRunner
        .run(&writes(&dir, "over", 3, 1024 * 1024))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge, "{error}");
    let other = scratch("kbf-runner-cap-within");
    let ok = SystemRunner
        .run(&writes(&other, "within", 1, 3 * 1024 * 1024))
        .unwrap();
    assert!(ok.success(), "{}", ok.stderr_text());
}

/// Catches a cap that needs a time limit: without one the command is still watched, and a
/// directory that is not there holds nothing.
#[test]
fn a_cap_works_without_a_time_limit_and_over_a_missing_directory() {
    let dir = scratch("kbf-runner-cap-nolimit");
    let mut over = writes(&dir, "over", 3, 1024 * 1024);
    over.timeout = None;
    let error = SystemRunner.run(&over).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge, "{error}");
    let missing = Cmd {
        write_cap: Some(WriteCap {
            dir: dir.join("not-there"),
            max: 1,
        }),
        ..cmd("/usr/bin/true", &[])
    };
    assert!(SystemRunner.run(&missing).unwrap().success());
}

/// A drop with exact groups to uid 0 or gid 0 is refused, so the program never runs as root or
/// in group 0 (`wheel`) through it; the drop by `Command::uid` and `gid` is not covered by this
/// check, and no drop at all is not refused.
#[test]
fn an_exact_groups_drop_to_uid_or_gid_zero_is_refused() {
    use super::{Identity, drop_ids};
    use std::process::Command;
    for (uid, gid) in [(0, 20), (501, 0), (0, 0)] {
        let mut command = Command::new("/usr/bin/true");
        let kind = drop_ids(&mut command, Some(Identity { uid, gid }), true).map_err(|e| e.kind());
        assert_eq!(
            kind,
            Err(std::io::ErrorKind::PermissionDenied),
            "{uid} {gid}"
        );
        assert!(drop_ids(&mut command, Some(Identity { uid, gid }), false).is_ok());
    }
    let mut command = Command::new("/usr/bin/true");
    assert!(drop_ids(&mut command, None, true).is_ok());
    assert!(drop_ids(&mut command, Some(Identity { uid: 501, gid: 20 }), true).is_ok());
}

/// The words for how a command ended, from a fixed exit.
#[test]
fn ended_says_the_status_or_the_signal() {
    let exec = |code, signal| Exec {
        code,
        signal,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    assert_eq!(exec(Some(0), None).ended(), "exit status 0");
    assert_eq!(exec(None, Some(9)).ended(), "signal 9 (SIGKILL)");
    assert_eq!(exec(None, Some(13)).ended(), "signal 13 (SIGPIPE)");
    assert_eq!(exec(None, Some(11)).ended(), "signal 11 (SIGSEGV)");
    assert_eq!(exec(None, Some(63)).ended(), "signal 63 (unnamed)");
    assert_eq!(exec(None, None).ended(), "no exit status");
}

/// The variable that makes `helper_raise_sigterm_when_asked` end this test binary with SIGTERM.
const RAISE: &str = "KBF_RUNNER_TEST_RAISE_SIGTERM";

/// Not a test of its own: the program the next test runs. Without the variable it does
/// nothing; with it, the test binary sends itself SIGTERM and so ends by that signal.
#[test]
fn helper_raise_sigterm_when_asked() {
    if std::env::var_os(RAISE).is_some() {
        rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
            .expect("send SIGTERM to self");
        // The default action ends the process before this; a stop here would be a hang.
        std::process::abort();
    }
}

/// Catches a runner that does not record the signal that ended a real child (`signal` always
/// none): the test binary is run again, filtered to the helper above, with the variable set.
/// It ends itself by SIGTERM, and the result has that signal, no exit code, and the words
/// `fetch` uses in its reasons.
#[test]
fn a_real_child_that_kills_itself_reports_signal_15() {
    let mut c = cmd(
        std::env::current_exe().unwrap().to_str().unwrap(),
        &["helper_raise_sigterm_when_asked", "--test-threads=1"],
    );
    c.env = vec![(RAISE.into(), "1".into())];
    let exec = SystemRunner.run(&c).unwrap();
    assert_eq!(exec.signal, Some(15), "{}", exec.stderr_text());
    assert_eq!(exec.code, None);
    assert!(!exec.success());
    assert_eq!(exec.ended(), "signal 15 (SIGTERM)");
}
