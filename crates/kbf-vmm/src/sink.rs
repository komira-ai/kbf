//! The host's end of the results channel: frames a guest sends over virtio-socket
//! (`kbf_guest::frame`) become files in the VM's results directory, exactly where the
//! probes read them from the share (`ready-system`, `guest-probe-root.json`, ...).
//!
//! The guest is not trusted, and the results directory is on the host, outside every
//! directory the guest can write to (a share the guest can write would let it swap the
//! directory for a link and send the host's writes anywhere). [`Results`] opens it once,
//! without following a link, and makes every file through that descriptor (`openat`,
//! `renameat`), so nothing the guest does to a path can redirect a write.
//!
//! A frame names a file; the name is checked before any path is made from it (no
//! separator, no leading dot, no `..`, bounded length), the file is made under a name no
//! guest name can have and renamed into place (so a reader never sees half of it), a log
//! is capped, and the whole VM may create only [`MAX_FILES`] files and [`MAX_TOTAL_BYTES`]
//! bytes. A connection carries a bounded number of frames, each within a deadline.
//! [`Limiter`] bounds the connections.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kbf_guest::frame::{Frame, Kind, read_frame, write_frame};

/// The most connections served at once; a ninth is rejected.
pub const MAX_CONNECTIONS: usize = 8;
/// The most frames one connection may carry.
pub const MAX_FRAMES_PER_CONNECTION: u32 = 256;
/// The longest file name a frame may name.
pub const MAX_NAME_BYTES: usize = 100;
/// The most one `<name>.log` file may hold.
pub const MAX_LOG_BYTES: u64 = 4 << 20;
/// The most distinct files one VM may create in its results directory.
pub const MAX_FILES: usize = 64;
/// The most bytes all of one VM's files may hold together.
pub const MAX_TOTAL_BYTES: u64 = 64 << 20;
/// The longest one frame may take to arrive, from the wait for its first byte to its last.
/// A single read is bounded too ([`READ_TIMEOUT_SECS`]), but a guest that sends a byte
/// just inside that bound would hold a slot for ever without this.
pub const FRAME_DEADLINE: Duration = Duration::from_secs(30);
/// The longest one read or write on a connection may block.
pub const READ_TIMEOUT_SECS: libc::time_t = 5;

/// Whether `name` is safe to make a file name of.
///
/// # Errors
/// Why it is not.
pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the name is empty".to_owned());
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(format!("the name is longer than {MAX_NAME_BYTES} bytes"));
    }
    if name.starts_with('.') || name.contains("..") {
        return Err("the name starts with a dot or holds `..`".to_owned());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err("the name may hold only letters, digits, `-`, `_` and `.`".to_owned());
    }
    Ok(())
}

