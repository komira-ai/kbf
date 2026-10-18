//! A stand-in for the fetch child, for the tests of the parent's end of the pipe
//! (`tests/fetch_pipe.rs`). Each argument is one operation, done in order:
//!
//! - `emit:TEXT` write TEXT to standard output; `err:TEXT` to standard error;
//! - `flood` write to standard output until it breaks; `trickle` one byte every 200 ms;
//! - `errflood:N` write N bytes to standard error;
//! - `sleep:SECS` wait; `exit:CODE` end with that status;
//! - `pid:FIFO` write this process's id to the named pipe (a file would break `RLIMIT_FSIZE`);
//! - `readfd:N` copy descriptor N to standard output;
//! - `report` print the limits and descriptors this process was started with.
//!
//! Built only with the `test-sandbox` feature: it is never part of a release.

use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::time::Duration;

fn limit(resource: libc::c_int) -> String {
    let mut got = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes the struct it is given.
    let rc = unsafe { libc::getrlimit(resource as _, &raw mut got) };
    format!("{}/{}/{rc}", got.rlim_cur, got.rlim_max)
}

fn open_descriptors() -> Vec<i32> {
    let mut listed: Vec<i32> = std::fs::read_dir("/proc/self/fd")
        .expect("the descriptor directory")
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    // SAFETY: F_GETFD on a number only reports whether it is open (the listing's own is not).
    listed.retain(|n| unsafe { libc::fcntl(*n, libc::F_GETFD) } >= 0);
    listed.sort_unstable();
    listed
}

fn report(out: &mut impl Write) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let no_new_privs = status
        .lines()
        .find_map(|l| l.strip_prefix("NoNewPrivs:"))
        .map_or("?", str::trim);
    let link = |p: &str| std::fs::read_link(p).map_or("?".to_owned(), |t| t.display().to_string());
    let _ = writeln!(out, "fsize={}", limit(libc::RLIMIT_FSIZE as libc::c_int));
    let _ = writeln!(out, "core={}", limit(libc::RLIMIT_CORE as libc::c_int));
    let _ = writeln!(out, "nofile={}", limit(libc::RLIMIT_NOFILE as libc::c_int));
    let _ = writeln!(out, "nonewprivs={no_new_privs}");
    let _ = writeln!(out, "env={}", std::env::vars().count());
    let _ = writeln!(out, "stdin={}", link("/proc/self/fd/0"));
    let _ = writeln!(
        out,
        "cwd={}",
        std::env::current_dir().map_or("?".to_owned(), |d| d.display().to_string())
    );
    for fd in open_descriptors() {
        let _ = writeln!(out, "fd={fd}");
    }
}

fn main() {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    for op in std::env::args().skip(1) {
        let (name, arg) = op.split_once(':').unwrap_or((op.as_str(), ""));
        match name {
            "emit" => {
                out.write_all(arg.as_bytes()).expect("write");
                out.flush().expect("flush");
            }
            "err" => {
                let _ = err.write_all(arg.as_bytes());
            }
            "flood" => {
                let chunk = [b'y'; 8192];
                while out.write_all(&chunk).is_ok() {}
                return;
            }
            "trickle" => loop {
                if out.write_all(b"a").and_then(|()| out.flush()).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            },
            "errflood" => {
                let chunk = [b'e'; 4096];
                for _ in 0..arg.parse::<usize>().expect("a count") / chunk.len() {
                    let _ = err.write_all(&chunk);
                }
            }
            "sleep" => std::thread::sleep(Duration::from_secs_f64(arg.parse().expect("seconds"))),
            "exit" => std::process::exit(arg.parse().expect("a code")),
            "pid" => {
                let mut fifo = std::fs::OpenOptions::new()
                    .write(true)
                    .open(arg)
                    .expect("the pid pipe");
                write!(fifo, "{}", std::process::id()).expect("write the pid");
            }
            "readfd" => {
                // SAFETY: the parent lent this descriptor; this process owns it from here.
                let mut lent = unsafe { std::fs::File::from_raw_fd(arg.parse().expect("an fd")) };
                let mut bytes = Vec::new();
                lent.read_to_end(&mut bytes)
                    .expect("read the lent descriptor");
                out.write_all(&bytes).expect("write");
                out.flush().expect("flush");
            }
            "report" => {
                report(&mut out);
                out.flush().expect("flush");
            }
            other => panic!("no such operation: {other}"),
        }
    }
}
