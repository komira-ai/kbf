//! The helper's wire format on its Unix socket.
//!
//! One connection carries one request. Every message is a frame: a 4-byte big-endian
//! length, then that many bytes of JSON. The helper speaks first: once it has checked
//! the process that connected, it sends [`Reply::Hello`] with a fresh nonce (or
//! [`Reply::Refused`], and hangs up). The client's request is then one frame, a
//! [`Call`] that carries that nonce, so a request written before the helper's check
//! cannot pass for one written after it (S4.3). `run` attaches four descriptors to it
//! (`SCM_RIGHTS`): stdin, stdout, stderr and the lease directory, in that order, and so
//! does `role-run`. The
//! helper answers with one frame, except that a started `run` answers
//! [`Reply::Started`] and then, when the process exits, [`Reply::Exited`].

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use rustix::net::sockopt::{Timeout, set_socket_timeout};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use serde::{Deserialize, Serialize};

/// The largest frame either side accepts.
pub const MAX_FRAME: usize = 64 * 1024;
/// The number of descriptors a `run` request carries.
pub const RUN_FDS: usize = 4;
/// The most entries each list of a [`Reply::Scanned`] names, each cut to
/// [`SCAN_LINE_BYTES`], so the reply fits a frame; the helper's log holds them all.
pub const SCAN_LINES: usize = 20;
pub const SCAN_LINE_BYTES: usize = 300;

/// The most bytes of `kbf-vmm`'s report an image verb's reply carries.
pub const IMAGE_REPORT_BYTES: usize = 16 * 1024;

/// The receive buffer for ancillary data. On Linux, room for one more descriptor than a
/// request may carry: the kernel cuts what does not fit, says so (`MSG_CTRUNC`), and
/// the cut is refused. On macOS the kernel takes at most `MCLBYTES` (2048) bytes of
/// control data with one message, so this buffer is never cut there; that matters,
/// because a cut message keeps its full length in its header on macOS, which the
/// parser would read past.
#[cfg(target_os = "linux")]
const CONTROL_BYTES: usize = rustix::cmsg_space!(ScmRights(RUN_FDS + 1));
#[cfg(not(target_os = "linux"))]
const CONTROL_BYTES: usize = 4096;

/// The client's one frame: the nonce of the helper's [`Reply::Hello`] on this
/// connection, and the request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub nonce: String,
    pub request: Request,
}

/// What the daemon asks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    /// Create the lease's user; an administrator only with a gate grant.
    UserCreate {
        lease: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant: Option<String>,
    },
    /// Start a process as the lease's user, with the attached descriptors; in its GUI
    /// session when `session` is `gui`.
    Run {
        lease: String,
        argv: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Session::is_background")]
        session: Session,
    },
    /// Whether the lease user's GUI session (`gui/<uid>`) is up, after `session-login`.
    SessionReady { lease: String },
    /// End every process of the lease's uid.
    KillUid { lease: String },
    /// Delete the lease's user and sweep what it leaves.
    UserDelete { lease: String },
    /// Auto-login into the lease's user, then a userspace restart.
    SessionLogin { lease: String },
    /// Rest the Mac, then a userspace restart: at the standing VM user when the helper
    /// has one (`--vm-user`), else at the login window (auto-login cleared).
    SessionIdle,
    /// Start `kbf-vmm vm-run --spec <file>` as the standing VM user in its GUI session,
    /// with the same four descriptors as `run`. Refused unless the argument vector is
    /// exactly the helper's configured `kbf-vmm`, `vm-run`, `--spec` and a regular file
    /// inside the images directory reached through no link, the VM user's `gui/<uid>`
    /// domain is up and no lease holds the session.
    RoleRun {
        argv: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// Whether the standing VM user's GUI session (`gui/<uid>`) is up. False while a
    /// lease holds the session.
    RoleSessionReady,
    /// End every process of the standing VM user (its session with them).
    RoleKillUid,
    /// End the process group a `role-run` started and nothing else: `pgid` is the pid
    /// `Started` reported, `start` that process's start time and `boot` the boot session
    /// it ran in. The helper kills the group only while that process still leads it, is
    /// the VM user's, and has this start time in this boot.
    RoleReap { pgid: i32, start: u64, boot: String },
    /// As root, take the base image bundle `bundle` (a path directly inside the staging
    /// directory), move it into the images directory, write the guest's first-boot files
    /// into it (`kbf-vmm image setup`) and seal it: root's, mode 0750/0640 with the VM
    /// user's group, `uchg`. There is no program and no argument here: the helper runs its
    /// own installed `kbf-vmm`.
    ImageSetup { bundle: String },
    /// As root, copy the diagnostics allowlist out of the stopped guest bundle `bundle`
    /// (directly inside the staging or the images directory) into the new directory `out`
    /// (directly inside the images directory) (`kbf-vmm image harvest`); the helper makes
    /// `out`, root's and mode 0700.
    ImageHarvest { bundle: String, out: String },
    /// The boot reset, before the daemon's `Hello`: `held` are the leases the server
    /// still holds for this node.
    BootReset {
        #[serde(default)]
        held: Vec<String>,
    },
    /// The leak scan against the recorded baseline.
    Scan,
    /// Record the leak scan's baseline, in its window.
    Baseline,
    /// Reboot, after the helper's own scan came back dirty.
    RebootDirty,
}

