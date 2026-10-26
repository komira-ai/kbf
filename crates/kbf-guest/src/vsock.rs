//! The address of a virtio socket, shared by the two ends a macOS guest has: the client that
//! reaches the host ([`crate::probe_host`]) and the listener the host reaches
//! ([`crate::shutdown`]).

/// The address family of virtio sockets. UNCERTAIN: this is the value `<sys/vsock.h>` of
/// the macOS SDK gives `AF_VSOCK` (as on Linux). Whether a macOS guest's kernel serves the
/// family to a process at all is what a run in a guest answers; a `socket` call that
/// fails is reported as such, not guessed around.
pub const AF_VSOCK: i32 = 40;
/// The host's context id.
pub const HOST_CID: u32 = 2;
/// "Any context": the address a listener binds, so that it accepts whoever connects.
pub const CID_ANY: u32 = u32::MAX;
/// The size of `struct sockaddr_vm`.
pub const SOCKADDR_VM_LEN: usize = 16;

/// The bytes of a `struct sockaddr_vm` for `cid` and `port`. UNCERTAIN: the layout is the
/// macOS SDK's as read from `<sys/vsock.h>`: a length byte, the family byte, two reserved
/// bytes, then the port and the context id as native (little-endian on Apple silicon)
/// 32-bit integers.
#[must_use]
pub fn sockaddr_vm(cid: u32, port: u32) -> [u8; SOCKADDR_VM_LEN] {
    let mut addr = [0u8; SOCKADDR_VM_LEN];
    addr[0] = SOCKADDR_VM_LEN as u8;
    addr[1] = AF_VSOCK as u8;
    addr[4..8].copy_from_slice(&port.to_le_bytes());
    addr[8..12].copy_from_slice(&cid.to_le_bytes());
    addr
}

/// Sets the receive and send timeouts of the socket `fd`, so that a peer that stops
/// answering cannot hold a read or a write for good.
///
/// # Errors
/// The `errno` of the failed `setsockopt`.
pub fn set_io_timeout(fd: std::os::fd::RawFd, timeout: std::time::Duration) -> Result<(), i32> {
    let tv = libc::timeval {
        tv_sec: libc::time_t::try_from(timeout.as_secs()).unwrap_or(libc::time_t::MAX),
        // Under a million, so it fits the 32-bit type of macOS as well as the 64-bit one of Linux.
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        // SAFETY: `tv` is a `timeval` and its size is passed; `fd` is the caller's socket.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                std::ptr::from_ref(&tv).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// Catches a timeout that is not applied (a silent peer would hold the listener for
    /// good) and a descriptor that is not a socket being reported as success.
    #[test]
    fn a_silent_peer_times_out_and_a_bad_descriptor_is_an_error() {
        let (mut a, _b) = UnixStream::pair().expect("pair");
        set_io_timeout(a.as_raw_fd(), Duration::from_millis(50)).expect("set");
        let e = a.read(&mut [0u8; 1]).expect_err("times out");
        assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(set_io_timeout(-1, Duration::from_secs(1)), Err(libc::EBADF));
    }

    /// Catches an address with the port and the context id swapped, or a wrong length or
    /// family byte: the client and the listener build the same bytes, so a swap would
    /// still look consistent end to end and fail only in a guest.
    #[test]
    fn the_address_has_the_length_family_port_and_context_id_in_their_places() {
        let a = sockaddr_vm(HOST_CID, 5000);
        assert_eq!(a[0], 16);
        assert_eq!(a[1], 40);
        assert_eq!(a[2..4], [0, 0]);
        assert_eq!(a[4..8], 5000u32.to_le_bytes());
        assert_eq!(a[8..12], 2u32.to_le_bytes());
        assert_eq!(a[12..], [0, 0, 0, 0]);
        assert_eq!(sockaddr_vm(CID_ANY, 5001)[8..12], [0xff; 4]);
    }
}
