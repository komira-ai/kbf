//! The host's end of the results channel: frames a guest sends over virtio-socket
//! (`kbf_guest::frame`) become files in the VM's results directory, exactly where the
//! probes read them from the share (`ready-system`, `guest-probe-root.json`, ...).
//!
//! The guest is not trusted. A frame names a file; the name is checked before any path is
//! made from it (no separator, no leading dot, no `..`, bounded length), the file is made
//! under a name no guest name can have and renamed into place (so a reader never sees half
//! of it and a link the guest planted is replaced, not followed), a log is capped, and a
//! connection carries a bounded number of frames. [`Limiter`] bounds the connections.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kbf_guest::frame::{Frame, Kind, read_frame, write_frame};

/// The most connections served at once; a ninth is rejected.
pub const MAX_CONNECTIONS: usize = 8;
/// The most frames one connection may carry.
pub const MAX_FRAMES_PER_CONNECTION: u32 = 256;
/// The longest file name a frame may name.
pub const MAX_NAME_BYTES: usize = 100;
/// The most one `<name>.log` file may hold.
pub const MAX_LOG_BYTES: u64 = 4 << 20;

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

/// Writes `frame` under `dir` and returns the file's path.
///
/// `ready` and `result` frames replace the file `<name>`; a `log` frame is appended to
/// `<name>.log` up to [`MAX_LOG_BYTES`].
///
/// # Errors
/// The kind is a reply kind, the name is unsafe, the log is full, or the file cannot be
/// written (the text says which).
pub fn store(dir: &Path, frame: &Frame) -> Result<PathBuf, String> {
    check_name(&frame.name)?;
    match frame.kind {
        Kind::Ready | Kind::Result => {
            let target = dir.join(&frame.name);
            let part = dir.join(part_name(&frame.name));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&part)
                .map_err(|e| format!("create {}: {e}", part.display()))?;
            let written = file
                .write_all(frame.body.as_bytes())
                .and_then(|()| std::fs::rename(&part, &target));
            if let Err(e) = written {
                let _ = std::fs::remove_file(&part);
                return Err(format!("write {}: {e}", target.display()));
            }
            Ok(target)
        }
        Kind::Log => {
            let target = dir.join(format!("{}.log", frame.name));
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&target)
                .map_err(|e| format!("open {}: {e}", target.display()))?;
            let meta = file
                .metadata()
                .map_err(|e| format!("stat {}: {e}", target.display()))?;
            if !meta.is_file() {
                return Err(format!("{} is not a regular file", target.display()));
            }
            if meta.len() + frame.body.len() as u64 > MAX_LOG_BYTES {
                return Err(format!(
                    "{} would pass {MAX_LOG_BYTES} bytes",
                    target.display()
                ));
            }
            file.write_all(frame.body.as_bytes())
                .map_err(|e| format!("write {}: {e}", target.display()))?;
            Ok(target)
        }
        Kind::Ack | Kind::Refused => Err("a guest sends ready, result and log frames".to_owned()),
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

