//! `kbf-act`, run as a test payload runs it: what each step prints or writes, the exit
//! status it leaves, and the process-level effects (signals, children, sessions,
//! memory) that only a separate process shows.

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const ACT: &str = env!("CARGO_BIN_EXE_kbf-act");

fn act(args: &[&str]) -> Command {
    let mut command = Command::new(ACT);
    command.args(args);
    command
}

fn output(args: &[&str]) -> Output {
    act(args).output().expect("kbf-act runs")
}

/// A fresh directory for one test, under Cargo's per-target temporary directory.
fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("old dir");
    }
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Waits up to 10 s for `done`.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < until, "never: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Catches: `exit` leaving another status (a test that waits for exit 3 would pass
/// on a crash); `echo` on the wrong stream or with its words not joined by single
/// spaces; `env` printing a default over a set value, or nothing for an unset one;
/// `pwd` other than the working directory; `id` other than the real user id; a step
/// after `exit` run anyway.
#[test]
fn steps_print_what_they_say_and_exit_sets_the_status() {
    let dir = dir("act-print");
    let after = dir.join("after");
    let uid = std::fs::metadata(&dir).expect("dir").uid();
    let out = act(&[
        "echo",
        "out",
        "two",
        ";",
        "echo",
        "--stderr",
        "err",
        ";",
        "env",
        "KBF_UNSET",
        "unset",
        ";",
        "env",
        "KBF_SET",
        ";",
        "env",
        "KBF_SET",
        "default",
        ";",
        "pwd",
        ";",
        "id",
        ";",
        "exit",
        "3",
        ";",
        "append",
        &after.display().to_string(),
        "x",
    ])
    .current_dir(&dir)
    .env("KBF_SET", "value")
    .env_remove("KBF_UNSET")
    .output()
    .expect("runs");
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let real = std::fs::canonicalize(&dir).expect("real");
    assert_eq!(
        text(&out.stdout),
        format!("out two\nunset\nvalue\nvalue\n{}\n{uid}\n", real.display())
    );
    assert_eq!(text(&out.stderr), "err\n");
    assert!(!after.exists(), "a step after exit ran");
}

/// Catches: a run without `exit` leaving other than 0; `append` truncating instead of
/// appending, or dropping the newline; `upper` not applied or written elsewhere.
#[test]
fn files_are_appended_and_upper_cased() {
    let dir = dir("act-files");
    let (log, input, upper) = (dir.join("log"), dir.join("in"), dir.join("upper"));
    let log = log.display().to_string();
    std::fs::write(&input, "two\nThree\n").expect("input");
    let upper_path = upper.display().to_string();
    let input = input.display().to_string();
    for word in ["a b", "c"] {
        let mut args = vec!["append", &log];
        args.extend(word.split(' '));
        args.extend([";", "upper", &upper_path, &input]);
        let out = output(&args);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
    }
    assert_eq!(std::fs::read_to_string(&log).expect("log"), "a b\nc\n");
    assert_eq!(
        std::fs::read_to_string(&upper).expect("upper"),
        "TWO\nTHREE\n"
    );
}

/// Catches: `rename` copying (the old name left) or not replacing; `symlink` with its
/// target and link swapped, or a copy instead of a link; `chmod` setting other bits
/// than asked (a test of a directory nobody may write would pass on a writable one).
#[test]
fn paths_are_renamed_linked_and_chmodded() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = dir("act-paths");
    let (from, to, link) = (dir.join("from"), dir.join("to"), dir.join("link"));
    std::fs::write(&from, "moved").expect("from");
    std::fs::write(&to, "replaced").expect("to");
    let [from_arg, to_arg, link_arg] = [&from, &to, &link].map(|p| p.display().to_string());
    let out = output(&[
        "rename", &from_arg, &to_arg, ";", "symlink", "to", &link_arg, ";", "chmod", "640", &to_arg,
    ]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(!from.exists(), "the old name is left");
    assert_eq!(std::fs::read_to_string(&to).expect("to"), "moved");
    assert_eq!(
        std::fs::read_link(&link).expect("a link"),
        Path::new("to"),
        "the link's target"
    );
    let mode = std::fs::metadata(&to).expect("to").permissions().mode();
    assert_eq!(mode & 0o7777, 0o640, "{mode:o}");
}

