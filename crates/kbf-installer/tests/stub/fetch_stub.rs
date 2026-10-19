//! A stand-in for the fetch child, for the tests of the parent's end of the pipe
//! (`tests/fetch_pipe.rs`) and of the fetcher built on it (`tests/fetcher_real.rs`). Each
//! argument is one operation, done in order:
//!
//! - `emit:TEXT` write TEXT to standard output; `err:TEXT` to standard error;
//! - `copy:PATH` write the file PATH to standard output; `head:N:PATH` its first N bytes;
//! - `serve:DIR` (as `fetch-child` only) write the file in DIR named by what the child is asked
//!   for: its `--sha256`, or `describe` for a `--describe`; a file that is not there is a
//!   failed fetch (a line on standard error and status 1), as a source without it would be;
//! - `flood` write to standard output until it breaks; `trickle` one byte every 200 ms;
//! - `errflood:N` write N bytes to standard error;
//! - `sleep:SECS` wait; `exit:CODE` end with that status; `kill` end by SIGKILL;
//! - `pid:FIFO` write this process's id to the named pipe (a file would break `RLIMIT_FSIZE`);
//! - `readfd:N` copy descriptor N to standard output;
//! - `report` print the limits and descriptors this process was started with.
//!
//! Started the way the parent starts the real child (`fetch-child --sha256 ... --ca-file
//! FILE`, flags a test cannot choose), the operations are the lines of FILE instead.
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
    // An instrumented build (coverage) has the profile runtime put its own variable in; the
    // parent's empty environment is what is being checked.
    let env = std::env::vars()
        .filter(|(name, _)| !name.contains("LLVM_"))
        .count();
    let _ = writeln!(out, "env={env}");
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

/// End now, without the exit handlers: an instrumented stub confined to write no file would
/// print a profile-write error on standard error, which the tests read.
fn leave(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    // SAFETY: _exit takes no pointers and never returns.
    unsafe { libc::_exit(code) }
}

/// The operations of a run started by `fetch_component`: the lines of the file `--ca-file`
/// names (the one flag of the child's that a test fills as it likes).
fn script(args: &[String]) -> Vec<String> {
    let path = args
        .iter()
        .position(|a| a == "--ca-file")
        .and_then(|at| args.get(at + 1))
        .expect("--ca-file names the operations");
    std::fs::read_to_string(path)
        .expect("the operations")
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// What a `fetch-child` run is asked for: the blob's digest, or `describe`.
fn asked_for(args: &[String]) -> String {
    if args.iter().any(|a| a == "--describe") {
        return "describe".to_owned();
    }
    args.iter()
        .position(|a| a == "--sha256")
        .and_then(|at| args.get(at + 1))
        .cloned()
        .expect("--sha256 or --describe")
}

fn main() {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (ops, asked) = if args.first().is_some_and(|a| a == "fetch-child") {
        (script(&args), Some(asked_for(&args)))
    } else {
        (args, None)
    };
    for op in ops {
        let (name, arg) = op.split_once(':').unwrap_or((op.as_str(), ""));
        match name {
            "serve" => {
                let key = asked.as_deref().expect("serve needs a fetch-child run");
                match std::fs::read(std::path::Path::new(arg).join(key)) {
                    Ok(bytes) => {
                        out.write_all(&bytes).expect("write");
                        out.flush().expect("flush");
                    }
                    Err(_) => {
                        let _ = writeln!(err, "fetch-child: nothing is served as {key}");
                        leave(1);
                    }
                }
            }
            "emit" => {
                out.write_all(arg.as_bytes()).expect("write");
                out.flush().expect("flush");
            }
            "copy" => {
                out.write_all(&std::fs::read(arg).expect("the file"))
                    .expect("write");
                out.flush().expect("flush");
            }
            "head" => {
                let (count, path) = arg.split_once(':').expect("head:N:PATH");
                let bytes = std::fs::read(path).expect("the file");
                let count: usize = count.parse().expect("a count");
                out.write_all(&bytes[..count]).expect("write");
                out.flush().expect("flush");
            }
            "kill" => {
                // SAFETY: kill on this process's own id; SIGKILL does not return to the caller.
                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                leave(137);
            }
            "err" => {
                let _ = err.write_all(arg.as_bytes());
            }
            "flood" => {
                let chunk = [b'y'; 8192];
                while out.write_all(&chunk).is_ok() {}
                leave(0);
            }
            "trickle" => loop {
                if out.write_all(b"a").and_then(|()| out.flush()).is_err() {
                    leave(0);
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
            "exit" => leave(arg.parse().expect("a code")),
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
    leave(0);
}
