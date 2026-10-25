//! The macOS guest's virtio-socket listener for the host's shutdown command
//! ([`crate::shutdown`]).
//!
//! # FFI-BOUNDARY
//! The listening descriptor is owned by [`VsockListener`] (an `OwnedFd`) and closed when it
//! drops. Each accepted descriptor is owned by the `File` handed to the caller, which closes
//! it. Nothing else holds a descriptor.

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use crate::shutdown::{self, Daemon, Incoming, SystemRunner};
use crate::vsock::{AF_VSOCK, CID_ANY, SOCKADDR_VM_LEN, set_io_timeout, sockaddr_vm};

/// How long a read or write on an accepted connection may block: a peer that connects and
/// stops cannot hold the listener.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn describe(stage: &str) -> String {
    let e = errno();
    format!(
        "{stage} failed: errno {e} ({})",
        std::io::Error::from_raw_os_error(e)
    )
}

/// A listening virtio socket.
pub struct VsockListener {
    fd: OwnedFd,
}

impl VsockListener {
    /// Binds `port` on any context id and listens.
    ///
    /// UNCERTAIN until a macOS guest runs it: that the guest's kernel serves `AF_VSOCK` to a
    /// process, that `bind` takes `struct sockaddr_vm` as laid out in
    /// [`sockaddr_vm`], and that the any-context id is all ones as on Linux. Each failing
    /// step is named with its `errno`.
    ///
    /// # Errors
    /// A step failed: `socket`, `bind` or `listen`.
    pub fn bind(port: u32) -> Result<Self, String> {
        // SAFETY: plain socket(2) call; a descriptor it returns is owned by the `OwnedFd`.
        let fd = unsafe { libc::socket(AF_VSOCK, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(describe("socket"));
        }
        // SAFETY: `fd` was just returned by socket(2) and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let addr = sockaddr_vm(CID_ANY, port);
        // SAFETY: `addr` is a `struct sockaddr_vm`-sized buffer and its length is passed.
        let bound = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                addr.as_ptr().cast(),
                SOCKADDR_VM_LEN as libc::socklen_t,
            )
        };
        if bound != 0 {
            return Err(describe("bind"));
        }
        // SAFETY: `fd` is a bound socket.
        if unsafe { libc::listen(fd.as_raw_fd(), 1) } != 0 {
            return Err(describe("listen"));
        }
        Ok(Self { fd })
    }
}

impl Incoming for VsockListener {
    type Conn = File;

    fn accept(&mut self) -> Result<File, String> {
        loop {
            // SAFETY: the peer address is not wanted: both pointers are null.
            let fd = unsafe {
                libc::accept(
                    self.fd.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if fd < 0 {
                match errno() {
                    libc::EINTR | libc::ECONNABORTED => continue,
                    _ => return Err(describe("accept")),
                }
            }
            // SAFETY: `fd` was just returned by accept(2) and nothing else owns it.
            let conn = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
            // A connection whose timeouts cannot be set is dropped, not served unbounded.
            if set_io_timeout(conn.as_raw_fd(), IO_TIMEOUT).is_ok() {
                return Ok(conn);
            }
        }
    }
}

/// The real daemon: listens on the guest's virtio socket and runs `/sbin/shutdown`.
pub struct SystemDaemon;

impl Daemon for SystemDaemon {
    fn run(&self, port: u32, log: &mut dyn Write) -> Result<(), String> {
        let mut listener = VsockListener::bind(port)?;
        let _ = writeln!(
            log,
            "kbf-guest: shutdown-listen: listening on vsock port {port}"
        );
        shutdown::serve(&mut listener, &SystemRunner, log)
    }
}