/// Where `run` starts a process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Session {
    /// Outside any login session: the system's launchd domain, as the helper runs.
    #[default]
    Background,
    /// In the lease user's GUI (Aqua) session, `gui/<uid>`, through `launchctl asuser`:
    /// the session `session-login` started. Refused while the session is not up.
    Gui,
}

impl Session {
    /// Whether this is [`Session::Background`], which a request leaves out.
    #[must_use]
    pub const fn is_background(&self) -> bool {
        matches!(self, Self::Background)
    }
}

/// What the helper answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum Reply {
    /// The caller passed the check at connect; its request must carry this nonce.
    Hello { nonce: String },
    /// The user exists with this uid.
    Created { uid: u32 },
    /// The process started with this pid.
    Started { pid: u32 },
    /// The process ended: its exit code, or the signal that ended it.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// No process of the uid remains.
    Killed,
    /// The user is gone and its state swept; `existed` is false when there was no
    /// user record left to delete (a repeated or resumed delete).
    Deleted { existed: bool },
    /// The session switch is set and a userspace restart was asked for; the restart
    /// may end the connection before this arrives.
    Restarting,
    /// Whether the lease user's GUI session is up.
    SessionReady { ready: bool },
    /// The boot reset is done: the lease whose session it kept, and the leases whose
    /// users it deleted.
    Reset {
        kept: Option<String>,
        deleted: Vec<String>,
    },
    /// The scan ran: `clean` when nothing was added to or removed from the baseline
    /// and every item could be read. Each list names at most
    /// [`SCAN_LINES`] entries, then how many more there are.
    Scanned {
        clean: bool,
        added: Vec<String>,
        removed: Vec<String>,
        errors: Vec<String>,
    },
    /// The baseline is recorded: how many items each class holds.
    Baselined { classes: Vec<(String, usize)> },
    /// An image verb ran: what `kbf-vmm` printed (its JSON report), cut to
    /// [`IMAGE_REPORT_BYTES`].
    Imaged { report: String },
    /// The request was refused or failed, and why.
    Refused { reason: String },
}

