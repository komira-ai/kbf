//! The guest's half of a clean shutdown driven by the host.
//!
//! `VZVirtualMachine.requestStopWithError` is ignored by a macOS guest resting at the login
//! window (E1 runs 7 and 8: every VM was force-stopped after the shutdown timeout and the
//! data the guest had not flushed was lost). So the host asks over the virtio socket
//! instead: a root LaunchDaemon (`kbf-guest shutdown-listen`, installed by image setup)
//! listens on [`PORT`], the host connects to the guest and sends one frame
//! ([`Kind::Shutdown`], name [`NAME`], empty body), the daemon answers `ack` and runs
//! [`PROGRAM`] with [`ARGS`].
//!
//! # Who may ask
//! On a VM the peer of a virtio-socket connection is the host: the virtio socket device is
//! the guest's only transport to another context, and its other end is the process that
//! runs the VM. The listener therefore carries no credentials and says so here rather than
//! pretending to authenticate; what it accepts is limited instead. It reads exactly one
//! frame of at most [`MAX_COMMAND_FRAME_BYTES`] bytes (refused from the length alone when
//! longer), accepts exactly the `shutdown` command with its name and an empty body, serves
//! one connection at a time under a read and write timeout, and refuses everything else
//! with a `refused` frame, running nothing. (UNCERTAIN until a guest runs it: whether a
//! process inside the guest can connect to its own listener; the command is the same one
//! the host may send, and the guest user could run `shutdown` itself only as root.)
//!
//! The command is run with a fixed program and fixed arguments by `exec`, never through a
//! shell, and nothing in the frame reaches the command line.
//!
//! This module has no socket in it: the connections come from an [`Incoming`] and the
//! program is run by a [`Runner`], so the whole flow runs in unit tests on any OS. The
//! macOS listener is `shutdown_host`.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::frame::{Frame, Kind, read_frame_within, write_frame};

/// The guest vsock port the daemon listens on (the guest-to-host results use 5000).
pub const PORT: u32 = 5001;
/// The name of the command frame.
pub const NAME: &str = "shutdown";
/// The largest frame, header excluded, the listener reads: the command is 48 bytes of
/// JSON. A longer frame is refused from its length alone.
pub const MAX_COMMAND_FRAME_BYTES: usize = 256;
/// The program that shuts the guest down.
pub const PROGRAM: &str = "/sbin/shutdown";
/// Its arguments: halt, now.
pub const ARGS: [&str; 2] = ["-h", "now"];
/// How long the whole frame has to arrive, from the first read. The socket's per-read
/// timeout alone lets a peer holding the one connection slot send a byte every few seconds.
pub const FRAME_DEADLINE: Duration = Duration::from_secs(10);

/// Runs a program by `exec`, no shell.
pub trait Runner {
    /// Runs `program` with `args` and waits for it.
    ///
    /// # Errors
    /// The program could not be started or exited non-zero.
    fn run(&self, program: &str, args: &[&str]) -> Result<(), String>;
}

/// The real [`Runner`]: a child process with no stdin.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<(), String> {
        let status = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .status()
            .map_err(|e| format!("run {program}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{program} exited {status}"))
        }
    }
}

/// Where connections come from.
pub trait Incoming {
    type Conn: Read + Write;

    /// The next connection, waiting for one.
    ///
    /// # Errors
    /// The listener failed; the daemon exits and launchd starts it again.
    fn accept(&mut self) -> Result<Self::Conn, String>;
}

/// The part of the daemon `main` starts: listen on `port` and [`serve`] until the guest is
/// shutting down. The real one exists in a macOS guest only.
pub trait Daemon {
    /// # Errors
    /// The listener could not be set up or failed.
    fn run(&self, port: u32, log: &mut dyn Write) -> Result<(), String>;
}

/// What became of one connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Served {
    /// The command was accepted and the shutdown program ran and succeeded. `acked` says
    /// whether the acknowledgement could be written first (the shutdown runs either way:
    /// the host asked for it).
    Shutdown { acked: bool },
    /// The command was accepted and acknowledged; the shutdown program failed.
    ShutdownFailed(String),
    /// The frame was not the command (or not a frame) and was refused; nothing ran.
    Refused(String),
    /// The peer sent nothing.
    Dropped,
}

