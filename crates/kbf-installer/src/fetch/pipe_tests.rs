//! The parent's end of the fetch child, with `sh` scripts as the child: each plays one way a
//! source or a child misbehaves, and the parent's answer is read from the file it wrote and
//! the error it returned. The real child against a real TLS server is in
//! `tests/fetch_e2e.rs`.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::*;
use crate::fixtures::scratch_dir;

fn spec(script: &str) -> ChildSpec {
    ChildSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into(), "sh".into()],
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

/// Run `script` as the child into a fresh file under `name`'s scratch directory.
fn run(name: &str, script: &str, caps: Caps) -> (Result<Received, PipeFault>, Vec<u8>, Duration) {
    let dir = scratch_dir(&format!("pipe-{name}"));
    let path = dir.join("download");
    let mut out = File::create(&path).expect("file");
    let started = Instant::now();
    let result = receive(&spec(script), &[], None, caps, &mut out);
    (
        result,
        fs::read(&path).expect("the download"),
        started.elapsed(),
    )
}

/// A named pipe in `dir` that a child can report its pid through: the child may not write a
/// file (`RLIMIT_FSIZE` is 0, which does not apply to a pipe), and the test reads the pid from
/// the other end on a thread.
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
    // A killed child is reaped by the time `receive` returns: no such process.
    // SAFETY: kill with signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) != 0 }
}

/// The happy path: the bytes are written to the file exactly and counted, and the hash is
/// the hash of what was written, so that the digest the caller compares is of the file.
///
/// Catches a hash taken over the wrong bytes (a chunk skipped or counted twice) and a count
/// that is not the bytes written.
#[test]
fn the_bytes_are_written_counted_and_hashed() {
    let (got, file, _) = run("ok", "printf abcde", caps(5));
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
    let (got, file, took) = run("oversize", "exec yes", caps);
    assert!(matches!(got, Err(PipeFault::Oversize { size: 1000 })), "{got:?}");
    assert!(file.len() <= 1000, "{} bytes written", file.len());
    assert!(took < Duration::from_secs(10), "{took:?}");
}

/// The child that is killed is dead: its pid is gone once the fetch has returned.
///
/// Catches a child left running (a zombie, or a process still reading the source) after the
/// parent gave up.
#[test]
fn the_killed_child_is_gone() {
    let dir = scratch_dir("pipe-dead-pid");
    let (fifo, pid) = pid_pipe(&dir);
    let script = format!("echo $$ > {}; exec yes", fifo.display());
    let (got, _, _) = run("dead", &script, caps(1000));
    assert!(got.is_err());
    assert!(gone(pid.join().expect("the pid")));
}

