//! The parent's end of the fetch child (`kbf_installer::fetch::pipe::receive`) with a stand-in
//! child (`tests/stub/fetch_stub.rs`) that plays one way a source or a child misbehaves per
//! test. The answer is read from the file the parent wrote and the error it returned. The real
//! child against real servers is in `tests/fetch_e2e.rs`.

use std::fs::{self, File};
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kbf_installer::fetch::identity::Identity;
use kbf_installer::fetch::pipe::{Caps, ChildSpec, LOG_BYTES, PipeFault, Received, receive};
use kbf_installer::run::Confine;
use sha2::{Digest, Sha256};

const STUB: &str = env!("CARGO_BIN_EXE_kbf-fetch-stub");

fn scratch(name: &str) -> PathBuf {
    kbf_host::testkit::scratch(&format!("fetch-pipe-{name}"))
}

fn spec(ops: &[&str]) -> ChildSpec {
    ChildSpec {
        program: PathBuf::from(STUB),
        args: ops.iter().map(Into::into).collect(),
        run_as: None,
        confine: Confine::FETCH,
    }
}

fn caps(size: u64) -> Caps {
    Caps {
        size,
        no_byte: Duration::from_secs(20),
        total: Duration::from_secs(40),
    }
}

/// Run the stub with `ops` into a fresh file; the result, what was written, how long it took.
fn run(name: &str, ops: &[&str], caps: Caps) -> (Result<Received, PipeFault>, Vec<u8>, Duration) {
    let dir = scratch(name);
    let path = dir.join("download");
    let mut out = File::create(&path).expect("file");
    let started = Instant::now();
    let result = receive(&spec(ops), &[], None, caps, &mut out);
    (
        result,
        fs::read(&path).expect("the download"),
        started.elapsed(),
    )
}

/// A named pipe that a child reports its pid through (the child may not write a file:
/// `RLIMIT_FSIZE` is 0, which does not apply to a pipe); the pid is read on a thread.
fn pid_pipe(dir: &Path) -> (PathBuf, std::thread::JoinHandle<libc::pid_t>) {
    let path = dir.join("pid");
    let c = std::ffi::CString::new(path.to_str().expect("utf8")).expect("c string");
    // SAFETY: mkfifo on a path this test just formed.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let reader = path.clone();
    let handle = std::thread::spawn(move || {
        fs::read_to_string(reader)
            .expect("the pid")
            .trim()
            .parse()
            .expect("a pid")
    });
    (path, handle)
}

fn gone(pid: libc::pid_t) -> bool {
    // SAFETY: kill with signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) != 0 }
}

/// The happy path: the bytes are written to the file exactly and counted, and the hash is the
/// hash of what was written, so that the digest the caller compares is of the file.
///
/// Catches a hash taken over the wrong bytes (a chunk skipped or counted twice) and a count
/// that is not the bytes written.
#[test]
fn the_bytes_are_written_counted_and_hashed() {
    let (got, file, _) = run("ok", &["emit:abcde"], caps(5));
    let got = got.expect("received");
    assert_eq!((got.count, file.as_slice()), (5, b"abcde".as_slice()));
    assert_eq!(got.sha256, hex::encode(Sha256::digest(b"abcde")));
}

/// A source that sends more than it declared: the child is killed and the fetch fails, and
/// never more than the declared bytes are written. The child here floods without end; the
/// fetch must end in moments, not at the time cap.
///
/// Catches the parent reading on past the declared size (the fetch would end at the whole
/// time cap with `TimedOut`, and the file would grow) and a child left running.
///
/// Mutant seen red: the `count > caps.size` return replaced by `continue`.
#[test]
fn a_source_that_sends_too_much_is_killed_and_refused() {
    let mut caps = caps(1000);
    caps.total = Duration::from_secs(30);
    let (got, file, took) = run("oversize", &["flood"], caps);
    assert!(
        matches!(got, Err(PipeFault::Oversize { size: 1000 })),
        "{got:?}"
    );
    assert!(file.len() <= 1000, "{} bytes written", file.len());
    assert!(took < Duration::from_secs(10), "{took:?}");
}

/// The child that is killed is dead: its pid is gone once the fetch has returned.
///
/// Catches a child left running (a zombie, or a process still reading the source) after the
/// parent gave up.
#[test]
fn the_killed_child_is_gone() {
    let dir = scratch("dead-pid");
    let (fifo, pid) = pid_pipe(&dir);
    let pid_op = format!("pid:{}", fifo.display());
    let (got, _, _) = run("dead", &[&pid_op, "flood"], caps(1000));
    assert!(got.is_err());
    assert!(gone(pid.join().expect("the pid")));
}

