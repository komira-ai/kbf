//! The container's first process (`init/kbf_init.rs`), run on the host.
//!
//! A host is not a PID namespace, but the init makes itself the subreaper of its
//! descendants, so what it does for a container's orphans it does here: an orphan of
//! the program is its child. `tests/oci_crun.rs` runs it in a real container.
//!
//! The programs it runs are `kbf-act` steps (this test binary through a link). Each
//! test says the defect it would catch; the reaping test goes red when the init stops
//! reaping (the mutant: `waitpid(-1, ..)` made `waitpid(program, ..)` in `reap`).

kbf_testkit::act_test_hook!();

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};

/// The init this build embeds.
const INIT: &str = env!("KBF_INIT_BINARY");

/// A scratch directory holding a link to this binary named `kbf-act`.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps directory")
        .join("kbf-driver-container-unit")
        .join(format!("init-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    kbf_testkit::act::link_self(&dir.join(kbf_testkit::act::NAME)).expect("link kbf-act");
    dir
}

/// The init, in `dir`, running `program` and `args` with the environment empty but for
/// `PATH` (if any); its stdin a file `report` in `dir`, holding `stdin` to begin with.
fn init(dir: &Path, path: Option<&Path>, stdin: &str, program: &str, args: &[&str]) -> Command {
    std::fs::write(dir.join("report"), stdin).expect("report file");
    let report = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("report"))
        .expect("open the report file");
    let mut command = Command::new(INIT);
    command
        .arg("--")
        .arg(program)
        .args(args)
        .env_clear()
        .current_dir(dir)
        .stdin(report)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command
}

/// What running the init to its end gave.
struct Run {
    code: i32,
    report: String,
    stdout: String,
}