/// Sends `message` as one frame, with `fds` attached.
///
/// # Errors
/// The message is larger than [`MAX_FRAME`], or the socket write failed.
pub fn send<T: Serialize>(
    socket: BorrowedFd<'_>,
    message: &T,
    fds: &[BorrowedFd<'_>],
) -> io::Result<()> {
    send_frame(socket, &serde_json::to_vec(message)?, fds)
}

/// Receives one frame as a `T`, and the descriptors attached to it; `None` at a clean
/// end of stream before any byte. More than `max_fds` descriptors is an error (they
/// are closed).
///
/// # Errors
/// The stream ended inside a frame, the frame is too large or not a `T`, or too many
/// descriptors came.
pub fn recv<T: for<'de> Deserialize<'de>>(
    socket: BorrowedFd<'_>,
    max_fds: usize,
) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    recv_frame(socket, max_fds, None)?
        .map(|(body, fds)| Ok((serde_json::from_slice(&body)?, fds)))
        .transpose()
}

/// [`recv`], with the whole frame due by `deadline`: each read waits only for what is
/// left until then (it sets the socket's receive timeout), so a sender that trickles
/// bytes cannot stretch the wait.
///
/// # Errors
/// As [`recv`]; [`io::ErrorKind::TimedOut`] once the deadline has passed.
pub fn recv_by<T: for<'de> Deserialize<'de>>(
    socket: BorrowedFd<'_>,
    max_fds: usize,
    deadline: Instant,
) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    let origin = Instant::now();
    recv_within(
        socket,
        max_fds,
        deadline.saturating_duration_since(origin),
        &|| origin.elapsed(),
    )
}

/// A reading of a clock: time since an origin that does not change.
pub type Now<'a> = &'a dyn Fn() -> Duration;

/// [`recv_by`] with the deadline `within` after the call, on the clock `now`: the whole
/// frame is due when `now()` reads `within` more than it did at the call, and each read
/// waits only for what `now()` says is left. A test passes a clock it moves itself.
///
/// # Errors
/// As [`recv_by`].
pub fn recv_within<T: for<'de> Deserialize<'de>>(
    socket: BorrowedFd<'_>,
    max_fds: usize,
    within: Duration,
    now: Now<'_>,
) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    let due = Due {
        by: now().saturating_add(within),
        now,
    };
    recv_frame(socket, max_fds, Some(&due))?
        .map(|(body, fds)| Ok((serde_json::from_slice(&body)?, fds)))
        .transpose()
}

/// When a frame is due, and the clock that says what is left.
struct Due<'a> {
    by: Duration,
    now: Now<'a>,
}

fn send_frame(socket: BorrowedFd<'_>, body: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut frame = u32::try_from(body.len())
        .map_err(io::Error::other)?
        .to_be_bytes()
        .to_vec();
    frame.extend_from_slice(body);
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(RUN_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if fds.len() > RUN_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many descriptors",
        ));
    }
    if !fds.is_empty() {
        // The buffer has room for RUN_FDS descriptors, so this cannot fail; were it
        // to, the request would arrive without them and be refused.
        control.push(SendAncillaryMessage::ScmRights(fds));
    }
    let sent = rustix::net::sendmsg(
        socket,
        &[IoSlice::new(&frame)],
        &mut control,
        SendFlags::empty(),
    )?;
    write_all(socket, &frame[sent..])
}

/// Writes what a short `sendmsg` left (the descriptors went with its first byte).
fn write_all(socket: BorrowedFd<'_>, mut rest: &[u8]) -> io::Result<()> {
    while !rest.is_empty() {
        let wrote = rustix::net::send(socket, rest, SendFlags::empty())?;
        rest = &rest[wrote..];
    }
    Ok(())
}

/// One frame's bytes and descriptors; see [`recv`].
fn recv_frame(
    socket: BorrowedFd<'_>,
    max_fds: usize,
    deadline: Option<&Due<'_>>,
) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut fds = Vec::new();
    let mut header = [0u8; 4];
    let got = fill(socket, &mut header, &mut fds, deadline)?;
    if got == 0 {
        return Ok(None);
    }
    if got < header.len() {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    if fill(socket, &mut body, &mut fds, deadline)? < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if fds.len() > max_fds {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many descriptors",
        ));
    }
    // The clock is looked at before a read, so a frame whose last bytes came in one
    // read, after the deadline, would pass: look once more, before it is parsed.
    if let Some(deadline) = deadline
        && (deadline.now)() >= deadline.by
    {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the deadline passed",
        ));
    }
    Ok(Some((body, fds)))
}