/// A child that sends part of the bytes and then goes quiet is killed at the no-byte limit,
/// long before the whole-fetch limit.
///
/// Catches the quiet limit not being applied (the fetch would sit until the whole limit).
///
/// Mutant seen red: `quiet` left out of the deadline (`whole.min(quiet)` replaced by `whole`).
#[test]
fn a_stalled_child_is_killed_at_the_no_byte_limit() {
    let mut caps = caps(10);
    caps.no_byte = Duration::from_millis(700);
    caps.total = Duration::from_secs(30);
    let (got, file, took) = run("stall", "printf ab; exec sleep 600", caps);
    assert!(
        matches!(got, Err(PipeFault::Stalled { got: 2, size: 10, .. })),
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
    let script = "while true; do printf a; sleep 0.2; done";
    let (got, _, took) = run("trickle", script, caps);
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
        "echo 'source unreachable: boom' >&2; printf abc",
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

/// A child that fails after it delivered every byte is judged on the bytes: the exit status
/// is no input.
///
/// Catches the exit status being read (design 7.2: "judged on the bytes").
#[test]
fn a_child_that_fails_after_all_the_bytes_is_judged_on_them() {
    let (got, file, _) = run("late-fail", "printf abcde; exit 7", caps(5));
    assert_eq!(got.expect("received").count, 5);
    assert_eq!(file, b"abcde");
}

/// A child that delivered every byte and will not close its output is also judged on the
/// bytes, at the no-byte limit, and killed.
#[test]
fn a_child_that_will_not_close_after_all_the_bytes_is_judged_on_them() {
    let mut caps = caps(5);
    caps.no_byte = Duration::from_millis(500);
    let (got, file, _) = run("linger", "printf abcde; exec sleep 600", caps);
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
    let script = "head -c 1048576 /dev/zero | tr '\\0' e >&2; printf abcde";
    let mut caps = caps(5);
    caps.no_byte = Duration::from_secs(5);
    let (got, file, _) = run("chatty", script, caps);
    let got = got.expect("received");
    assert_eq!(file, b"abcde");
    assert_eq!(got.log.len(), LOG_BYTES);
}

/// What the child is: an empty environment, `/dev/null` for input, `/` as its directory,
/// no file it may write, no core, few descriptors, no new privileges, and none of the
/// descriptors the parent holds.
///
/// Catches a child that inherits the root parent's environment, a descriptor (the lent key,
/// the download file) or the ability to write files.
#[cfg(target_os = "linux")]
#[test]
fn the_child_is_confined() {
    // A descriptor with close-on-exec off, as one an earlier owner forgot to mark would be.
    let held = File::open("/etc/hostname")
        .or_else(|_| File::open("/proc/self/status"))
        .expect("a file");
    // SAFETY: dup of a descriptor this test owns.
    let leaked = unsafe { libc::dup(std::os::fd::AsRawFd::as_raw_fd(&held)) };
    assert!(leaked > 2);
    let script = "echo limit_fsize=$(ulimit -f); echo limit_core=$(ulimit -c); \
                  echo limit_nofile=$(ulimit -n); grep NoNewPrivs /proc/self/status; \
                  echo home=${HOME:-none}; \
                  echo stdin=$(readlink /proc/self/fd/0); echo cwd=$(readlink /proc/self/cwd); \
                  for f in /proc/self/fd/*; do echo fd=${f##*/}; done";
    assert!(std::env::var_os("HOME").is_some(), "the test needs a HOME to prove it is not passed on");
    let (_, file, _) = run("confined", script, caps(1_000_000));
    // SAFETY: closing the descriptor made above.
    unsafe { libc::close(leaked) };
    let said = String::from_utf8(file).expect("text");
    for want in [
        "limit_fsize=0\n",
        "limit_core=0\n",
        "limit_nofile=16\n",
        "NoNewPrivs:\t1\n",
        "home=none\n",
        "stdin=/dev/null\n",
        "cwd=/\n",
    ] {
        assert!(said.contains(want), "{want:?} not in {said}");
    }
    let fds: Vec<&str> = said.lines().filter_map(|l| l.strip_prefix("fd=")).collect();
    assert!(
        fds.iter().all(|fd| ["0", "1", "2", "3"].contains(fd)),
        "{fds:?}"
    );
    assert!(!fds.contains(&leaked.to_string().as_str()), "{fds:?}");
}

/// A program that cannot be started is its own error, not a short read.
#[test]
fn a_child_that_cannot_start_is_a_spawn_error() {
    let mut s = spec("true");
    s.program = PathBuf::from("/nonexistent/kbf-installer");
    let mut out = File::create(scratch_dir("pipe-nostart").join("f")).expect("file");
    let got = receive(&s, &[], None, caps(1), &mut out);
    assert!(matches!(got, Err(PipeFault::Spawn(_))), "{got:?}");
}

/// The child dies with the parent: a parent killed in the middle of a fetch leaves no child
/// reading the source. (The parent here is a throwaway process: this test binary, started
/// again with a marker in its arguments.)
#[cfg(target_os = "linux")]
#[test]
fn the_child_dies_with_its_parent() {
    if let Some(dir) = std::env::var_os("KBF_PIPE_PDEATH_DIR") {
        // In the throwaway parent: fetch from a child that never ends.
        let dir = PathBuf::from(dir);
        let script = format!("echo $$ > {}; exec sleep 600", dir.join("pid").display());
        let mut out = File::create(dir.join("download")).expect("file");
        let _ = receive(&spec(&script), &[], None, caps(10), &mut out);
        return;
    }
    let dir = scratch_dir("pipe-pdeath");
    let (_, pid) = pid_pipe(&dir);
    let mut parent = std::process::Command::new(std::env::current_exe().expect("this binary"))
        .args([
            "--exact",
            "fetch::pipe::tests::the_child_dies_with_its_parent",
            "--nocapture",
        ])
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