/// A reader that fails once its deadline has passed, whatever the stream does.
struct Deadline<'a, S> {
    inner: &'a mut S,
    /// Time since an origin: the monotonic clock in the daemon, a counter in a test.
    now: &'a dyn Fn() -> Duration,
    until: Duration,
}

impl<S: Read> Read for Deadline<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.now)() >= self.until {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the frame did not arrive within its deadline",
            ));
        }
        self.inner.read(buf)
    }
}

/// [`handle_within`] with [`FRAME_DEADLINE`].
pub fn handle<S: Read + Write, R: Runner>(stream: &mut S, runner: &R) -> Served {
    let origin = Instant::now();
    handle_within(stream, runner, FRAME_DEADLINE, &|| origin.elapsed())
}

/// Answers the one frame on `stream`, which has `deadline` to arrive: acknowledges the
/// shutdown command and runs it, or refuses. `now` reads the clock the deadline is
/// measured on.
pub fn handle_within<S: Read + Write, R: Runner>(
    stream: &mut S,
    runner: &R,
    deadline: Duration,
    now: &dyn Fn() -> Duration,
) -> Served {
    let refuse = |stream: &mut S, name: &str, why: String| {
        // Best effort: the peer may be gone.
        let _ = write_frame(stream, &Frame::new(Kind::Refused, name, &why));
        Served::Refused(why)
    };
    let mut reader = Deadline {
        inner: &mut *stream,
        now,
        until: now() + deadline,
    };
    match read_frame_within(&mut reader, MAX_COMMAND_FRAME_BYTES) {
        Ok(Some(frame)) if frame == Frame::new(Kind::Shutdown, NAME, "") => {
            let acked = write_frame(stream, &Frame::new(Kind::Ack, NAME, "")).is_ok();
            match runner.run(PROGRAM, &ARGS) {
                Ok(()) => Served::Shutdown { acked },
                Err(e) => Served::ShutdownFailed(e),
            }
        }
        Ok(Some(frame)) => {
            let why = format!("only the {NAME} command is accepted, not {:?}", frame.kind);
            refuse(stream, &frame.name, why)
        }
        Ok(None) => Served::Dropped,
        Err(e) => refuse(stream, "", e.to_string()),
    }
}