fn finish(dir: &Path, command: &mut Command) -> Run {
    let out = command.output().expect("run the init");
    Run {
        code: out.status.code().unwrap_or(-1),
        report: std::fs::read_to_string(dir.join("report")).unwrap_or_default(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    }
}

/// Runs the act `steps` under the init, by the link's path.
fn act(dir: &Path, steps: &[&str]) -> Run {
    let link = dir.join("kbf-act");
    finish(
        dir,
        &mut init(dir, None, "", link.to_str().expect("utf-8"), steps),
    )
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn signal(child: &Child, signal: Signal) {
    let pid = Pid::from_raw(i32::try_from(child.id()).expect("pid")).expect("pid");
    rustix::process::kill_process(pid, signal).expect("signal the init");
}

/// Catches the exit code of the program not being the init's: a code is kept as it is
/// (0, a small one, 255), and a program killed by signal `n` is `128 + n`, as `crun`
/// reported it when the program was the container's first process.
#[test]
fn the_exit_status_of_the_program_is_the_inits() {
    let dir = scratch("status");
    for code in ["0", "7", "255"] {
        assert_eq!(
            act(&dir, &["exit", code]).code,
            code.parse::<i32>().unwrap()
        );
    }
    assert_eq!(act(&dir, &["kill-self", "9"]).code, 137);
    assert_eq!(act(&dir, &["kill-self", "15"]).code, 143);
}

/// Catches an orphan staying a zombie (the mopm defect: a process nobody waited for is
/// still there for `kill(pid, 0)`): the program starts a process whose parent exits at
/// once, so the init adopts it; it ends 0.2 s later; while the program is still running,
/// the process must be gone from `/proc` altogether (a zombie is not).
#[test]
fn an_orphan_that_has_ended_is_reaped() {
    let dir = scratch("orphan");
    let link = dir.join("kbf-act");
    let mut running = init(
        &dir,
        None,
        "",
        link.to_str().expect("utf-8"),
        &[
            "run",
            link.to_str().expect("utf-8"),
            "spawn-child",
            "--pid",
            "orphan.pid",
            "sleep",
            "0.2",
            ";",
            "sleep",
            "20",
        ],
    )
    .spawn()
    .expect("init");
    wait_for(&dir.join("orphan.pid"));
    let pid = std::fs::read_to_string(dir.join("orphan.pid"))
        .expect("pid")
        .trim()
        .to_owned();
    let proc = PathBuf::from("/proc").join(&pid);
    let deadline = Instant::now() + Duration::from_secs(5);
    while proc.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let left = std::fs::read_to_string(proc.join("stat")).ok();
    signal(&running, Signal::TERM);
    running.wait().expect("wait");
    assert_eq!(left, None, "the orphan {pid} outlived its end, unreaped");
}

/// Catches a signal sent to the init not reaching the program (the init is the
/// container's first process, so the driver's `crun kill` lands on it): the program is
/// ended by the signal as it would be itself, and the init answers `128 + n`.
#[test]
fn a_signal_to_the_init_is_the_programs() {
    for (name, signum, code) in [(Signal::TERM, "TERM", 143), (Signal::HUP, "HUP", 129)] {
        let dir = scratch(&format!("signal-{signum}"));
        let link = dir.join("kbf-act");
        let mut running = init(
            &dir,
            None,
            "",
            link.to_str().expect("utf-8"),
            &["append", "ready", "x", ";", "sleep", "30"],
        )
        .spawn()
        .expect("init");
        wait_for(&dir.join("ready"));
        signal(&running, name);
        assert_eq!(running.wait().expect("wait").code(), Some(code), "{signum}");
    }
}

/// Catches the program being looked up other than as `crun` looks it up: a bare name on
/// the environment's `PATH` only (no `PATH` is not found, though `env` is on a host's), a
/// path as it is, relative from the working directory; the report in `crun`'s words and
/// the shell's exit codes (127 not found, 126 found but not executable).
#[test]
fn a_program_that_cannot_start_is_reported_in_cruns_words() {
    let dir = scratch("refused");
    std::fs::write(dir.join("data"), b"not a program").expect("data");
    let refused =
        |path: Option<&Path>, program: &str| finish(&dir, &mut init(&dir, path, "", program, &[]));

    let no_path = refused(None, "env");
    assert_eq!(no_path.code, 127);
    assert_eq!(
        no_path.report,
        "executable file `env` not found in $PATH: No such file or directory\n"
    );
    let missing = refused(Some(&dir), "no-such-tool");
    assert_eq!(missing.code, 127);
    assert!(
        missing.report.contains("`no-such-tool` not found"),
        "{}",
        missing.report
    );
    assert_eq!(refused(None, "/no/such/tool").code, 127);
    let data = refused(None, "./data");
    assert_eq!(data.code, 126);
    assert!(
        data.report.starts_with("open executable: "),
        "{}",
        data.report
    );
    assert_eq!(refused(None, "/").code, 126);

    // Found on the PATH, behind a directory that has no such file: it runs, and the
    // report is untouched.
    let path = std::env::join_paths([Path::new("/nonexistent"), dir.as_path()]).expect("path");
    let found = finish(
        &dir,
        &mut init(
            &dir,
            Some(Path::new(&path)),
            "",
            "kbf-act",
            &["echo", "found"],
        ),
    );
    assert_eq!(found.code, 0);
    assert_eq!(found.stdout, "found\n");
    assert_eq!(found.report, "");
}

/// Catches the program inheriting the init's stdin, which is the driver's file for the
/// report (the program could read or write it): the report file holds a marker, and the
/// program reading its stdin gets nothing of it.
#[test]
fn the_program_has_no_stdin_of_the_inits() {
    let dir = scratch("stdin");
    let link = dir.join("kbf-act");
    let ran = finish(
        &dir,
        &mut init(
            &dir,
            None,
            "marker\n",
            link.to_str().expect("utf-8"),
            &["cat", "/proc/self/fd/0"],
        ),
    );
    assert_eq!(ran.code, 0, "{}", ran.report);
    assert_eq!(ran.stdout, "", "the program read the init's stdin");
}

/// Catches the report file being reachable by the program, which runs as the same user:
/// through `/proc/<init>/fd/0` it could write words of its own for the driver to read as
/// the init's (a lookup failure it did not have, a farm failure it did not cause). A
/// process that is not dumpable has its `/proc` entries closed to its own user; this
/// opens the init's stdin, as the program could, and must be refused. (A superuser has
/// the capability that opens it, so the check does not apply to one.)
#[test]
fn the_program_cannot_open_the_inits_stdin() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let dir = scratch("dumpable");
    let link = dir.join("kbf-act");
    let mut running = init(
        &dir,
        None,
        "",
        link.to_str().expect("utf-8"),
        &["append", "ready", "x", ";", "sleep", "30"],
    )
    .spawn()
    .expect("init");
    wait_for(&dir.join("ready"));
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/proc/{}/fd/0", running.id()));
    signal(&running, Signal::KILL);
    running.wait().expect("wait");
    let denied = opened.expect_err("the init's stdin was opened by its own user");
    assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
}

/// Catches no usage check: without `--` and a program the init exits 2 instead of
/// running something.
#[test]
fn a_call_without_a_program_is_a_usage_error() {
    for args in [&[][..], &["--"], &["/bin/true"]] {
        let status = Command::new(INIT)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("init");
        assert_eq!(status.code(), Some(2), "{args:?}");
    }
}