/// Catches: `wait-for` returning before the path exists (a test gated on it would go
/// on at once), waiting on although it is there, or never giving up (a red run of a
/// test gated on it would hang instead of failing).
#[test]
fn wait_for_returns_when_the_path_appears_and_gives_up_in_time() {
    let dir = dir("act-wait-for");
    let gate = dir.join("gate");
    let gate_arg = gate.display().to_string();
    let started = Instant::now();
    let mut waiting = act(&["wait-for", &gate_arg, "10", ";", "echo", "through"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("runs");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        waiting.try_wait().expect("status").is_none(),
        "went on before the path existed"
    );
    std::fs::write(&gate, "").expect("gate");
    let out = waiting.wait_with_output().expect("exits");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), "through\n");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    let out = output(&["wait-for", &gate_arg, "0", ";", "echo", "there"]);
    assert_eq!(text(&out.stdout), "there\n", "{out:?}");

    let started = Instant::now();
    let missing = dir.join("missing").display().to_string();
    let out = output(&["wait-for", &missing, "0.3"]);
    let took = started.elapsed();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(took >= Duration::from_millis(300), "{took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
}

/// Catches: `act::main` answering only under the name `kbf-act` (another crate's bin
/// has a name of its own, and would run nothing), or differently from the hook.
#[test]
fn main_answers_under_any_name() {
    use std::os::unix::process::CommandExt as _;
    let out = Command::new(ACT)
        .arg0("kbf-other-act")
        .args(["echo", "from", "main", ";", "exit", "4"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    assert_eq!(text(&out.stdout), "from main\n");
}

/// Catches: a failed step exiting 0 or 2 (taken for success or for a typo); a
/// malformed step found only when reached, after earlier steps ran; a failure with
/// nothing on stderr; an argument that is not UTF-8 accepted.
#[test]
fn failures_exit_1_and_usage_errors_exit_2_before_any_step() {
    use std::os::unix::ffi::OsStrExt as _;
    let dir = dir("act-failures");
    let early = dir.join("early").display().to_string();
    let missing = dir.join("missing").display().to_string();
    let no_dir = dir.join("no/such/file").display().to_string();
    let taken = dir.join("taken");
    std::fs::write(&taken, "").expect("taken");
    let taken = taken.display().to_string();
    let cases: [(&[&str], i32, &str); 12] = [
        (&["rename", &missing, &early], 1, "rename "),
        (&["symlink", &missing, &taken], 1, "link "),
        (&["chmod", "644", &missing], 1, "chmod "),
        (&["wait-for", &missing, "0"], 1, "not there after"),
        (&["env", "KBF_NEVER_SET"], 1, "KBF_NEVER_SET is not set"),
        (&["upper", &early, &missing], 1, "read "),
        (&["upper", &no_dir, ACT], 1, "create "),
        (&["append", &no_dir, "x"], 1, "append to "),
        (&["echo", "x"], 1, "write: "),
        (&["append", &early, "x", ";", "nope"], 2, "bad step"),
        (&["sleep", "soon"], 2, "not a number of seconds"),
        (&[], 2, "usage"),
    ];
    for (args, code, said) in cases {
        // macOS has no /dev/full.
        if args == ["echo", "x"] && !cfg!(target_os = "linux") {
            continue;
        }
        let mut command = act(args);
        command.env_remove("KBF_NEVER_SET");
        if args == ["echo", "x"] {
            command.stdout(std::fs::File::create("/dev/full").expect("/dev/full"));
        }
        let out = command.output().expect("runs");
        assert_eq!(out.status.code(), Some(code), "{args:?}: {out:?}");
        assert!(text(&out.stderr).contains(said), "{args:?}: {out:?}");
        assert!(!Path::new(&early).exists(), "{args:?}: a step ran");
    }
    let out = Command::new(ACT)
        .arg(std::ffi::OsStr::from_bytes(b"echo\xff"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(text(&out.stderr).contains("is not UTF-8"), "{out:?}");
}

/// Catches: `kill-self` sending another signal, or the program exiting by itself
/// instead of being killed (a test of how kbf reports a signalled action would see an
/// exit status); a signal the process ignores taken for a kill.
#[test]
fn kill_self_dies_by_the_signal() {
    let out = output(&["kill-self", "9", ";", "exit", "0"]);
    assert_eq!(out.status.signal(), Some(9), "{out:?}");
    let out = output(&["kill-self", "15"]);
    assert_eq!(out.status.signal(), Some(15), "{out:?}");
    // SIGWINCH (28 on Linux and macOS) is ignored by default.
    let out = output(&["kill-self", "28", ";", "exit", "0"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(text(&out.stderr).contains("still running after signal 28"));
}

/// Catches: `sleep` not sleeping, or for whole seconds only.
#[test]
fn sleep_takes_its_time() {
    let started = Instant::now();
    let out = output(&["sleep", "0.3"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(300), "{took:?}");
    assert!(took < Duration::from_secs(1), "{took:?}");
}

/// Catches: `spawn-child` waiting for its child (a test of output held open past a
/// process's exit would see none), the child not given the same stdout, and the
/// child not running its step.
#[test]
fn spawn_child_runs_on_holding_stdout() {
    let dir = dir("act-spawn");
    let mark = dir.join("mark").display().to_string();
    let mut parent = act(&[
        "spawn-child",
        "append",
        &mark,
        "child",
        ";",
        "spawn-child",
        "sleep",
        "1",
        ";",
        "echo",
        "parent",
    ])
    .stdout(Stdio::piped())
    .spawn()
    .expect("runs");
    let started = Instant::now();
    let status = parent.wait().expect("parent exits");
    assert!(status.success(), "{status}");
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "the parent waited for its child: {:?}",
        started.elapsed()
    );
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut parent.stdout.take().expect("stdout"), &mut stdout)
        .expect("read");
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the child did not hold stdout: {:?}",
        started.elapsed()
    );
    assert_eq!(stdout, "parent\n");
    eventually("the child ran its step", || {
        std::fs::read_to_string(&mark).is_ok_and(|m| m == "child\n")
    });
}

/// Catches: `setsid` not making a new session (a test of a process that escapes its
/// group would test nothing), and a second `setsid` that cannot work reported as done.
#[test]
fn setsid_leads_a_new_session() {
    let mut child = act(&["setsid", ";", "sleep", "3"]).spawn().expect("runs");
    let pid = rustix::process::Pid::from_child(&child);
    eventually("the child leads its own session", || {
        rustix::process::getsid(Some(pid)).is_ok_and(|sid| sid == pid)
    });
    // Let it exit by itself: a killed process writes no coverage data.
    let status = child.wait().expect("reaped");
    assert!(status.success(), "{status}");
    let out = output(&["setsid", ";", "setsid"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(text(&out.stderr).contains("setsid: "), "{out:?}");
}

/// Catches: `alloc` that allocates without touching (the pages stay unmapped and a
/// memory-limit test never reaches its limit), or frees before the next step.
#[cfg(target_os = "linux")]
#[test]
fn alloc_holds_resident_memory() {
    let mut child = act(&["alloc", "64", ";", "sleep", "3"])
        .spawn()
        .expect("runs");
    let status = format!("/proc/{}/status", child.id());
    eventually("64 MiB resident", || {
        std::fs::read_to_string(&status)
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("VmRSS:"))
                    .and_then(|kb| kb.trim().trim_end_matches(" kB").parse::<u64>().ok())
            })
            .is_some_and(|kb| kb >= 64 << 10)
    });
    // Let it exit by itself: a killed process writes no coverage data.
    let status = child.wait().expect("reaped");
    assert!(status.success(), "{status}");
}