/// Fills `buf` from the socket, collecting attached descriptors, by `deadline` if
/// there is one. Returns how many bytes came: fewer than `buf` holds only if the
/// stream ended.
fn fill(
    socket: BorrowedFd<'_>,
    buf: &mut [u8],
    fds: &mut Vec<OwnedFd>,
    deadline: Option<&Due<'_>>,
) -> io::Result<usize> {
    let late = || io::Error::new(io::ErrorKind::TimedOut, "the deadline passed");
    let mut got = 0;
    while got < buf.len() {
        if let Some(deadline) = deadline {
            let left = deadline.by.saturating_sub((deadline.now)());
            if left.is_zero() {
                return Err(late());
            }
            set_socket_timeout(socket, Timeout::Recv, Some(left))?;
        }
        let mut space = [MaybeUninit::uninit(); CONTROL_BYTES];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let message = match rustix::net::recvmsg(
            socket,
            &mut [IoSliceMut::new(&mut buf[got..])],
            &mut control,
            recv_flags(),
        ) {
            // The receive timeout set above ran out: the deadline passed.
            Err(rustix::io::Errno::AGAIN) if deadline.is_some() => return Err(late()),
            // A signal handler ran; with a receive timeout set, the kernel does not
            // restart the call. Read again, to the time the deadline leaves.
            Err(rustix::io::Errno::INTR) => continue,
            other => other?,
        };
        for received in control.drain().filter_map(rights) {
            for fd in received {
                close_on_exec(&fd)?;
                fds.push(fd);
            }
        }
        if message.flags.contains(rustix::net::ReturnFlags::CTRUNC) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many descriptors: the kernel cut them off",
            ));
        }
        if message.bytes == 0 {
            break;
        }
        got += message.bytes;
    }
    Ok(got)
}

