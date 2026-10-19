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
/// inheritable, one at it and above does not.
#[test]
fn cloexec_from_marks_only_descriptors_from_the_first_up() {
    let below = std::fs::File::open("/dev/null").expect("open");
    let above = std::fs::File::open("/dev/null").expect("open");
    let flags = |fd: i32| {
        // SAFETY: reading the flags of a descriptor this test holds.
        unsafe { libc::fcntl(fd, libc::F_GETFD) }
    };
    let (below, above) = (below.as_raw_fd(), above.as_raw_fd());
    assert!(above > below);
    // SAFETY: clearing the flag on descriptors this test holds, as a tool that leaves
    // a lock open for its children does.
    unsafe {
        libc::fcntl(below, libc::F_SETFD, 0);
        libc::fcntl(above, libc::F_SETFD, 0);
    }
    reaped::cloexec_from(above).expect("mark");
    assert_eq!(
        flags(below) & libc::FD_CLOEXEC,
        0,
        "a descriptor below the first"
    );
    assert_eq!(
        flags(above) & libc::FD_CLOEXEC,
        libc::FD_CLOEXEC,
        "the first"
    );
}

/// Catches a child that inherits a descriptor the test left open for it to leak (the build
/// box's slot lock, on descriptor 9): the child's table has only 0 to 2, and the parent's
/// inheritable descriptor is not in it.
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
    assert!(
        held.is_empty(),
        "inherited {held:?}, leaked was {}",
        leaked.as_raw_fd()
    );
}