/// A name that no checked name can equal (it starts with a dot), unique in this process.
fn part_name(name: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(".{name}.{}.part", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// The state the budget and the log caps are judged on, held under one lock so that two
/// connections writing the same name cannot both pass a check that only one may.
#[derive(Debug, Default)]
struct Spent {
    /// The bytes each file made so far holds, by file name.
    files: HashMap<String, u64>,
    total: u64,
}

/// A VM's results directory on the host, opened once; every file is made through the
/// descriptor.
#[derive(Debug)]
pub struct Results {
    dir: File,
    path: PathBuf,
    spent: Mutex<Spent>,
}

/// `openat(2)` of `name` under `dir`, without following a link at the leaf.
fn open_at(dir: RawFd, name: &str, flags: libc::c_int) -> io::Result<File> {
    let name = CString::new(name).map_err(io::Error::other)?;
    // SAFETY: `dir` is a descriptor `Results` keeps open and `name` is NUL-terminated and
    // outlives the call; the returned descriptor is owned by the `File` made from it.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            libc::c_uint::from(0o600_u16),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by openat(2) and nothing else owns it.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn rename_at(dir: RawFd, from: &str, to: &str) -> io::Result<()> {
    let (from, to) = (
        CString::new(from).map_err(io::Error::other)?,
        CString::new(to).map_err(io::Error::other)?,
    );
    // SAFETY: both are NUL-terminated and outlive the call; `dir` stays open.
    let rc = unsafe { libc::renameat(dir, from.as_ptr(), dir, to.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn unlink_at(dir: RawFd, name: &str) {
    // A name with a NUL cannot have been created, so there is nothing to remove for it.
    let _ = CString::new(name).map(|name| {
        // SAFETY: `name` is NUL-terminated and outlives the call; `dir` stays open.
        unsafe { libc::unlinkat(dir, name.as_ptr(), 0) }
    });
}

/// Whether a directory with owner `uid` and `mode` may hold results for the user `euid`:
/// it must be theirs and closed to everyone else.
fn check_private(path: &Path, uid: u32, euid: u32, mode: u32) -> Result<(), String> {
    if uid != euid {
        return Err(format!("{} is not owned by this user", path.display()));
    }
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is open to other users (mode {:o}); it must be 0700",
            path.display(),
            mode & 0o777
        ));
    }
    Ok(())
}

impl Results {
    /// Opens `path`, which must be a directory (not a link to one), owned by this user and
    /// closed to everyone else (mode 0700).
    ///
    /// # Errors
    /// It cannot be opened, is a link, or is not private.
    pub fn open(path: &Path) -> Result<Self, String> {
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| format!("open the results directory {}: {e}", path.display()))?;
        let meta = dir
            .metadata()
            .map_err(|e| format!("stat {}: {e}", path.display()))?;
        // SAFETY: geteuid takes no arguments and cannot fail.
        let euid = unsafe { libc::geteuid() };
        check_private(path, meta.uid(), euid, meta.mode())?;
        Ok(Self {
            dir,
            path: path.to_owned(),
            spent: Mutex::new(Spent::default()),
        })
    }

    /// The directory's path, for messages.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `frame` and returns the file's path.
    ///
    /// `ready` and `result` frames replace the file `<name>`; a `log` frame is appended to
    /// `<name>.log` up to [`MAX_LOG_BYTES`]. At most [`MAX_FILES`] files and
    /// [`MAX_TOTAL_BYTES`] bytes in all.
    ///
    /// # Errors
    /// The kind is a reply kind, the name is unsafe, a cap or the budget is spent, or the
    /// file cannot be written (the text says which).
    pub fn store(&self, frame: &Frame) -> Result<PathBuf, String> {
        check_name(&frame.name)?;
        let fd = self.dir.as_raw_fd();
        let len = frame.body.len() as u64;
        // One lock for the whole write: the checks and the write are one step.
        let mut spent = self
            .spent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match frame.kind {
            Kind::Ready | Kind::Result => {
                let old = spent.files.get(&frame.name).copied();
                spent.admit(&frame.name, old, len)?;
                let part = part_name(&frame.name);
                let target = self.path.join(&frame.name);
                let mut file = open_at(fd, &part, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
                    .map_err(|e| format!("create {}: {e}", self.path.join(&part).display()))?;
                let written = file
                    .write_all(frame.body.as_bytes())
                    .and_then(|()| rename_at(fd, &part, &frame.name));
                if let Err(e) = written {
                    unlink_at(fd, &part);
                    return Err(format!("write {}: {e}", target.display()));
                }
                spent.record(&frame.name, old, len);
                Ok(target)
            }
            Kind::Log => {
                let file_name = format!("{}.log", frame.name);
                let target = self.path.join(&file_name);
                let old = spent.files.get(&file_name).copied();
                if old.unwrap_or(0) + len > MAX_LOG_BYTES {
                    return Err(format!(
                        "{} would pass {MAX_LOG_BYTES} bytes",
                        target.display()
                    ));
                }
                spent.admit_growth(&file_name, old, len)?;
                let mut file = open_at(
                    fd,
                    &file_name,
                    libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_NONBLOCK,
                )
                .map_err(|e| format!("open {}: {e}", target.display()))?;
                let meta = file
                    .metadata()
                    .map_err(|e| format!("stat {}: {e}", target.display()))?;
                if !meta.is_file() {
                    return Err(format!("{} is not a regular file", target.display()));
                }
                file.write_all(frame.body.as_bytes())
                    .map_err(|e| format!("write {}: {e}", target.display()))?;
                spent.files.insert(file_name, old.unwrap_or(0) + len);
                spent.total += len;
                Ok(target)
            }
            Kind::Shutdown | Kind::Ack | Kind::Refused => {
                Err("a guest sends ready, result and log frames".to_owned())
            }
        }
    }
}

impl Spent {
    /// Whether a file `name` that held `old` bytes may be replaced by one of `new`.
    fn admit(&self, name: &str, old: Option<u64>, new: u64) -> Result<(), String> {
        self.check(name, old.is_none(), self.total - old.unwrap_or(0) + new)
    }

    /// Whether a file `name` that held `old` bytes may grow by `by`.
    fn admit_growth(&self, name: &str, old: Option<u64>, by: u64) -> Result<(), String> {
        self.check(name, old.is_none(), self.total + by)
    }

    fn check(&self, name: &str, is_new: bool, total_after: u64) -> Result<(), String> {
        if is_new && self.files.len() >= MAX_FILES {
            return Err(format!(
                "the budget of {MAX_FILES} files is spent; {name} not stored"
            ));
        }
        if total_after > MAX_TOTAL_BYTES {
            return Err(format!(
                "the budget of {MAX_TOTAL_BYTES} bytes is spent; {name} not stored"
            ));
        }
        Ok(())
    }

    fn record(&mut self, name: &str, old: Option<u64>, new: u64) {
        self.total = self.total - old.unwrap_or(0) + new;
        self.files.insert(name.to_owned(), new);
    }
}

/// A stream whose reads fail once the current frame's deadline has passed; the deadline
/// is set again at the start of each frame.
struct Deadline<'a, S> {
    inner: &'a mut S,
    frame: Duration,
    /// The time since an origin; the real one is the monotonic clock, a test passes its own.
    now: &'a dyn Fn() -> Duration,
    until: Duration,
}

impl<S> Deadline<'_, S> {
    fn start_frame(&mut self) {
        self.until = (self.now)() + self.frame;
    }
}