/// The descriptors of an `SCM_RIGHTS` message; no other kind is asked for.
fn rights(message: RecvAncillaryMessage<'_>) -> Option<impl Iterator<Item = OwnedFd> + '_> {
    match message {
        RecvAncillaryMessage::ScmRights(fds) => Some(fds),
        _ => None,
    }
}

/// Linux delivers received descriptors close-on-exec; macOS has no such flag, so
/// [`close_on_exec`] marks them at once (and the child's own pass covers the gap).
#[cfg(target_os = "linux")]
fn recv_flags() -> RecvFlags {
    RecvFlags::CMSG_CLOEXEC
}

#[cfg(not(target_os = "linux"))]
fn recv_flags() -> RecvFlags {
    RecvFlags::empty()
}

fn close_on_exec(fd: &OwnedFd) -> io::Result<()> {
    Ok(rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    use super::*;

    fn run_request() -> Request {
        Request::Run {
            lease: "1.2".to_owned(),
            argv: vec!["/usr/bin/true".to_owned()],
            env: vec![("A".to_owned(), "b".to_owned())],
            session: Session::Background,
        }
    }

    /// Catches: descriptors lost or reordered between the daemon and the helper, or
    /// a frame read short.
    #[test]
    fn a_frame_and_its_descriptors_arrive_together() {
        let (a, b) = UnixStream::pair().unwrap();
        let files: Vec<std::fs::File> = (0..RUN_FDS)
            .map(|_| std::fs::File::open("/dev/null").unwrap())
            .collect();
        let fds: Vec<BorrowedFd<'_>> = files.iter().map(AsFd::as_fd).collect();
        send(a.as_fd(), &run_request(), &fds).unwrap();
        send(a.as_fd(), &Reply::Killed, &[]).unwrap();
        drop(a);
        let (request, got) = recv::<Request>(b.as_fd(), RUN_FDS).unwrap().unwrap();
        assert_eq!(request, run_request());
        assert_eq!(got.len(), RUN_FDS);
        for fd in &got {
            let flags = rustix::io::fcntl_getfd(fd).unwrap();
            assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
        }
        assert_eq!(
            recv::<Reply>(b.as_fd(), 0).unwrap().unwrap().0,
            Reply::Killed
        );
        assert!(recv::<Reply>(b.as_fd(), 0).unwrap().is_none());
    }

    /// Catches: a request carrying more descriptors than `run` takes accepted (the
    /// extra ones would stay open in the helper).
    #[test]
    fn too_many_descriptors_are_refused() {
        let (a, b) = UnixStream::pair().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        send(a.as_fd(), &Reply::Killed, &[file.as_fd(), file.as_fd()]).unwrap();
        let error = recv::<Reply>(b.as_fd(), 1).unwrap_err();
        assert!(error.to_string().contains("too many"), "{error}");

        let five = [file.as_fd(); RUN_FDS + 1];
        let error = send(a.as_fd(), &Reply::Killed, &five).unwrap_err();
        assert!(error.to_string().contains("too many"), "{error}");

        // Many more than a request carries, in one message: all received, then refused.
        let (a, b) = UnixStream::pair().unwrap();
        let many = [file.as_fd(); 8 * RUN_FDS];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(8 * RUN_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&many)));
        let frame = [0, 0, 0, 2, b'{', b'}'];
        rustix::net::sendmsg(
            &a,
            &[IoSlice::new(&frame)],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        let error = recv::<Reply>(b.as_fd(), RUN_FDS).unwrap_err();
        assert!(error.to_string().contains("too many"), "{error}");
    }

    /// The kernel cuts ancillary data that does not fit the receive buffer, and says
    /// so; the cut is refused, not taken for the whole. (Linux: macOS's buffer is
    /// larger than any control data its kernel passes.)
    #[cfg(target_os = "linux")]
    #[test]
    fn descriptors_beyond_the_buffer_are_refused() {
        let (a, b) = UnixStream::pair().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        let many = [file.as_fd(); 4 * RUN_FDS];
        let body = b"{}";
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(body);
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4 * RUN_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&many)));
        rustix::net::sendmsg(
            &a,
            &[IoSlice::new(&frame)],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        let error = recv::<Reply>(b.as_fd(), 4 * RUN_FDS).unwrap_err();
        assert!(error.to_string().contains("cut them off"), "{error}");
    }

    /// Catches: an unbounded frame length (the helper would allocate what any caller
    /// asks), a truncated frame taken whole, or unknown fields accepted.
    #[test]
    fn malformed_frames_are_refused() {
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&(MAX_FRAME as u32 + 1).to_be_bytes()).unwrap();
        // Closed, so a reader that skipped the check fails at once rather than waiting.
        drop(a);
        assert!(
            recv::<Request>(b.as_fd(), 0)
                .unwrap_err()
                .to_string()
                .contains("too large")
        );

        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&[0, 0, 0, 9, b'{']).unwrap();
        drop(a);
        assert_eq!(
            recv::<Request>(b.as_fd(), 0).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&[0, 0]).unwrap();
        drop(a);
        assert_eq!(
            recv::<Request>(b.as_fd(), 0).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        let (mut a, b) = UnixStream::pair().unwrap();
        let body = br#"{"verb":"kill-uid","lease":"1.1","uid":0}"#;
        a.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
        a.write_all(body).unwrap();
        assert!(recv::<Request>(b.as_fd(), 0).is_err());

        let (a, _b) = UnixStream::pair().unwrap();
        let huge = Reply::Refused {
            reason: "x".repeat(MAX_FRAME),
        };
        assert!(
            send(a.as_fd(), &huge, &[])
                .unwrap_err()
                .to_string()
                .contains("too large")
        );
    }

    /// A clock that reads the next of `readings` (in ms) at each call and the last one
    /// after that, and counts the calls: each call is one look at what is left, so a
    /// test says when each read of the frame happens without waiting for it.
    struct Script {
        readings: Vec<u64>,
        calls: std::cell::Cell<usize>,
    }

    impl Script {
        fn new(readings: &[u64]) -> Self {
            Self {
                readings: readings.to_vec(),
                calls: std::cell::Cell::new(0),
            }
        }
        fn now(&self) -> std::time::Duration {
            let at = self.calls.get();
            self.calls.set(at + 1);
            let ms = self.readings[at.min(self.readings.len() - 1)];
            std::time::Duration::from_millis(ms)
        }
    }

    fn receive_timeout(fd: BorrowedFd<'_>) -> Option<std::time::Duration> {
        rustix::net::sockopt::socket_timeout(fd, rustix::net::sockopt::Timeout::Recv).unwrap()
    }

    /// Catches: `recv_by` checking the deadline only after reading, a deadline already
    /// passed handed to the OS as a zero timeout (which means no timeout), a plain
    /// `recv`'s own read timeout reported as a passed deadline, and a frame that
    /// arrives in time not being read.
    #[test]
    fn recv_by_ends_at_its_deadline() {
        use std::time::Duration;
        let frame = [0, 0, 0, 8, b'"', b'k', b'i', b'l', b'l', b'e', b'd', b'"'];
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&frame).unwrap();
        let late = recv_by::<Reply>(b.as_fd(), 0, Instant::now()).unwrap_err();
        assert_eq!(late.kind(), io::ErrorKind::TimedOut);
        let soon = Instant::now() + Duration::from_secs(10);
        let (reply, _) = recv_by::<Reply>(b.as_fd(), 0, soon).unwrap().unwrap();
        assert_eq!(reply, Reply::Killed);
        // Half a frame, the rest never sent (`a` stays open): refused at the deadline.
        a.write_all(&frame[..6]).unwrap();
        let deadline = Instant::now() + Duration::from_millis(50);
        let late = recv_by::<Reply>(b.as_fd(), 0, deadline).unwrap_err();
        assert_eq!(late.kind(), io::ErrorKind::TimedOut, "{late}");
        // Without a deadline, a read timeout set on the socket is its own error.
        let (_a, b) = UnixStream::pair().unwrap();
        b.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let error = recv::<Reply>(b.as_fd(), 0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
    }

    /// Catches: a receive timeout set once, before the first read, rather than before
    /// every read from what is left (a short read would then restart the full wait and
    /// a trickling sender could stretch the deadline). Two bytes of a header are
    /// queued; the clock reads 0 ms at the start and 30 ms when the second read looks
    /// at what is left of 50 ms, so that read may wait 20 ms and no more.
    #[test]
    fn each_read_waits_only_for_what_is_left_of_the_deadline() {
        use std::time::Duration;
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&[0, 0]).unwrap();
        // Readings: the start (0), the first look (0), the second look (30).
        let clock = Script::new(&[0, 0, 30]);
        let now = || clock.now();
        let late = recv_within::<Reply>(b.as_fd(), 0, Duration::from_millis(50), &now).unwrap_err();
        assert_eq!(late.kind(), io::ErrorKind::TimedOut, "{late}");
        assert_eq!(clock.calls.get(), 3, "one look per read");
        assert_eq!(receive_timeout(b.as_fd()), Some(Duration::from_millis(20)));
        drop(a);
    }

    /// Catches a receive that gives up when a signal interrupts it. With a receive
    /// timeout set, `recvmsg` is never restarted after a signal handler runs (Linux
    /// returns EINTR even under `SA_RESTART`): the read must be retried, to the same
    /// deadline, not reported as an error. The first look at what is left allows 30 s
    /// (a safety bound: nothing arrives, so only a signal ends that read); a signaller
    /// interrupts it until the read has looked again, and the second look finds the
    /// deadline passed.
    #[test]
    fn a_signal_does_not_end_recv_by() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        extern "C" fn ignore(_: libc::c_int) {}
        // SAFETY: installs a handler that does nothing for a signal no other code of
        // this test binary sends or handles; the struct is zeroed, then filled.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = ignore as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&raw mut action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGUSR2, &raw const action, std::ptr::null_mut()),
                0
            );
        }
        // SAFETY: the calling thread's own id; it outlives the signalling thread,
        // which is joined below.
        let me = unsafe { libc::pthread_self() } as usize;
        let (_a, b) = UnixStream::pair().unwrap();
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let signaller = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                // SAFETY: the receiving thread is alive until this thread is joined.
                unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGUSR2) };
                std::thread::yield_now();
            }
        });
        // Readings: the start (0), the first look (0: 30 s left), the second look
        // (after the interrupted read: past the 30 s deadline).
        let clock = Script::new(&[0, 0, 30_000]);
        let now = || clock.now();
        let late = recv_within::<Reply>(b.as_fd(), 0, Duration::from_secs(30), &now).unwrap_err();
        done.store(true, Ordering::SeqCst);
        signaller.join().unwrap();
        assert_eq!(late.kind(), io::ErrorKind::TimedOut, "{late}");
        assert_eq!(
            clock.calls.get(),
            3,
            "the read looked again after the signal"
        );
    }

    /// What a short `sendmsg` leaves is written after it, whole. (A blocking Unix
    /// stream socket rarely sends short, so the remainder path is driven directly.)
    #[test]
    fn the_rest_of_a_short_send_is_written() {
        let (a, mut b) = UnixStream::pair().unwrap();
        write_all(a.as_fd(), b"rest of the frame").unwrap();
        drop(a);
        let mut got = String::new();
        std::io::Read::read_to_string(&mut b, &mut got).unwrap();
        assert_eq!(got, "rest of the frame");
        let big = Reply::Refused {
            reason: "y".repeat(MAX_FRAME - 64),
        };
        let (a, b) = UnixStream::pair().unwrap();
        let reader = std::thread::spawn(move || recv::<Reply>(b.as_fd(), 0).unwrap().unwrap().0);
        send(a.as_fd(), &big, &[]).unwrap();
        assert_eq!(reader.join().unwrap(), big);
    }

    /// Only descriptors are taken from ancillary data (credentials, which the socket
    /// never asks for, are not descriptors).
    #[cfg(target_os = "linux")]
    #[test]
    fn credentials_are_not_descriptors() {
        let credentials = rustix::net::UCred {
            pid: rustix::process::getpid(),
            uid: rustix::process::getuid(),
            gid: rustix::process::getgid(),
        };
        assert!(rights(RecvAncillaryMessage::ScmCredentials(credentials)).is_none());
    }

    #[test]
    fn the_wire_names_are_kebab_case() {
        let text = serde_json::to_string(&Request::UserCreate {
            lease: "1.1".to_owned(),
            grant: None,
        })
        .unwrap();
        assert_eq!(text, r#"{"verb":"user-create","lease":"1.1"}"#);
        let text = serde_json::to_string(&Reply::Deleted { existed: true }).unwrap();
        assert_eq!(text, r#"{"deleted":{"existed":true}}"#);
        let text = serde_json::to_string(&Request::RebootDirty).unwrap();
        assert_eq!(text, r#"{"verb":"reboot-dirty"}"#);
        let classes = vec![("lease-users".to_owned(), 1)];
        let text = serde_json::to_string(&Reply::Baselined { classes }).unwrap();
        assert_eq!(text, r#"{"baselined":{"classes":[["lease-users",1]]}}"#);
    }

    /// Catches: role verbs that change the wire of the lease verbs, or whose names
    /// differ from the ones the daemon sends; and a `role-run` that needs an `env` an
    /// older sender leaves out.
    #[test]
    fn the_role_verbs_are_their_own_verbs() {
        let text = serde_json::to_string(&Request::RoleSessionReady).unwrap();
        assert_eq!(text, r#"{"verb":"role-session-ready"}"#);
        let text = serde_json::to_string(&Request::RoleKillUid).unwrap();
        assert_eq!(text, r#"{"verb":"role-kill-uid"}"#);
        let reap = Request::RoleReap {
            pgid: 4242,
            start: 1_700_000_000_123_456,
            boot: "B0".to_owned(),
        };
        let text = serde_json::to_string(&reap).unwrap();
        assert_eq!(
            text,
            r#"{"verb":"role-reap","pgid":4242,"start":1700000000123456,"boot":"B0"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), reap);
        let run = Request::RoleRun {
            argv: vec!["/usr/bin/true".to_owned()],
            env: Vec::new(),
        };
        let text = serde_json::to_string(&run).unwrap();
        assert_eq!(
            text,
            r#"{"verb":"role-run","argv":["/usr/bin/true"],"env":[]}"#
        );
        let bare = r#"{"verb":"role-run","argv":["/usr/bin/true"]}"#;
        assert_eq!(serde_json::from_str::<Request>(bare).unwrap(), run);
    }

    /// Catches: an image verb that can carry a program, an argument vector or a
    /// guest binary path from the caller (the helper must run only its own installed
    /// `kbf-vmm`, with argv it builds), or whose names differ from the daemon's.
    #[test]
    fn an_image_verb_carries_paths_and_nothing_to_run() {
        let setup = Request::ImageSetup {
            bundle: "/images/base".to_owned(),
        };
        let text = serde_json::to_string(&setup).unwrap();
        assert_eq!(text, r#"{"verb":"image-setup","bundle":"/images/base"}"#);
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), setup);
        let harvest = Request::ImageHarvest {
            bundle: "/images/clone".to_owned(),
            out: "/images/out".to_owned(),
        };
        let text = serde_json::to_string(&harvest).unwrap();
        assert_eq!(
            text,
            r#"{"verb":"image-harvest","bundle":"/images/clone","out":"/images/out"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), harvest);
        for hostile in [
            r#"{"verb":"image-setup","bundle":"/images/b","program":"/bin/sh"}"#,
            r#"{"verb":"image-setup","bundle":"/images/b","argv":["/bin/sh"]}"#,
            r#"{"verb":"image-setup","bundle":"/images/b","guest-binary":"/tmp/g"}"#,
            r#"{"verb":"image-harvest","bundle":"/images/b","out":"/images/o","args":[]}"#,
        ] {
            assert!(
                serde_json::from_str::<Request>(hostile).is_err(),
                "{hostile}"
            );
        }
        let text = serde_json::to_string(&Reply::Imaged {
            report: "{}".to_owned(),
        })
        .unwrap();
        assert_eq!(text, r#"{"imaged":{"report":"{}"}}"#);
    }

    /// Catches a `run` that changes on the wire for the daemons that predate the
    /// session target (a background run must still send, and read, no `session`), and a
    /// GUI run whose target is lost or misnamed.
    #[test]
    fn a_run_names_its_session_only_when_it_is_the_gui() {
        let background = serde_json::to_string(&run_request()).unwrap();
        assert!(!background.contains("session"), "{background}");
        let gui = Request::Run {
            lease: "1.2".to_owned(),
            argv: vec!["/usr/bin/true".to_owned()],
            env: vec![("A".to_owned(), "b".to_owned())],
            session: Session::Gui,
        };
        let text = serde_json::to_string(&gui).unwrap();
        assert!(text.ends_with(r#","session":"gui"}"#), "{text}");
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), gui);
        assert_eq!(
            serde_json::from_str::<Request>(&background).unwrap(),
            run_request()
        );
        let ready = serde_json::to_string(&Request::SessionReady {
            lease: "1.2".to_owned(),
        })
        .unwrap();
        assert_eq!(ready, r#"{"verb":"session-ready","lease":"1.2"}"#);
        let answer = serde_json::to_string(&Reply::SessionReady { ready: true }).unwrap();
        assert_eq!(answer, r#"{"session-ready":{"ready":true}}"#);
    }
}
