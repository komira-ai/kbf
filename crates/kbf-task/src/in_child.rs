//! Test helper: runs a closure in a forked copy of the test process, for the parts of the child's
//! work that change the process they run in (a namespace, a filter, a process of its own).

/// What a child that panicked exits with.
pub const PANICKED: i32 = 99;

/// Runs `f` in a forked copy of this process and returns its exit code, or 128 plus the signal
/// that ended it. The child leaves through `exit`, which writes its coverage profile like any
/// process's.
pub fn in_child(f: impl FnOnce() -> i32) -> i32 {
    // SAFETY: the child runs `f` and leaves through `exit`; nothing else of the parent runs in
    // it. The tests are one process each (nextest) or tolerate the usual fork caveats of a
    // threaded parent for the few allocations `f` makes.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork");
    if pid == 0 {
        let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(PANICKED);
        // SAFETY: `exit` runs the handlers that write the coverage profile.
        unsafe { libc::exit(code) };
    }
    status_of(pid)
}

/// Waits for the child `pid` and returns its exit code, or 128 plus the signal that ended it.
pub fn status_of(pid: libc::pid_t) -> i32 {
    let pid = rustix::process::Pid::from_raw(pid).expect("a child's pid");
    let waited = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty());
    let (_, status) = waited.expect("waitpid").expect("a status");
    crate::child::exit_code(status)
}

/// The errno of `socket(domain)`, or 0.
pub fn socket(domain: libc::c_int) -> i32 {
    // SAFETY: integer arguments only.
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if fd >= 0 {
        // SAFETY: `fd` was opened above.
        unsafe { libc::close(fd) };
        0
    } else {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
    }
}

/// Whether this process is dumpable.
pub fn dumpable() -> i32 {
    // SAFETY: integer arguments only.
    unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) }
}