impl<S: Read> Read for Deadline<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.now)() >= self.until {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the frame did not arrive in time",
            ));
        }
        self.inner.read(buf)
    }
}

impl<S: Write> Write for Deadline<'_, S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Why [`serve`] stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The guest closed the connection between two frames.
    Closed,
    /// A frame could not be read (too large, cut, not a frame, a stream error).
    BadFrame,
    /// A reply could not be written.
    ReplyFailed,
    /// The connection carried [`MAX_FRAMES_PER_CONNECTION`] frames and wanted more.
    TooManyFrames,
}

/// What one connection did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Served {
    /// Frames written to files.
    pub stored: u32,
    /// Frames refused (unsafe name, wrong kind, a write that failed).
    pub refused: u32,
    pub end: End,
}

/// Answers the frames on `stream` until it ends: each is stored in `results` and answered
/// with an `ack`, or answered with `refused` and the reason. A frame that cannot be read
/// ends the connection (the stream cannot be resynchronised) after one `refused` reply.
/// Every frame and failure is logged to `log`.
pub fn serve<S: Read + Write>(stream: &mut S, results: &Results, log: &mut dyn Write) -> Served {
    let origin = Instant::now();
    serve_within(stream, results, log, FRAME_DEADLINE, &|| origin.elapsed())
}

/// [`serve`] with each frame due within `frame`.
fn serve_within<S: Read + Write>(
    stream: &mut S,
    results: &Results,
    log: &mut dyn Write,
    frame: Duration,
    now: &dyn Fn() -> Duration,
) -> Served {
    let mut stream = Deadline {
        inner: stream,
        frame,
        now,
        until: now() + frame,
    };
    let stream = &mut stream;
    let mut served = Served {
        stored: 0,
        refused: 0,
        end: End::TooManyFrames,
    };
    for _ in 0..MAX_FRAMES_PER_CONNECTION {
        stream.start_frame();
        let frame = match read_frame(stream) {
            Ok(Some(f)) => f,
            Ok(None) => {
                served.end = End::Closed;
                return served;
            }
            Err(e) => {
                let _ = writeln!(log, "kbf-vmm: vsock frame refused: {e}");
                let _ = write_frame(stream, &Frame::new(Kind::Refused, "", &e.to_string()));
                served.end = End::BadFrame;
                return served;
            }
        };
        let reply = match results.store(&frame) {
            Ok(path) => {
                let _ = writeln!(
                    log,
                    "kbf-vmm: vsock {:?} {} -> {} ({} bytes)",
                    frame.kind,
                    frame.name,
                    path.display(),
                    frame.body.len()
                );
                served.stored += 1;
                Frame::new(Kind::Ack, &frame.name, "")
            }
            Err(why) => {
                let _ = writeln!(
                    log,
                    "kbf-vmm: vsock {:?} {:?} refused: {why}",
                    frame.kind, frame.name
                );
                served.refused += 1;
                Frame::new(Kind::Refused, &frame.name, &why)
            }
        };
        if write_frame(stream, &reply).is_err() {
            served.end = End::ReplyFailed;
            return served;
        }
    }
    served
}

