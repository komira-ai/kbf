//! `reaped`: a guarded child does not outlive its guard, whatever it does with SIGTERM, and
//! holds only the descriptors it was given.

use std::os::fd::AsRawFd as _;
use std::process::Command;
use std::time::{Duration, Instant};

use kbf_testkit::reaped::{self, GRACE};

const ACT: &str = env!("CARGO_BIN_EXE_kbf-act");

/// `kill(pid, 0)` says there is no such process: it was reaped, not just killed.
fn gone(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Catches a guard that does nothing on drop (the child stays), one that kills but never
/// waits (a zombie answers `kill(pid, 0)` as alive), and one that waits for SIGTERM alone:
/// the child ignores it and is gone after the grace, not before it.
#[test]
fn a_child_that_ignores_sigterm_is_killed_and_reaped_on_drop() {
    let mut command = Command::new(ACT);
    // The flag says the signal is ignored already: a SIGTERM that came first would end it.
    let flag = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("reaped-ignoring");
    let _ = std::fs::remove_dir(&flag);
    command.args(["ignore-signal", "15", ";", "mkdir"]);
    command.arg(&flag).args([";", "sleep", "60"]);
    let child = reaped::spawn(&mut command, 3).expect("spawn");
    let pid = child.id().cast_signed();
    let waited = Instant::now();
    while !flag.exists() {
        assert!(
            waited.elapsed() < Duration::from_secs(10),
            "the child never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!gone(pid));
    let dropped = Instant::now();
    drop(child);
    assert!(gone(pid), "a child outlived its guard");
    assert!(
        dropped.elapsed() >= GRACE,
        "SIGKILL before the grace was over"
    );
}

/// Catches a guard that always waits out the grace: a child that exits on SIGTERM is
/// reaped at once. Also that a panic unwinding through the guard reaps (the `Drop` runs).
#[test]
fn a_child_that_stops_on_sigterm_is_reaped_at_once_even_in_a_panic() {
    let mut command = Command::new(ACT);
    command.args(["sleep", "60"]);
    let child = reaped::spawn(&mut command, 3).expect("spawn");
    let pid = child.id().cast_signed();
    let dropped = Instant::now();
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _held = child;
        panic!("a failed assertion");
    }));
    assert!(unwound.is_err());
    assert!(gone(pid));
    assert!(
        dropped.elapsed() < GRACE,
        "waited out the grace for a child that stops"
    );
}