/// A child that sends part of the bytes and then goes quiet is killed at the no-byte limit,
/// long before the whole-fetch limit.
///
/// Catches the quiet limit not being applied (the fetch would sit until the whole limit).
///
/// Mutant seen red: `quiet` stretched a thousandfold in the pipe's deadline.
#[test]
fn a_stalled_child_is_killed_at_the_no_byte_limit() {
    let mut caps = caps(10);
    caps.no_byte = Duration::from_millis(700);
    caps.total = Duration::from_secs(30);
    let (got, file, took) = run("stall", &["emit:ab", "sleep:600"], caps);
    assert!(
        matches!(
            got,
            Err(PipeFault::Stalled {
                got: 2,
                size: 10,
                ..
            })
        ),
        "{got:?}"
    );
    assert_eq!(file, b"ab");
    assert!(took < Duration::from_secs(10), "{took:?}");
}

/// A child that keeps sending, slowly, is stopped by the whole-fetch limit.
///
/// Catches the whole limit missing: a source trickling a byte at a time would hold the
/// installer for as long as it liked.
#[test]
fn a_trickling_child_is_stopped_by_the_whole_time_limit() {
    let mut caps = caps(1_000_000);
    caps.total = Duration::from_millis(1500);
    let (got, _, took) = run("trickle", &["trickle"], caps);
    assert!(matches!(got, Err(PipeFault::TimedOut { .. })), "{got:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
}

/// A child that ends before the declared bytes is a failed fetch, whatever its status, and
/// what it said on standard error is in the error for the operator.
///
/// Catches a short body accepted, and a log lost.
#[test]
fn a_short_body_is_a_failed_fetch_with_the_childs_words() {
    let (got, _, _) = run(
        "short",
        &["err:source unreachable: boom\n", "emit:abc"],
        caps(10),
    );
    match got {
        Err(PipeFault::Short { got, size, log }) => {
            assert_eq!((got, size), (3, 10));
            assert_eq!(log, "source unreachable: boom");
        }
        other => panic!("{other:?}"),
    }
}

/// A child that fails after it delivered every byte is judged on the bytes: the exit status is
/// no input.
///
/// Catches the exit status being read (design 7.2: "judged on the bytes").
#[test]
fn a_child_that_fails_after_all_the_bytes_is_judged_on_them() {
    let (got, file, _) = run("late-fail", &["emit:abcde", "exit:7"], caps(5));
    assert_eq!(got.expect("received").count, 5);
    assert_eq!(file, b"abcde");
}

/// A child that delivered every byte and will not close its output is also judged on the
/// bytes, at the no-byte limit, and killed.
#[test]
fn a_child_that_will_not_close_after_all_the_bytes_is_judged_on_them() {
    let mut caps = caps(5);
    caps.no_byte = Duration::from_millis(500);
    let (got, file, _) = run("linger", &["emit:abcde", "sleep:600"], caps);
    assert_eq!(got.expect("received").count, 5);
    assert_eq!(file, b"abcde");
}

/// A chatty child's standard error is drained, only the first 4 KiB are kept, and its bytes
/// still arrive: a pipe nobody reads would block it at 64 KiB.
///
/// Catches standard error not being read (the child stalls, the fetch fails), and a log
/// without a bound.
///
/// Mutant seen red: the `capture` of standard error left out.
#[test]
fn standard_error_is_drained_and_kept_to_four_kib() {
    let mut caps = caps(5);
    caps.no_byte = Duration::from_secs(5);
    let (got, file, _) = run("chatty", &["errflood:1048576", "emit:abcde"], caps);
    let got = got.expect("received");
    assert_eq!(file, b"abcde");
    assert_eq!(got.log.len(), LOG_BYTES);
}

/// What the child is: an empty environment, `/dev/null` for input, `/` as its directory, no
/// file it may write, no core, few descriptors, no new privileges, and none of the descriptors
/// the parent holds.
///
/// Catches a child that inherits the root parent's environment, a descriptor (the lent key,
/// the download file) or the ability to write files.
///
/// Mutants seen red: each of `RLIMIT_FSIZE`, `RLIMIT_NOFILE` and `no_new_privs` left out.
#[cfg(target_os = "linux")]
#[test]
fn the_child_is_confined() {
    // A descriptor with close-on-exec off, as one an earlier owner forgot to mark would be.
    let held = File::open("/proc/self/status").expect("a file");
    // SAFETY: dup of a descriptor this test owns (dup leaves close-on-exec off, on purpose).
    let leaked = unsafe { libc::dup(held.as_raw_fd()) };
    assert!(leaked > 2);
    assert!(
        std::env::var_os("PATH").is_some(),
        "the test needs an environment to prove it is not passed on"
    );
    let (_, file, _) = run("confined", &["report"], caps(1_000_000));
    // SAFETY: closing the descriptor made above.
    unsafe { libc::close(leaked) };
    let said = String::from_utf8(file).expect("text");
    for want in [
        "fsize=0/0/0\n",
        "core=0/0/0\n",
        "nofile=16/16/0\n",
        "nonewprivs=1\n",
        "env=0\n",
        "stdin=/dev/null\n",
        "cwd=/\n",
    ] {
        assert!(said.contains(want), "{want:?} not in {said}");
    }
    let fds: Vec<&str> = said.lines().filter_map(|l| l.strip_prefix("fd=")).collect();
    assert_eq!(fds, ["0", "1", "2"], "{said}");
}

/// A program that cannot be started is its own error, not a short read.
#[test]
fn a_child_that_cannot_start_is_a_spawn_error() {
    let mut s = spec(&[]);
    s.program = PathBuf::from("/nonexistent/kbf-installer");
    let mut out = File::create(scratch("nostart").join("f")).expect("file");
    let got = receive(&s, &[], None, caps(1), &mut out);
    assert!(matches!(got, Err(PipeFault::Spawn(_))), "{got:?}");
}

/// The child dies with the parent: a parent killed in the middle of a fetch leaves no child
/// reading the source. (The parent here is a throwaway process: this test binary, started
/// again with a marker in its environment.)
///
/// Mutant seen red: `PR_SET_PDEATHSIG` left out.
#[cfg(target_os = "linux")]
#[test]
fn the_child_dies_with_its_parent() {
    if let Some(dir) = std::env::var_os("KBF_PIPE_PDEATH_DIR") {
        // In the throwaway parent: fetch from a child that never ends.
        let dir = PathBuf::from(dir);
        let pid_op = format!("pid:{}", dir.join("pid").display());
        let mut out = File::create(dir.join("download")).expect("file");
        let _ = receive(
            &spec(&[&pid_op, "sleep:600"]),
            &[],
            None,
            caps(10),
            &mut out,
        );
        return;
    }
    let dir = scratch("pdeath");
    let (_, pid) = pid_pipe(&dir);
    let mut parent = std::process::Command::new(std::env::current_exe().expect("this binary"))
        .args(["--exact", "the_child_dies_with_its_parent", "--nocapture"])
        .env("KBF_PIPE_PDEATH_DIR", &dir)
        .spawn()
        .expect("the parent starts");
    // The pid arrives when the child has started and written it.
    let pid = pid.join().expect("the pid");
    std::thread::sleep(Duration::from_millis(200));
    parent.kill().expect("kill the parent");
    parent.wait().expect("reaped");
    let until = Instant::now() + Duration::from_secs(10);
    while !gone(pid) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(gone(pid), "the child outlived its parent");
}

/// What the child is lent is the identity and nothing else: with a lent descriptor the child
/// holds 0, 1, 2 and that one (until it closes it), and reads from its front what the parent wrote; a descriptor the
/// parent merely holds open is not there.
///
/// Catches a lent descriptor that is closed on exec (the child gets nothing), one positioned
/// at its end, and a leaked one.
///
/// Mutant seen red: the `fcntl(fd, F_SETFD, 0)` in `before_exec_confined` removed.
#[cfg(target_os = "linux")]
#[test]
fn the_lent_descriptor_reaches_the_child_and_no_other_does() {
    let id = Identity {
        cert: b"cert".to_vec(),
        key: b"key".to_vec(),
        ca: b"ca".to_vec(),
    };
    let lent = id.lend().expect("lends");
    let n = lent.as_raw_fd();
    let held = File::open("/proc/self/status").expect("a file");
    // SAFETY: dup of a descriptor this test owns, left inheritable on purpose.
    let leaked = unsafe { libc::dup(held.as_raw_fd()) };
    let read = format!("readfd:{n}");
    let dir = scratch("lend");
    let mut out = File::create(dir.join("download")).expect("file");
    let got = receive(
        &spec(&[&read, "report"]),
        &[],
        Some(lent.as_fd()),
        caps(1_000_000),
        &mut out,
    );
    // SAFETY: closing the descriptor made above.
    unsafe { libc::close(leaked) };
    // The declared size is not the point here (the stub sends what it sends): the file is.
    assert!(
        matches!(got, Ok(_) | Err(PipeFault::Short { .. })),
        "{got:?}"
    );
    let said = fs::read(dir.join("download")).expect("the download");
    let (blob, report) = said.split_at(id.encode().len());
    assert_eq!(Identity::decode(blob).expect("the blob"), id);
    let report = String::from_utf8(report.to_vec()).expect("text");
    let fds: Vec<&str> = report
        .lines()
        .filter_map(|l| l.strip_prefix("fd="))
        .collect();
    // The stub closed the lent descriptor when it had read it; nothing else was there.
    assert_eq!(fds, ["0", "1", "2"], "{report}");
}
