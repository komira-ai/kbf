//! A test helper, not part of the tool: makes one system call that needs the network and exits
//! with 0 when it worked and with the errno when it did not. The tests run it as an action's
//! Command (`tests/network.rs`), because only a real process shows what the filter does.
//!
//! `socket_probe <mode>`, where the mode is `inet`, `inet6`, `unix`, `netlink`, `packet`
//! (a socket of that domain), `pair-inet` or `pair-unix` (`socketpair`), `uring`
//! (`io_uring_setup`), `x32` (`socket(AF_INET)` under the x32 number, `x86_64` only), or
//! `child <mode>`: the same, in a copy of this program that this one starts and waits for, to
//! show that a descendant has the same limits.

use std::os::unix::process::ExitStatusExt as _;
use std::process::{Command, ExitCode};

/// Exit code for a mode this program does not know.
const UNKNOWN_MODE: u8 = 254;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    ExitCode::from(probe(&args))
}

fn probe(args: &[&str]) -> u8 {
    match args {
        ["child", rest @ ..] => child(rest),
        ["inet"] => socket(libc::AF_INET),
        ["inet6"] => socket(libc::AF_INET6),
        ["unix"] => socket(libc::AF_UNIX),
        #[cfg(target_os = "linux")]
        ["netlink"] => socket(libc::AF_NETLINK),
        #[cfg(target_os = "linux")]
        ["packet"] => socket(libc::AF_PACKET),
        ["pair-inet"] => pair(libc::AF_INET),
        ["pair-unix"] => pair(libc::AF_UNIX),
        #[cfg(target_os = "linux")]
        ["uring"] => uring(),
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        ["x32"] => x32(),
        _ => UNKNOWN_MODE,
    }
}

/// The exit code for a system call that returned `result`: 0, or its errno.
fn outcome(result: libc::c_long) -> u8 {
    if result >= 0 {
        return 0;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    u8::try_from(errno).unwrap_or(UNKNOWN_MODE)
}

fn socket(domain: libc::c_int) -> u8 {
    // SAFETY: integer arguments only; a descriptor it returns is closed with the process.
    outcome(libc::c_long::from(unsafe {
        libc::socket(domain, libc::SOCK_STREAM, 0)
    }))
}

fn pair(domain: libc::c_int) -> u8 {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors.
    outcome(libc::c_long::from(unsafe {
        libc::socketpair(domain, libc::SOCK_STREAM, 0, fds.as_mut_ptr())
    }))
}

#[cfg(target_os = "linux")]
fn uring() -> u8 {
    // `struct io_uring_params` is 120 bytes and zero is a valid request.
    let mut params = [0u8; 120];
    // SAFETY: the kernel writes at most the 120 bytes of the structure.
    outcome(unsafe { libc::syscall(libc::SYS_io_uring_setup, 1, params.as_mut_ptr()) })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn x32() -> u8 {
    let x32_bit: libc::c_long = 0x4000_0000;
    // SAFETY: integer arguments only.
    outcome(unsafe {
        libc::syscall(
            libc::SYS_socket | x32_bit,
            libc::AF_INET,
            libc::SOCK_STREAM,
            0,
        )
    })
}

/// Runs this program again with `rest` and answers with its code (128 plus the signal that
/// ended it).
fn child(rest: &[&str]) -> u8 {
    let Ok(me) = std::env::current_exe() else {
        return UNKNOWN_MODE;
    };
    match Command::new(me).args(rest).status() {
        Ok(status) => status
            .code()
            .or_else(|| status.signal().map(|s| 128 + s))
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(UNKNOWN_MODE),
        Err(_) => UNKNOWN_MODE,
    }
}