/// Catches `cloexec_from` that marks the wrong range: a descriptor below the first stays
/// inheritable, one at it and above does not. The check runs in a child (the hooks place
/// descriptors 20 to 22, `spawn` marks from 21), so this process's descriptors, which other
/// tests open and close in parallel, are neither read nor marked.
#[cfg(target_os = "linux")]
#[test]
fn cloexec_from_marks_only_descriptors_from_the_first_up() {
    use std::os::unix::process::CommandExt as _;
    let mut command = Command::new(ACT);
    command.args(["sleep", "60"]);
    // SAFETY: the hook makes only async-signal-safe calls (`open`, `dup2`, `close`) and runs
    // in the forked child; `dup2` clears close-on-exec on the copy.
    unsafe {
        command.pre_exec(|| {
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
            if null < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in 20..=22 {
                if libc::dup2(null, fd) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if null < 20 {
                libc::close(null);
            }
            Ok(())
        });
    }
    let child = reaped::spawn(&mut command, 21).expect("spawn");
    // `spawn` returns after the exec: the child holds what survived it.
    let held: Vec<i32> = std::fs::read_dir(format!("/proc/{}/fd", child.id()))
        .expect("the child's descriptors")
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    assert!(held.contains(&20), "a descriptor below the first: {held:?}");
    assert!(
        !held.contains(&21) && !held.contains(&22),
        "the first and above: {held:?}"
    );
}

/// Catches `cloexec_from` that marks the wrong range, in this process: a descriptor below the
/// first stays inheritable, one at it and above does not. The descriptors are placed with
/// `F_DUPFD` from 900 and 1000, a range the other tests (which open a few low descriptors in
/// parallel) never reach, so the call marks nothing but these two.
#[test]
fn cloexec_from_marks_only_descriptors_from_the_first_up_in_process() {
    let null = std::fs::File::open("/dev/null").expect("open");
    let dup_from = |base: i32| {
        // SAFETY: duplicates a descriptor this test holds, without close-on-exec.
        let fd = unsafe { libc::fcntl(null.as_raw_fd(), libc::F_DUPFD, base) };
        assert!(fd >= base, "no descriptor from {base}");
        fd
    };
    let flags = |fd: i32| {
        // SAFETY: reading the flags of a descriptor this test holds.
        unsafe { libc::fcntl(fd, libc::F_GETFD) }
    };
    let (below, above) = (dup_from(900), dup_from(1000));
    reaped::cloexec_from(1000).expect("mark");
    assert_eq!(
        flags(below) & libc::FD_CLOEXEC,
        0,
        "a descriptor below the first"
    );
    assert_eq!(
        flags(above) & libc::FD_CLOEXEC,
        libc::FD_CLOEXEC,
        "the first and above"
    );
    // SAFETY: closing the two duplicates this test made.
    unsafe {
        libc::close(below);
        libc::close(above);
    }
}

/// Catches a child that inherits a descriptor the test left open for it to leak (the build
/// box's slot lock, on descriptor 9): no descriptor above 2 of the child's table leads to the
/// file the parent left inheritable.
#[cfg(target_os = "linux")]
#[test]
fn a_spawned_child_holds_no_descriptor_the_test_left_inheritable() {
    let leaked = std::fs::File::open("/dev/null").expect("open");
    // SAFETY: clearing the flag on a descriptor this test holds.
    unsafe { libc::fcntl(leaked.as_raw_fd(), libc::F_SETFD, 0) };
    let mut command = Command::new(ACT);
    command.args(["sleep", "60"]);
    let child = reaped::spawn(&mut command, 3).expect("spawn");
    let mut held: Vec<i32> = std::fs::read_dir(format!("/proc/{}/fd", child.id()))
        .expect("fds")
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    held.retain(|fd| *fd > 2);
    // Only the descriptors that lead to the leaked file count: a child still starting up
    // holds the dynamic loader's own descriptors (a library it maps) for a moment.
    held.retain(|fd| {
        std::fs::read_link(format!("/proc/{}/fd/{fd}", child.id()))
            .is_ok_and(|to| to == std::path::Path::new("/dev/null"))
    });
    assert!(
        held.is_empty(),
        "inherited {held:?}, leaked was {}",
        leaked.as_raw_fd()
    );
}

/// Catches a guard that signals a child it has already waited for (the pid may be reused by
/// then) or that cannot be waited through: a child the test reaped itself, through the
/// guard's `&mut Child`, is left alone on drop, and its status is the one the child exited
/// with.
#[test]
fn a_child_the_test_already_reaped_is_left_alone_on_drop() {
    let mut command = Command::new(ACT);
    command.args(["exit", "7"]);
    let mut child = reaped::spawn(&mut command, 3).expect("spawn");
    let status = child.wait().expect("wait through the guard");
    assert_eq!(status.code(), Some(7));
    let dropped = Instant::now();
    drop(child);
    assert!(
        dropped.elapsed() < GRACE,
        "waited out the grace for a child that had exited"
    );
}

/// Catches a `first_line` that waits for ever on a silent child (the test would hang until an
/// outer timeout killed the harness), and one that loses the line or the reader.
#[test]
fn a_silent_child_times_out_and_a_talking_one_gives_its_line() {
    let mut silent = Command::new("sleep");
    silent.arg("600").stdout(std::process::Stdio::piped());
    let mut silent = reaped::spawn(&mut silent, 3).expect("spawn");
    let began = Instant::now();
    let err = reaped::first_line(
        silent.stdout.take().expect("stdout"),
        Duration::from_millis(200),
    )
    .expect_err("silent");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert!(began.elapsed() < Duration::from_secs(5));

    let mut talking = Command::new("echo");
    talking.arg("hello").stdout(std::process::Stdio::piped());
    let mut talking = reaped::spawn(&mut talking, 3).expect("spawn");
    let (line, _reader) = reaped::first_line(
        talking.stdout.take().expect("stdout"),
        Duration::from_secs(10),
    )
    .expect("a line");
    assert_eq!(line, "hello\n");
}

/// The process is gone or a zombie waiting for its reaper (an orphan's reaper is init).
fn dead(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit(") ")
            .next()
            .is_some_and(|rest| rest.starts_with('Z')),
    }
}

/// The helper half of [`a_guarded_child_dies_when_its_parent_dies_without_unwinding`]: runs
/// only when started by it. Spawns a guarded `sleep`, writes its pid, and ends the process
/// with `exit`, which runs no destructor (as a kill of the harness would not).
#[test]
#[ignore = "started by a_guarded_child_dies_when_its_parent_dies_without_unwinding"]
fn orphaning_parent_helper() {
    let Some(file) = std::env::var_os("KBF_REAPED_PIDFILE") else {
        return;
    };
    let mut sleep = Command::new("sleep");
    sleep.arg("600");
    let child = reaped::spawn(&mut sleep, 3).expect("spawn");
    std::fs::write(&file, child.id().to_string()).expect("pid file");
    std::process::exit(0);
}

/// Catches a guard that covers only an orderly end: when the process that spawned the child
/// ends without running destructors (killed harness, `exit`), the child must go too, not run
/// on holding the build slots.
#[test]
fn a_guarded_child_dies_when_its_parent_dies_without_unwinding() {
    let file = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("reaped-orphan-pid");
    let _ = std::fs::remove_file(&file);
    let status = Command::new(std::env::current_exe().expect("this binary"))
        .args([
            "--ignored",
            "--exact",
            "orphaning_parent_helper",
            "--test-threads=1",
        ])
        .env("KBF_REAPED_PIDFILE", &file)
        .output()
        .expect("helper")
        .status;
    assert!(status.success());
    let pid: i32 = std::fs::read_to_string(&file)
        .expect("pid")
        .parse()
        .expect("number");
    let waited = Instant::now();
    while !dead(pid) {
        if waited.elapsed() > Duration::from_secs(10) {
            // SAFETY: ends the sleeper this test started, so a red run leaks nothing.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            panic!("the child outlived its parent");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
