//! A test helper, not part of the tool: makes one system call that needs the network and exits
//! with 0 when it worked and with the errno when it did not. The tests run it as an action's
//! Command (`tests/network.rs`), because only a real process shows what the filter does.
//!
//! `socket_probe <mode>`, where the mode is `inet`, `inet6`, `unix`, `netlink`, `packet`
//! (a socket of that domain), `pair-inet` or `pair-unix` (`socketpair`), `uring`
//! (`io_uring_setup`), `x32` (`socket(AF_INET)` under the x32 number, `x86_64` only), or
//! `child <mode>`: the same, in a copy of this program that this one starts and waits for, to
//! show that a descendant has the same limits. Other modes: `read <path>` reads a file (exit 0, or the errno), `link-read <from> <to>` and `rename-read <from> <to>` make a hard link or rename and then read the new name (the errno of the first step that fails), `touch <path>` makes a file (to show
//! the program ran), `ifaces` prints the network interfaces it sees, `ids` prints its user and
//! group id, `tcp-loopback` (listen on 127.0.0.1, connect, accept: exit 0 or the errno) and
//! `tcp-outside` (connect to an address outside any local network: the errno), `int80` (`x86_64`) makes a 32-bit system call, `steal <pid> <fd>` takes a
//! descriptor out of another process with `pidfd_getfd` (exit 0 if it worked, else the errno),
//! and `hidden <pid>` exits 0 when `/proc/<pid>/status` is not owned by its own user, which is what a
//! non-dumpable process looks like.

use std::os::unix::fs::MetadataExt as _;
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
        ["read", path] => match std::fs::read(path) {
            Ok(_) => 0,
            Err(e) => errno_of(&e),
        },
        ["link-read", from, to] => {
            std::fs::hard_link(from, to).map_or_else(|e| errno_of(&e), |()| probe(&["read", to]))
        }
        ["rename-dir-read", from, to, name] => std::fs::rename(from, to).map_or_else(
            |e| errno_of(&e),
            |()| probe(&["read", &format!("{to}/{name}")]),
        ),
        ["rename-read", from, to] => {
            std::fs::rename(from, to).map_or_else(|e| errno_of(&e), |()| probe(&["read", to]))
        }
        ["touch", path] => u8::from(std::fs::File::create(path).is_err()),
        ["ifaces"] => ifaces(),
        ["tcp-loopback"] => tcp_loopback(),
        ["tcp-outside"] => tcp_outside(),
        ["tcp-loopback6"] => tcp_loopback6(),
        ["ids"] => ids(),
        ["hidden", pid] => hidden(pid),
        #[cfg(target_arch = "x86_64")]
        ["int80"] => int80(),
        #[cfg(target_os = "linux")]
        ["netlink"] => socket(libc::AF_NETLINK),
        #[cfg(target_os = "linux")]
        ["packet"] => socket(libc::AF_PACKET),
        ["pair-inet"] => pair(libc::AF_INET),
        ["pair-unix"] => pair(libc::AF_UNIX),
        #[cfg(target_os = "linux")]
        ["uring"] => uring(),
        #[cfg(target_os = "linux")]
        ["steal", pid, fd] => steal(pid, fd),
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

/// Prints the interface names of `/proc/net/dev` that this process's network namespace has.
fn ifaces() -> u8 {
    let Ok(text) = std::fs::read_to_string("/proc/net/dev") else {
        return UNKNOWN_MODE;
    };
    for line in text.lines().skip(2) {
        if let Some((name, _)) = line.split_once(':') {
            println!("{}", name.trim());
        }
    }
    0
}

fn ids() -> u8 {
    // SAFETY: no arguments.
    println!("{} {}", unsafe { libc::getuid() }, unsafe {
        libc::getgid()
    });
    0
}

fn hidden(pid: &str) -> u8 {
    let Ok(meta) = std::fs::metadata(format!("/proc/{pid}/status")) else {
        return UNKNOWN_MODE;
    };
    // SAFETY: no arguments.
    u8::from(meta.uid() == unsafe { libc::getuid() })
}

#[cfg(target_os = "linux")]
fn steal(pid: &str, fd: &str) -> u8 {
    let (Ok(pid), Ok(fd)) = (pid.parse::<libc::c_long>(), fd.parse::<libc::c_long>()) else {
        return UNKNOWN_MODE;
    };
    // SAFETY: integer arguments only.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if pidfd < 0 {
        return outcome(pidfd);
    }
    // SAFETY: integer arguments only.
    outcome(unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd, fd, 0) })
}

/// `getpid` through the 32-bit entry (`int 0x80`), which the kernel serves for a 32-bit ABI
/// process and also for a 64-bit one.
#[cfg(target_arch = "x86_64")]
fn int80() -> u8 {
    let result: u64;
    // SAFETY: the 32-bit `getpid` (20) reads no memory; the registers it changes are declared.
    unsafe {
        std::arch::asm!(
            "int 0x80",
            inlateout("rax") 20u64 => result,
            options(nostack),
        );
    }
    // A result in -4095..0 is an error number (negated).
    u8::from(result > u64::MAX - 4095)
}

fn errno_of(e: &std::io::Error) -> u8 {
    u8::try_from(e.raw_os_error().unwrap_or(0)).unwrap_or(UNKNOWN_MODE)
}

fn tcp_loopback() -> u8 {
    let run = || -> std::io::Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let client = std::net::TcpStream::connect(listener.local_addr()?)?;
        let (_server, _) = listener.accept()?;
        drop(client);
        Ok(())
    };
    run().map_or_else(|e| errno_of(&e), |()| 0)
}

fn tcp_outside() -> u8 {
    // An address of the documentation range (RFC 5737): no route to it exists in an empty
    // namespace, so the connect fails at once with `ENETUNREACH`.
    // A bounded wait: where the network is open the packets may get no answer at all, which is
    // reported as `ETIMEDOUT`, not waited for.
    let address = std::net::SocketAddr::from(([192, 0, 2, 1], 9));
    match std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(3)) {
        Ok(_) => 0,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            u8::try_from(libc::ETIMEDOUT).unwrap_or(UNKNOWN_MODE)
        }
        Err(e) => errno_of(&e),
    }
}

/// Listens on `[::1]` and connects to it: 0, or the errno of the step that failed.
fn tcp_loopback6() -> u8 {
    let run = || -> std::io::Result<()> {
        let listener = std::net::TcpListener::bind("[::1]:0")?;
        let client = std::net::TcpStream::connect(listener.local_addr()?)?;
        let (_server, _) = listener.accept()?;
        drop(client);
        Ok(())
    };
    run().map_or_else(|e| errno_of(&e), |()| 0)
}