/// Answers the frames on `stream` until it ends: each is stored under `dir` and answered
/// with an `ack`, or answered with `refused` and the reason. A frame that cannot be read
/// ends the connection (the stream cannot be resynchronised) after one `refused` reply.
/// Every frame and failure is logged to `log`.
pub fn serve<S: Read + Write>(stream: &mut S, dir: &Path, log: &mut dyn Write) -> Served {
    let mut served = Served {
        stored: 0,
        refused: 0,
        end: End::TooManyFrames,
    };
    for _ in 0..MAX_FRAMES_PER_CONNECTION {
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
        let reply = match store(dir, &frame) {
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

    fn dir(name: &str) -> kbf_testkit::dir::TestDir {
        kbf_testkit::dir::TestDir::under(std::env::temp_dir(), &format!("kbf-vmm-sink-{name}"))
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

    /// Catches a ready or result frame that does not land at `<results>/<name>` with the
    /// body byte for byte, one that leaves its temporary file, and one that does not
    /// replace an earlier file (a retried marker).
    #[test]
    fn ready_and_result_frames_become_the_files_the_probes_read() {
        let d = dir("files");
        let p = store(&d, &Frame::new(Kind::Ready, "ready-system", "{\"a\": 1}\n")).expect("ready");
        assert_eq!(p, d.join("ready-system"));
        assert_eq!(std::fs::read_to_string(&p).expect("read"), "{\"a\": 1}\n");
        store(&d, &Frame::new(Kind::Result, "ready-system", "second")).expect("again");
        assert_eq!(std::fs::read_to_string(&p).expect("read"), "second");
        store(&d, &Frame::new(Kind::Result, "guest-probe-root.json", "{}")).expect("result");
        let mut names: Vec<_> = std::fs::read_dir(&*d)
            .expect("dir")
            .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["guest-probe-root.json", "ready-system"]);
    }

    /// Catches a frame whose name escapes the directory being written outside it, and the
    /// wrong kinds being stored: nothing appears in the directory or its parent.
    #[test]
    fn a_frame_cannot_write_outside_the_results_directory() {
        let d = dir("escape");
        let results = d.join("results");
        std::fs::create_dir(&results).expect("results");
        for name in ["../escaped", "a/../../escaped", "/tmp/escaped", ".part"] {
            for kind in [Kind::Ready, Kind::Result, Kind::Log] {
                assert!(
                    store(&results, &Frame::new(kind, name, "x")).is_err(),
                    "{name}"
                );
            }
        }
        for kind in [Kind::Ack, Kind::Refused] {
            let e = store(&results, &Frame::new(kind, "ok", "x")).expect_err("kind");
            assert!(e.contains("ready, result and log"), "{e}");
        }
        assert_eq!(std::fs::read_dir(&results).expect("dir").count(), 0);
        assert_eq!(
            std::fs::read_dir(&*d).expect("dir").count(),
            1,
            "only results/"
        );
    }

    /// Catches a symbolic link in the results directory being followed: a marker over a link
    /// replaces the link, and a log through a link is refused, so the link's target
    /// is never written.
    #[test]
    fn a_link_the_guest_planted_is_replaced_or_refused_never_followed() {
        let d = dir("link");
        let target = d.join("victim");
        std::fs::write(&target, "keep").expect("victim");
        std::os::unix::fs::symlink(&target, d.join("ready-system")).expect("link");
        std::os::unix::fs::symlink(&target, d.join("x.log")).expect("link");
        store(&d, &Frame::new(Kind::Ready, "ready-system", "new")).expect("replaced");
        assert_eq!(std::fs::read_to_string(&target).expect("victim"), "keep");
        assert_eq!(
            std::fs::read_to_string(d.join("ready-system")).expect("m"),
            "new"
        );
        assert!(!d.join("ready-system").is_symlink());
        assert!(store(&d, &Frame::new(Kind::Log, "x", "boom")).is_err());
        assert_eq!(std::fs::read_to_string(&target).expect("victim"), "keep");
    }

    /// Catches a log that replaces instead of appends, one that grows without bound, and a
    /// log aimed at something that is not a file.
    #[test]
    fn logs_append_up_to_a_cap_and_only_to_files() {
        let d = dir("log");
        let log = |s: &str| store(&d, &Frame::new(Kind::Log, "vsock-test", s));
        log("one\n").expect("one");
        let p = log("two\n").expect("two");
        assert_eq!(p, d.join("vsock-test.log"));
        assert_eq!(std::fs::read_to_string(&p).expect("read"), "one\ntwo\n");
        let fill = "x".repeat(usize::try_from(MAX_LOG_BYTES).expect("fits") - 8);
        log(&fill).expect("fills up to the cap");
        let e = log("overflow").expect_err("over the cap");
        assert!(e.contains("would pass"), "{e}");
        std::fs::create_dir(d.join("dir.log")).expect("dir");
        assert!(store(&d, &Frame::new(Kind::Log, "dir", "x")).is_err());
        // A FIFO with no reader cannot be opened for writing without blocking; with a reader
        // it opens, and is refused as not a regular file.
        let fifo = std::ffi::CString::new(d.join("pipe.log").to_str().expect("p")).expect("c");
        // SAFETY: `fifo` is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let e = store(&d, &Frame::new(Kind::Log, "pipe", "x")).expect_err("no reader");
        assert!(e.contains("open"), "{e}");
        let _reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(d.join("pipe.log"))
            .expect("reader");
        let e = store(&d, &Frame::new(Kind::Log, "pipe", "x")).expect_err("fifo");
        assert!(e.contains("not a regular file"), "{e}");
    }

    /// Catches a write that fails halfway leaving its temporary file behind, and the error
    /// naming nothing: a directory where the marker goes makes the rename fail.
    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        let d = dir("fail");
        std::fs::create_dir(d.join("ready-system")).expect("blocker");
        std::fs::write(d.join("ready-system").join("x"), "x").expect("x");
        let e = store(&d, &Frame::new(Kind::Ready, "ready-system", "m")).expect_err("rename");
        assert!(e.starts_with("write "), "{e}");
        assert!(e.contains("ready-system"), "{e}");
        assert_eq!(
            std::fs::read_dir(&*d).expect("dir").count(),
            1,
            "no .part left"
        );
        let missing = d.join("none");
        let e = store(&missing, &Frame::new(Kind::Ready, "r", "m")).expect_err("no dir");
        assert!(e.contains("create"), "{e}");
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
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.fail_writes {
                return Err(std::io::Error::other("gone"));
            }
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Catches a frame that is stored without an `ack`, a refused frame that is acked or
    /// that stops the stream, and counters or log lines that disagree with what happened.
    #[test]
    fn each_frame_is_answered_and_the_connection_ends_when_the_guest_closes() {
        let d = dir("serve");
        let mut pipe = Pipe::with(&[
            Frame::new(Kind::Ready, "ready-login", "{}"),
            Frame::new(Kind::Result, "../evil", "x"),
            Frame::new(Kind::Log, "vsock-test", "hello\n"),
        ]);
        let mut log = Vec::new();
        let served = serve(&mut pipe, &d, &mut log);
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
        assert!(d.join("ready-login").exists());
        assert!(d.join("vsock-test.log").exists());
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
            let served = serve(&mut pipe, &d, &mut Vec::new());
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
        assert_eq!(std::fs::read_dir(&*d).expect("dir").count(), 0);
    }

    /// Catches a connection that is served without end: after the per-connection frame
    /// limit it stops, and one whose replies cannot be written stops too.
    #[test]
    fn a_connection_is_bounded_in_frames_and_ends_when_replies_fail() {
        let d = dir("bound");
        let many: Vec<Frame> = (0..=MAX_FRAMES_PER_CONNECTION)
            .map(|_| Frame::new(Kind::Log, "spam", "x"))
            .collect();
        let mut pipe = Pipe::with(&many);
        let served = serve(&mut pipe, &d, &mut Vec::new());
        assert_eq!(
            (served.stored, served.end),
            (MAX_FRAMES_PER_CONNECTION, End::TooManyFrames)
        );
        let mut pipe = Pipe::with(&[Frame::new(Kind::Ready, "r", "{}")]);
        pipe.fail_writes = true;
        let served = serve(&mut pipe, &d, &mut Vec::new());
        assert_eq!((served.stored, served.end), (1, End::ReplyFailed));
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
