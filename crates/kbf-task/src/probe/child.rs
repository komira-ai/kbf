//! The calls the probe's Command makes inside the confinement (Linux). Each returns the value of
//! one `observe <name> <value>` line: `ok`, `errno <n>`, or for the secret path `readable`.

use std::io::Read as _;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

/// How long a connect may take; the namespace answers at once, so this only bounds a host
/// whose route is unexpectedly live.
const CONNECT: Duration = Duration::from_secs(3);

fn errno(e: &std::io::Error) -> String {
    format!("errno {}", e.raw_os_error().unwrap_or(0))
}

fn outcome<T>(result: std::io::Result<T>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(e) => errno(&e),
    }
}

/// Bind a listener on `127.0.0.1`, connect to it and accept.
fn loopback() -> String {
    outcome((|| {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let _client = TcpStream::connect_timeout(&listener.local_addr()?, CONNECT)?;
        listener.accept()
    })())
}

/// Connect to an address outside any local network.
fn inet() -> String {
    outcome(TcpStream::connect_timeout(
        &SocketAddr::from(super::OUTSIDE),
        CONNECT,
    ))
}

/// Make a socket of `domain`.
fn socket(domain: libc::c_int) -> String {
    // SAFETY: `socket` takes three integers and returns a descriptor or -1.
    fd_outcome(libc::c_long::from(unsafe {
        libc::socket(domain, libc::SOCK_RAW, 0)
    }))
}

/// `io_uring_setup` with a zeroed parameter block (`struct io_uring_params` is 120 bytes), so
/// that a kernel and a profile that allow it make a ring: the answer is `ok` then, not `EFAULT`.
fn io_uring() -> String {
    let mut params = [0u64; 15];
    // SAFETY: `params` is writable and at least as large as `struct io_uring_params`.
    fd_outcome(unsafe { libc::syscall(libc::SYS_io_uring_setup, 1u32, params.as_mut_ptr()) })
}

/// The value for a call that returns a descriptor or -1 with `errno`: the descriptor is closed.
pub(super) fn fd_outcome(fd: libc::c_long) -> String {
    if fd < 0 {
        return errno(&std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by the call and is not used again.
    unsafe { libc::close(fd as libc::c_int) };
    "ok".to_owned()
}

/// Whether the file at `path` can be opened and a byte read from it, or the directory listed.
/// Nothing read is kept or shown.
fn secret(path: &Path) -> String {
    let tried = (|| {
        if std::fs::metadata(path)?.is_dir() {
            std::fs::read_dir(path).map(|mut d| d.next())?;
        } else {
            let _bytes: usize = std::fs::File::open(path)?.read(&mut [0u8; 1])?;
        }
        Ok(())
    })();
    match tried {
        Ok(()) => "readable".to_owned(),
        Err(e) => errno(&e),
    }
}

/// The `observe` lines, one per call.
pub fn observations(secret_path: Option<&Path>) -> Vec<String> {
    let mut lines = vec![
        format!("observe loopback {}", loopback()),
        format!("observe inet {}", inet()),
        format!("observe netlink {}", socket(libc::AF_NETLINK)),
        format!("observe packet {}", socket(libc::AF_PACKET)),
        format!("observe io_uring {}", io_uring()),
    ];
    if let Some(path) = secret_path {
        lines.push(format!("observe secret {}", secret(path)));
    }
    lines
}