/// Sets the read and write timeouts of the connection `fd` to [`READ_TIMEOUT_SECS`].
///
/// # Errors
/// `setsockopt` failed; the connection must not be read without a timeout.
pub fn set_io_timeouts(fd: RawFd) -> io::Result<()> {
    let timeout = libc::timeval {
        tv_sec: READ_TIMEOUT_SECS,
        tv_usec: 0,
    };
    for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        // SAFETY: `timeout` is a `timeval` and its size is passed.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                std::ptr::from_ref(&timeout).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Serves the connection on `fd`, which the caller keeps open (it is not closed here).
/// The connection is refused, unread, when its timeouts cannot be set.
///
/// UNCERTAIN until a Mac ran it, and it did: the descriptor of a
/// `VZVirtioSocketConnection` is an ordinary stream socket that honours the timeouts.
///
/// # Errors
/// The timeouts could not be set.
pub fn serve_socket(fd: RawFd, results: &Results, log: &mut dyn Write) -> Result<Served, String> {
    set_io_timeouts(fd).map_err(|e| format!("connection refused, timeouts not set: {e}"))?;
    // SAFETY: the caller owns `fd` and keeps it open for this call, so the `File` must
    // never close it: `ManuallyDrop` keeps it from doing so.
    let mut stream = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    Ok(serve(&mut *stream, results, log))
}

/// Bounds how many connections are served at once.
#[derive(Debug, Clone)]
pub struct Limiter {
    open: Arc<AtomicUsize>,
    max: usize,
}

/// One served connection; dropping it frees the slot.
#[derive(Debug)]
pub struct Permit(Arc<AtomicUsize>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Limiter {
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            open: Arc::new(AtomicUsize::new(0)),
            max,
        }
    }

    /// A slot, or `None` when `max` connections are open.
    #[must_use]
    pub fn try_acquire(&self) -> Option<Permit> {
        self.open
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < self.max).then_some(n + 1)
            })
            .ok()
            .map(|_| Permit(self.open.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    fn dir(name: &str) -> kbf_testkit::dir::TestDir {
        kbf_testkit::dir::TestDir::under(std::env::temp_dir(), &format!("kbf-vmm-sink-{name}"))
    }

    /// A private results directory `results` under `d`, opened.
    fn open(d: &Path) -> (PathBuf, Results) {
        let path = d.join("results");
        std::fs::create_dir(&path).expect("results");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("mode");
        let r = Results::open(&path).expect("open");
        (path, r)
    }

    fn names(p: &Path) -> Vec<String> {
        let mut v: Vec<_> = std::fs::read_dir(p)
            .expect("dir")
            .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// Catches a name that could leave the results directory or collide with the sink's
    /// own files: separators, a leading dot, `..`, NUL, spaces, unicode, empty, too long.
    #[test]
    fn only_plain_file_names_are_accepted() {
        for ok in ["ready-system", "guest-probe-root.json", "a_b-C.9", "x.log"] {
            assert_eq!(check_name(ok), Ok(()), "{ok}");
        }
        let long = "a".repeat(MAX_NAME_BYTES + 1);
        for bad in [
            "",
            "../x",
            "a/b",
            "/etc/passwd",
            "..",
            ".hidden",
            "a..b",
            "a\\b",
            "a\0b",
            "a b",
            "é",
            long.as_str(),
        ] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(check_name(&"a".repeat(MAX_NAME_BYTES)), Ok(()));
    }

    /// Catches a results directory that a guest or another user could write to, or one
    /// that is a link: it is refused when it is open to others, owned by someone else's
    /// mode, a link to a private directory, missing, or a file.
    #[test]
    fn the_results_directory_must_be_a_private_directory_and_not_a_link() {
        let d = dir("open");
        let open_dir = d.join("open");
        std::fs::create_dir(&open_dir).expect("dir");
        for mode in [0o755, 0o770, 0o707, 0o777] {
            std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(mode))
                .expect("mode");
            let e = Results::open(&open_dir).expect_err("open to others");
            assert!(e.contains("must be 0700"), "{e}");
        }
        let (real, _) = open(&d);
        std::os::unix::fs::symlink(&real, d.join("link")).expect("link");
        let e = Results::open(&d.join("link")).expect_err("a link");
        assert!(e.contains("open the results directory"), "{e}");
        assert!(Results::open(&d.join("none")).is_err());
        std::fs::write(d.join("file"), "x").expect("file");
        assert!(Results::open(&d.join("file")).is_err());
    }

    /// Catches a directory of another user being taken as ours, which the mode check alone
    /// would pass: the owner must be the user the sink runs as.
    #[test]
    fn a_directory_owned_by_someone_else_is_not_private() {
        let p = Path::new("/r");
        assert_eq!(check_private(p, 501, 501, 0o40700), Ok(()));
        let e = check_private(p, 0, 501, 0o40700).expect_err("owner");
        assert!(e.contains("not owned"), "{e}");
        let e = check_private(p, 501, 501, 0o40750).expect_err("mode");
        assert!(e.contains("0700"), "{e}");
    }

    /// Catches a ready or result frame that does not land at `<results>/<name>` with the
    /// body byte for byte, one that leaves its temporary file, and one that does not
    /// replace an earlier file (a retried marker).
    #[test]
    fn ready_and_result_frames_become_files_in_the_results_directory() {
        let d = dir("files");
        let (path, r) = open(&d);
        let p = r
            .store(&Frame::new(Kind::Ready, "ready-system", "{\"a\": 1}\n"))
            .expect("ready");
        assert_eq!(p, path.join("ready-system"));
        assert_eq!(std::fs::read_to_string(&p).expect("read"), "{\"a\": 1}\n");
        r.store(&Frame::new(Kind::Result, "ready-system", "second"))
            .expect("again");
        assert_eq!(std::fs::read_to_string(&p).expect("read"), "second");
        r.store(&Frame::new(Kind::Result, "guest-probe-root.json", "{}"))
            .expect("result");
        assert_eq!(names(&path), ["guest-probe-root.json", "ready-system"]);
        let mode = std::fs::metadata(&p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// Catches a frame whose name escapes the directory being written outside it, and the
    /// wrong kinds being stored: nothing appears in the directory or its parent.
    #[test]
    fn a_frame_cannot_write_outside_the_results_directory() {
        let d = dir("escape");
        let (path, r) = open(&d);
        for name in ["../escaped", "a/../../escaped", "/tmp/escaped", ".part"] {
            for kind in [Kind::Ready, Kind::Result, Kind::Log] {
                assert!(r.store(&Frame::new(kind, name, "x")).is_err(), "{name}");
            }
        }
        for kind in [Kind::Shutdown, Kind::Ack, Kind::Refused] {
            let e = r.store(&Frame::new(kind, "ok", "x")).expect_err("kind");
            assert!(e.contains("ready, result and log"), "{e}");
        }
        assert_eq!(names(&path), Vec::<String>::new());
        assert_eq!(names(&d), ["results"]);
    }

    /// Catches the arbitrary host file write the verifier found: a guest that swaps the
    /// results directory for a link to a directory elsewhere (`mv results r.old; ln -s
    /// <dir> results`) after the sink opened it. The sink writes through the descriptor it
    /// opened, so the link's target gets nothing and the frames land in the original.
    #[test]
    fn a_results_directory_swapped_for_a_link_after_opening_is_not_followed() {
        let d = dir("swap");
        let (path, r) = open(&d);
        r.store(&Frame::new(Kind::Ready, "before", "1"))
            .expect("before");
        let outside = d.join("outside");
        std::fs::create_dir(&outside).expect("outside");
        std::fs::rename(&path, d.join("r.old")).expect("move");
        std::os::unix::fs::symlink(&outside, &path).expect("link");
        for kind in [Kind::Result, Kind::Ready, Kind::Log] {
            r.store(&Frame::new(kind, "after", "2")).expect("stored");
        }
        assert_eq!(names(&outside), Vec::<String>::new());
        assert_eq!(names(&d.join("r.old")), ["after", "after.log", "before"]);
    }

    /// Catches a symbolic link in the results directory being followed: a marker over a link
    /// replaces the link, and a log through a link is refused, so the link's target
    /// is never written.
    #[test]
    fn a_link_the_guest_planted_is_replaced_or_refused_never_followed() {
        let d = dir("link");
        let (path, r) = open(&d);
        let target = d.join("victim");
        std::fs::write(&target, "keep").expect("victim");
        std::os::unix::fs::symlink(&target, path.join("ready-system")).expect("link");
        std::os::unix::fs::symlink(&target, path.join("x.log")).expect("link");
        r.store(&Frame::new(Kind::Ready, "ready-system", "new"))
            .expect("replaced");
        assert_eq!(std::fs::read_to_string(&target).expect("victim"), "keep");
        assert_eq!(
            std::fs::read_to_string(path.join("ready-system")).expect("m"),
            "new"
        );
        assert!(!path.join("ready-system").is_symlink());
        assert!(r.store(&Frame::new(Kind::Log, "x", "boom")).is_err());
        assert_eq!(std::fs::read_to_string(&target).expect("victim"), "keep");
    }

    /// Catches a log that replaces instead of appends, one that grows without bound, and a
    /// log aimed at something that is not a file.
    #[test]
    fn logs_append_up_to_a_cap_and_only_to_files() {
        let d = dir("log");
        let (path, r) = open(&d);
        let log = |s: &str| r.store(&Frame::new(Kind::Log, "vsock-test", s));
        log("one\n").expect("one");
        let p = log("two\n").expect("two");
        assert_eq!(p, path.join("vsock-test.log"));
        assert_eq!(std::fs::read_to_string(&p).expect("read"), "one\ntwo\n");
        let fill = "x".repeat(usize::try_from(MAX_LOG_BYTES).expect("fits") - 8);
        log(&fill).expect("fills up to the cap");
        let e = log("overflow").expect_err("over the cap");
        assert!(e.contains("would pass"), "{e}");
        std::fs::create_dir(path.join("dir.log")).expect("dir");
        assert!(r.store(&Frame::new(Kind::Log, "dir", "x")).is_err());
        // A FIFO with no reader cannot be opened for writing without blocking; with a reader
        // it opens, and is refused as not a regular file.
        let fifo = CString::new(path.join("pipe.log").to_str().expect("p")).expect("c");
        // SAFETY: `fifo` is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let e = r
            .store(&Frame::new(Kind::Log, "pipe", "x"))
            .expect_err("no reader");
        assert!(e.contains("open"), "{e}");
        let _reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path.join("pipe.log"))
            .expect("reader");
        let e = r
            .store(&Frame::new(Kind::Log, "pipe", "x"))
            .expect_err("fifo");
        assert!(e.contains("not a regular file"), "{e}");
    }

    /// Catches a guest that fills the host: past the file budget a new name is refused
    /// while an existing one can still be replaced, and past the byte budget nothing
    /// grows, whether by replacing, by a new file or by a log; replacing with a smaller
    /// body gives the bytes back.
    #[test]
    fn a_vm_may_create_only_so_many_files_and_bytes() {
        let d = dir("budget");
        let (path, r) = open(&d);
        for i in 0..MAX_FILES {
            r.store(&Frame::new(Kind::Result, &format!("f{i}"), "x"))
                .expect("within the budget");
        }
        let e = r
            .store(&Frame::new(Kind::Result, "one-too-many", "x"))
            .expect_err("file budget");
        assert!(e.contains("files is spent"), "{e}");
        let e = r
            .store(&Frame::new(Kind::Log, "one-too-many", "x"))
            .expect_err("a log is a file too");
        assert!(e.contains("files is spent"), "{e}");
        r.store(&Frame::new(Kind::Result, "f0", "replaced"))
            .expect("an existing name is not a new file");
        assert_eq!(names(&path).len(), MAX_FILES);

        let d = dir("bytes");
        let (_, r) = open(&d);
        let big = "x".repeat(2 << 20);
        for i in 0..32 {
            r.store(&Frame::new(Kind::Result, &format!("b{i}"), &big))
                .expect("within the byte budget");
        }
        let e = r
            .store(&Frame::new(Kind::Result, "more", "x"))
            .expect_err("byte budget");
        assert!(e.contains("bytes is spent"), "{e}");
        assert!(r.store(&Frame::new(Kind::Log, "b0", "x")).is_err());
        assert!(
            r.store(&Frame::new(Kind::Result, "b0", &format!("{big}x")))
                .is_err()
        );
        r.store(&Frame::new(Kind::Result, "b0", "small"))
            .expect("a smaller body gives bytes back");
        r.store(&Frame::new(Kind::Result, "more", "x"))
            .expect("and the room is usable");
    }

    /// Catches a log cap that two connections can pass together (each checks the size,
    /// both append): many threads appending to one log stop at the cap exactly, and the
    /// file holds what was acknowledged.
    #[test]
    fn concurrent_appends_to_one_log_cannot_pass_the_cap() {
        let d = dir("race");
        let (path, r) = open(&d);
        let r = Arc::new(r);
        let chunk = "y".repeat(1 << 20);
        let stored: usize = (0..16)
            .map(|_| {
                let (r, chunk) = (r.clone(), chunk.clone());
                std::thread::spawn(move || {
                    usize::from(r.store(&Frame::new(Kind::Log, "race", &chunk)).is_ok())
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().expect("thread"))
            .sum();
        assert_eq!(stored as u64, MAX_LOG_BYTES >> 20);
        let size = std::fs::metadata(path.join("race.log")).expect("log").len();
        assert_eq!(size, MAX_LOG_BYTES);
    }

    /// Catches a write that fails halfway leaving its temporary file behind, and the error
    /// naming nothing: a directory where the marker goes makes the rename fail, and a
    /// failed write is not counted against the budget.
    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        let d = dir("fail");
        let (path, r) = open(&d);
        std::fs::create_dir(path.join("ready-system")).expect("blocker");
        std::fs::write(path.join("ready-system").join("x"), "x").expect("x");
        let e = r
            .store(&Frame::new(Kind::Ready, "ready-system", "m"))
            .expect_err("rename");
        assert!(e.starts_with("write "), "{e}");
        assert!(e.contains("ready-system"), "{e}");
        assert_eq!(names(&path), ["ready-system"], "no .part left");
        assert_eq!(r.spent.lock().expect("lock").total, 0);
    }

    /// A stream that reads from `input` and keeps what is written.
    struct Pipe {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
        fail_writes: bool,
    }

    impl Pipe {
        fn with(frames: &[Frame]) -> Self {
            let mut input = Vec::new();
            for f in frames {
                write_frame(&mut input, f).expect("frame");
            }
            Self {
                input: Cursor::new(input),
                output: Vec::new(),
                fail_writes: false,
            }
        }

        fn replies(&self) -> Vec<Frame> {
            let mut c = Cursor::new(self.output.clone());
            let mut v = Vec::new();
            while let Some(f) = read_frame(&mut c).expect("reply") {
                v.push(f);
            }
            v
        }
    }

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail_writes {
                return Err(io::Error::other("gone"));
            }
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Catches a frame that is stored without an `ack`, a refused frame that is acked or
    /// that stops the stream, and counters or log lines that disagree with what happened.
    #[test]
    fn each_frame_is_answered_and_the_connection_ends_when_the_guest_closes() {
        let d = dir("serve");
        let (path, r) = open(&d);
        let mut pipe = Pipe::with(&[
            Frame::new(Kind::Ready, "ready-login", "{}"),
            Frame::new(Kind::Result, "../evil", "x"),
            Frame::new(Kind::Log, "vsock-test", "hello\n"),
        ]);
        let mut log = Vec::new();
        let served = serve(&mut pipe, &r, &mut log);
        assert_eq!(
            served,
            Served {
                stored: 2,
                refused: 1,
                end: End::Closed
            }
        );
        let replies = pipe.replies();
        assert_eq!(replies.len(), 3);
        assert_eq!(replies[0], Frame::new(Kind::Ack, "ready-login", ""));
        assert_eq!(replies[1].kind, Kind::Refused);
        assert_eq!(replies[1].name, "../evil");
        assert!(replies[1].body.contains("dot"), "{}", replies[1].body);
        assert_eq!(replies[2], Frame::new(Kind::Ack, "vsock-test", ""));
        assert!(path.join("ready-login").exists());
        assert!(path.join("vsock-test.log").exists());
        assert!(!d.join("evil").exists());
        let log = String::from_utf8(log).expect("utf8");
        assert!(log.contains("Ready ready-login ->"), "{log}");
        assert!(log.contains("\"../evil\" refused"), "{log}");
    }

    /// Catches bad input that is silently skipped or that panics: an oversized length, a
    /// cut frame and junk each end the connection after one `refused`; nothing is stored.
    #[test]
    fn a_frame_that_cannot_be_read_ends_the_connection_with_a_refusal() {
        let d = dir("bad");
        let (path, r) = open(&d);
        for bytes in [
            vec![0xff, 0xff, 0xff, 0xff],
            vec![0, 0, 0, 9, b'{'],
            vec![0, 0, 0, 3, b'a', b'b', b'c'],
        ] {
            let mut pipe = Pipe {
                input: Cursor::new(bytes),
                output: Vec::new(),
                fail_writes: false,
            };
            let served = serve(&mut pipe, &r, &mut Vec::new());
            assert_eq!(
                served,
                Served {
                    stored: 0,
                    refused: 0,
                    end: End::BadFrame
                }
            );
            let replies = pipe.replies();
            assert_eq!((replies.len(), replies[0].kind), (1, Kind::Refused));
        }
        assert_eq!(names(&path), Vec::<String>::new());
    }

    /// Catches a connection that is served without end: after the per-connection frame
    /// limit it stops, and one whose replies cannot be written stops too.
    #[test]
    fn a_connection_is_bounded_in_frames_and_ends_when_replies_fail() {
        let d = dir("bound");
        let (_, r) = open(&d);
        let many: Vec<Frame> = (0..=MAX_FRAMES_PER_CONNECTION)
            .map(|_| Frame::new(Kind::Log, "spam", "x"))
            .collect();
        let mut pipe = Pipe::with(&many);
        let served = serve(&mut pipe, &r, &mut Vec::new());
        assert_eq!(
            (served.stored, served.end),
            (MAX_FRAMES_PER_CONNECTION, End::TooManyFrames)
        );
        let mut pipe = Pipe::with(&[Frame::new(Kind::Ready, "r", "{}")]);
        pipe.fail_writes = true;
        let served = serve(&mut pipe, &r, &mut Vec::new());
        assert_eq!((served.stored, served.end), (1, End::ReplyFailed));
    }

    /// A stream that hands out one byte of a valid frame per read, each after a pause,
    /// the way a guest that sends a byte just inside the per-read timeout does.
    struct Drip {
        bytes: Vec<u8>,
        at: usize,
        pause: Duration,
        /// The test's clock: every read moves it on by `pause`.
        clock: std::rc::Rc<std::cell::Cell<Duration>>,
        reads: usize,
    }

    impl Read for Drip {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.clock.set(self.clock.get() + self.pause);
            self.reads += 1;
            if self.at >= self.bytes.len() {
                return Ok(0);
            }
            buf[0] = self.bytes[self.at];
            self.at += 1;
            Ok(1)
        }
    }

    impl Write for Drip {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Catches a connection that a slow guest holds for ever: each read is fast enough
    /// for a per-read timeout, but the frame as a whole is not, so it ends as a bad frame
    /// at the deadline with most of the frame unread. A frame that arrives in time
    /// is served by the same code.
    #[test]
    fn a_frame_that_arrives_too_slowly_ends_the_connection_at_the_deadline() {
        let d = dir("drip");
        let (path, r) = open(&d);
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Frame::new(Kind::Ready, "r", "{}")).expect("frame");
        let total = bytes.len();
        let clock = std::rc::Rc::new(std::cell::Cell::new(Duration::ZERO));
        let now = || clock.get();
        let mut slow = Drip {
            bytes: bytes.clone(),
            at: 0,
            pause: Duration::from_millis(20),
            clock: clock.clone(),
            reads: 0,
        };
        let served = serve_within(
            &mut slow,
            &r,
            &mut Vec::new(),
            Duration::from_millis(100),
            &now,
        );
        assert_eq!(served.end, End::BadFrame);
        // Reads at 0, 20, 40, 60 and 80 ms were inside the deadline; the sixth is refused.
        assert_eq!(slow.reads, 5, "reads of {total} bytes");
        assert!(total > 5);
        assert_eq!(names(&path), Vec::<String>::new());
        let mut quick = Drip {
            bytes,
            at: 0,
            pause: Duration::ZERO,
            clock: clock.clone(),
            reads: 0,
        };
        let served = serve_within(
            &mut quick,
            &r,
            &mut Vec::new(),
            Duration::from_secs(30),
            &now,
        );
        assert_eq!((served.stored, served.end), (1, End::Closed));
    }

    /// Catches a connection read with no timeout because `setsockopt` failed (the old code
    /// only printed): on a descriptor that cannot take the options the connection is
    /// refused without reading, and on a real socket the options are set and the frames
    /// are served without closing the caller's descriptor.
    #[test]
    fn a_connection_whose_timeouts_cannot_be_set_is_refused() {
        let d = dir("sock");
        let (path, r) = open(&d);
        let e = serve_socket(-1, &r, &mut Vec::new()).expect_err("bad descriptor");
        assert!(e.contains("timeouts not set"), "{e}");
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` has room for the two descriptors socketpair returns.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0);
        // SAFETY: both descriptors were just returned and are owned here.
        let (mut guest, host) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
        write_frame(&mut guest, &Frame::new(Kind::Ready, "ready-login", "{}")).expect("send");
        guest.flush().expect("flush");
        // SAFETY: shutting down the write side of an open socket.
        assert_eq!(
            unsafe { libc::shutdown(guest.as_raw_fd(), libc::SHUT_WR) },
            0
        );
        let served = serve_socket(host.as_raw_fd(), &r, &mut Vec::new()).expect("served");
        assert_eq!((served.stored, served.end), (1, End::Closed));
        assert!(path.join("ready-login").exists());
        let reply = read_frame(&mut guest).expect("reply").expect("a frame");
        assert_eq!(reply.kind, Kind::Ack);
        // The descriptor was not closed by the serving.
        // SAFETY: fcntl F_GETFD on a descriptor still owned by `host`.
        assert!(unsafe { libc::fcntl(host.as_raw_fd(), libc::F_GETFD) } >= 0);
    }

    /// Catches a limiter that admits more than its maximum, one that does not free a slot
    /// when the connection ends, and clones that count separately.
    #[test]
    fn the_limiter_admits_the_maximum_and_frees_a_slot_on_drop() {
        let limiter = Limiter::new(2);
        let clone = limiter.clone();
        let a = limiter.try_acquire().expect("first");
        let b = clone.try_acquire().expect("second");
        assert!(limiter.try_acquire().is_none());
        drop(a);
        let c = limiter.try_acquire().expect("freed");
        assert!(clone.try_acquire().is_none());
        drop((b, c));
        assert!(Limiter::new(0).try_acquire().is_none());
    }
}