/// Serves connections one at a time until one carries an accepted shutdown that ran, then
/// returns; every outcome is logged to `log`.
///
/// # Errors
/// The listener failed.
pub fn serve<I: Incoming, R: Runner>(
    incoming: &mut I,
    runner: &R,
    log: &mut dyn Write,
) -> Result<(), String> {
    loop {
        let mut conn = incoming.accept()?;
        let served = handle(&mut conn, runner);
        let _ = writeln!(log, "kbf-guest: shutdown-listen: {served:?}");
        if matches!(served, Served::Shutdown { .. }) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::FrameError;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::rc::Rc;

    /// A stream that reads `input` and records what is written. `fail_write` makes every
    /// write fail.
    struct Pipe {
        input: Cursor<Vec<u8>>,
        written: Rc<RefCell<Vec<u8>>>,
        fail_write: bool,
    }

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.fail_write {
                return Err(std::io::Error::other("closed"));
            }
            self.written.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn pipe(frames: &[Frame]) -> (Pipe, Rc<RefCell<Vec<u8>>>) {
        let mut bytes = Vec::new();
        for f in frames {
            write_frame(&mut bytes, f).expect("encode");
        }
        raw_pipe(bytes)
    }

    fn raw_pipe(bytes: Vec<u8>) -> (Pipe, Rc<RefCell<Vec<u8>>>) {
        let written = Rc::new(RefCell::new(Vec::new()));
        let p = Pipe {
            input: Cursor::new(bytes),
            written: written.clone(),
            fail_write: false,
        };
        (p, written)
    }

    /// The frames in `bytes`.
    fn frames_of(bytes: &[u8]) -> Vec<Frame> {
        let mut r = Cursor::new(bytes.to_vec());
        let mut out = Vec::new();
        while let Some(f) = crate::frame::read_frame(&mut r).expect("frame") {
            out.push(f);
        }
        out
    }

    /// A runner that records every call and, at each, how many bytes the peer had been
    /// sent so far (to show the acknowledgement came first).
    struct Fake {
        calls: RefCell<Vec<(String, Vec<String>, usize)>>,
        seen: Rc<RefCell<Vec<u8>>>,
        fail: bool,
    }

    impl Fake {
        fn new(seen: &Rc<RefCell<Vec<u8>>>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                seen: seen.clone(),
                fail: false,
            }
        }
    }

    impl Runner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> Result<(), String> {
            self.calls.borrow_mut().push((
                program.to_owned(),
                args.iter().map(|a| (*a).to_owned()).collect(),
                self.seen.borrow().len(),
            ));
            if self.fail {
                Err("no such program".to_owned())
            } else {
                Ok(())
            }
        }
    }

    fn command() -> Frame {
        Frame::new(Kind::Shutdown, NAME, "")
    }

    /// Catches a listener that runs anything but the fixed program with the fixed
    /// arguments, runs it more than once, runs it before the acknowledgement is written
    /// (the host would then wait for an ack that the dying guest never sends), or answers
    /// with something other than an `ack` of the same name.
    #[test]
    fn a_shutdown_frame_is_acked_then_runs_exactly_the_fixed_argv() {
        let (mut p, written) = pipe(&[command()]);
        let runner = Fake::new(&written);
        assert_eq!(handle(&mut p, &runner), Served::Shutdown { acked: true });
        assert_eq!(
            *runner.calls.borrow(),
            vec![(
                "/sbin/shutdown".to_owned(),
                vec!["-h".to_owned(), "now".to_owned()],
                written.borrow().len()
            )]
        );
        assert!(!written.borrow().is_empty(), "the ack was written first");
        assert_eq!(
            frames_of(&written.borrow()),
            vec![Frame::new(Kind::Ack, "shutdown", "")]
        );
    }

    /// Catches a listener that skips the shutdown because the acknowledgement could not
    /// be written: the host's request is valid, and a power-button fallback would be
    /// ignored by the same guest.
    #[test]
    fn the_shutdown_runs_even_when_the_ack_cannot_be_written() {
        let (mut p, written) = pipe(&[command()]);
        p.fail_write = true;
        let runner = Fake::new(&written);
        assert_eq!(handle(&mut p, &runner), Served::Shutdown { acked: false });
        assert_eq!(runner.calls.borrow().len(), 1);
    }

    /// Catches a failed shutdown program reported as success, which would end the daemon
    /// (the guest is not going down) and leave the host with an ack and a running guest.
    #[test]
    fn a_failed_shutdown_program_is_reported_and_the_daemon_keeps_listening() {
        struct One(Option<Pipe>);
        impl Incoming for One {
            type Conn = Pipe;
            fn accept(&mut self) -> Result<Pipe, String> {
                self.0.take().ok_or_else(|| "listener closed".to_owned())
            }
        }
        let (p, written) = pipe(&[command()]);
        let mut runner = Fake::new(&written);
        runner.fail = true;
        assert_eq!(
            handle(&mut pipe(&[command()]).0, &runner),
            Served::ShutdownFailed("no such program".to_owned())
        );
        let mut log = Vec::new();
        let r = serve(&mut One(Some(p)), &runner, &mut log);
        assert_eq!(r, Err("listener closed".to_owned()));
        let log = String::from_utf8(log).expect("utf8");
        assert!(log.contains("ShutdownFailed"), "{log}");
    }

    /// Catches a listener that obeys any other frame kind, a shutdown with another name or
    /// a body (a payload nobody asked for), or one that does not say it refused: nothing
    /// runs and the peer is told why, with its own name echoed.
    #[test]
    fn other_frames_are_refused_and_nothing_runs() {
        for bad in [
            Frame::new(Kind::Ready, "shutdown", ""),
            Frame::new(Kind::Result, "shutdown", ""),
            Frame::new(Kind::Log, "shutdown", ""),
            Frame::new(Kind::Ack, "shutdown", ""),
            Frame::new(Kind::Refused, "shutdown", ""),
            Frame::new(Kind::Shutdown, "reboot", ""),
            Frame::new(Kind::Shutdown, "shutdown", "-r now"),
        ] {
            let (mut p, written) = pipe(std::slice::from_ref(&bad));
            let runner = Fake::new(&written);
            let served = handle(&mut p, &runner);
            assert!(matches!(served, Served::Refused(_)), "{bad:?}: {served:?}");
            assert!(runner.calls.borrow().is_empty(), "{bad:?}");
            let replies = frames_of(&written.borrow());
            assert_eq!(replies.len(), 1, "{bad:?}");
            assert_eq!(replies[0].kind, Kind::Refused, "{bad:?}");
            assert_eq!(replies[0].name, bad.name, "{bad:?}");
            assert!(replies[0].body.contains("shutdown"), "{bad:?}");
        }
    }

    /// Catches a listener that reads a frame over its small limit (a peer could make it
    /// buffer megabytes), accepts bytes that are not a frame, or treats a half frame as a
    /// command; and one that hangs up silently instead of saying why.
    #[test]
    fn oversize_and_malformed_input_is_refused_from_the_header_and_nothing_runs() {
        let big = u32::try_from(MAX_COMMAND_FRAME_BYTES + 1)
            .expect("fits")
            .to_be_bytes()
            .to_vec();
        let mut not_json = 5u32.to_be_bytes().to_vec();
        not_json.extend_from_slice(b"hello");
        let mut cut = encode_bytes(&command());
        cut.truncate(cut.len() - 4);
        for (what, bytes) in [
            ("oversize", big),
            ("not json", not_json),
            ("cut", cut),
            ("zero length", vec![0, 0, 0, 0]),
            ("half header", vec![0, 0]),
        ] {
            let (mut p, written) = raw_pipe(bytes);
            let runner = Fake::new(&written);
            let served = handle(&mut p, &runner);
            assert!(matches!(served, Served::Refused(_)), "{what}: {served:?}");
            assert!(runner.calls.borrow().is_empty(), "{what}");
            let replies = frames_of(&written.borrow());
            assert_eq!(replies.len(), 1, "{what}");
            assert_eq!(replies[0].kind, Kind::Refused, "{what}");
        }
        // The oversize case is refused before the body is read: the input held no body.
        let (mut p, written) = raw_pipe(
            u32::try_from(MAX_COMMAND_FRAME_BYTES + 1)
                .expect("fits")
                .to_be_bytes()
                .to_vec(),
        );
        let served = handle(&mut p, &Fake::new(&written));
        let why = FrameError::TooLarge {
            len: MAX_COMMAND_FRAME_BYTES + 1,
        };
        assert_eq!(served, Served::Refused(why.to_string()));
    }

    fn encode_bytes(f: &Frame) -> Vec<u8> {
        crate::frame::encode(f).expect("encode")
    }

    /// Catches a peer that drips the frame a byte at a time, each inside the socket's
    /// per-read timeout, and so holds the only connection for as long as it likes: past
    /// the deadline the read fails, the peer is refused, and nothing runs.
    #[test]
    fn a_frame_that_arrives_too_slowly_is_refused_and_nothing_runs() {
        /// Every read takes 10 ms of the test's clock, not of the wall clock.
        struct Drip(Cursor<Vec<u8>>, Rc<RefCell<Vec<u8>>>, Rc<Cell<Duration>>);
        impl Read for Drip {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.2.set(self.2.get() + Duration::from_millis(10));
                self.0.read(&mut buf[..1])
            }
        }
        impl Write for Drip {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.1.borrow_mut().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let written = Rc::new(RefCell::new(Vec::new()));
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let now = || clock.get();
        let frame = encode_bytes(&command());
        assert!(
            frame.len() > 10,
            "the frame takes more than the 100 ms deadline"
        );
        let mut p = Drip(Cursor::new(frame.clone()), written.clone(), clock.clone());
        let runner = Fake::new(&written);
        let served = handle_within(&mut p, &runner, Duration::from_millis(100), &now);
        assert_eq!(
            served,
            Served::Refused("stream error: the frame did not arrive within its deadline".into())
        );
        assert!(runner.calls.borrow().is_empty());
        // The same frame at the same pace is served when the deadline allows it.
        let mut p = Drip(Cursor::new(frame), written.clone(), clock.clone());
        let served = handle_within(&mut p, &runner, Duration::from_secs(30), &now);
        assert_eq!(served, Served::Shutdown { acked: true });
    }

    /// Catches a peer that connects and says nothing being treated as a command.
    #[test]
    fn a_peer_that_sends_nothing_is_dropped_and_nothing_runs() {
        let (mut p, written) = raw_pipe(Vec::new());
        let runner = Fake::new(&written);
        assert_eq!(handle(&mut p, &runner), Served::Dropped);
        assert!(runner.calls.borrow().is_empty());
        assert!(written.borrow().is_empty());
    }

    /// Catches a refusal that fails the daemon when the peer is gone, and a listener that
    /// stops at the first bad connection: refused and silent peers are logged and the next
    /// connection is served; the daemon returns only after a shutdown ran, and a listener
    /// error ends it with the error.
    #[test]
    fn serve_goes_on_after_bad_connections_and_returns_after_the_shutdown() {
        struct Script(VecDeque<Result<Pipe, String>>);
        impl Incoming for Script {
            type Conn = Pipe;
            fn accept(&mut self) -> Result<Pipe, String> {
                self.0
                    .pop_front()
                    .unwrap_or_else(|| Err("listener closed".to_owned()))
            }
        }
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut gone = pipe(&[Frame::new(Kind::Ready, "x", "")]).0;
        gone.fail_write = true;
        let mut incoming = Script(VecDeque::from([
            Ok(gone),
            Ok(raw_pipe(Vec::new()).0),
            Ok(pipe(&[command()]).0),
            Ok(pipe(&[command()]).0),
        ]));
        let runner = Fake::new(&seen);
        let mut log = Vec::new();
        assert_eq!(serve(&mut incoming, &runner, &mut log), Ok(()));
        assert_eq!(runner.calls.borrow().len(), 1, "stopped after the shutdown");
        assert_eq!(incoming.0.len(), 1, "the last connection was not taken");
        let log = String::from_utf8(log).expect("utf8");
        assert!(log.contains("Refused"), "{log}");
        assert!(log.contains("Dropped"), "{log}");
        assert!(log.contains("Shutdown { acked: true }"), "{log}");
        let mut failing = Script(VecDeque::from([Err("accept: errno 9".to_owned())]));
        assert_eq!(
            serve(&mut failing, &runner, &mut Vec::new()),
            Err("accept: errno 9".to_owned())
        );
    }

    /// Catches a real runner that reports a failing program as success, or a program that
    /// cannot be started as anything but an error naming it.
    #[test]
    fn the_system_runner_reports_the_exit_status_and_a_missing_program() {
        assert_eq!(SystemRunner.run("/usr/bin/true", &[]), Ok(()));
        let e = SystemRunner.run("/usr/bin/false", &[]).unwrap_err();
        assert!(e.contains("/usr/bin/false exited"), "{e}");
        let e = SystemRunner.run("/nonexistent/shutdown", &[]).unwrap_err();
        assert!(e.contains("/nonexistent/shutdown"), "{e}");
    }

    /// Catches the program or its arguments being changed to something that could take
    /// anything from a frame, or a port that collides with the results port.
    #[test]
    fn the_command_and_the_port_are_the_documented_constants() {
        assert_eq!((PROGRAM, ARGS), ("/sbin/shutdown", ["-h", "now"]));
        assert_eq!(PORT, 5001);
        assert!(encode_bytes(&command()).len() - 4 <= MAX_COMMAND_FRAME_BYTES);
    }
}
