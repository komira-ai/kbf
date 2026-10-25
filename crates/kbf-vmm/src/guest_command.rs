//! The host's command to a guest over virtio-socket: shut down cleanly.
//!
//! `requestStopWithError` is ignored by a macOS guest resting at the login window, so the
//! helper connects to the guest's command listener (`kbf-guest shutdown-listen`, port
//! [`kbf_guest::shutdown::PORT`]) and sends one `shutdown` frame; the daemon answers `ack`
//! and runs `/sbin/shutdown -h now` (`kbf_guest::shutdown`). The connection itself is made
//! by the VM helper (`vz::helper`), the only code that talks to the framework; this is the
//! exchange over it, which runs in unit tests on any OS.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_guest::frame::{self, Frame, Kind, SendError};
use kbf_guest::shutdown::NAME;

/// Decides who acts on a shutdown request: the connection's completion (which sends the
/// command) or the timer that falls back to the stop request when no completion comes.
/// Each calls [`try_settle`](Self::try_settle) before doing anything visible, and only the
/// first caller gets `true`; the other does nothing, so the guest gets the command or the
/// stop request, not both, and no spurious "no ack" is reported for a command that was sent.
#[derive(Debug, Default)]
pub struct Arbiter(AtomicBool);

impl Arbiter {
    /// True for exactly one caller over the arbiter's life.
    pub fn try_settle(&self) -> bool {
        !self.0.swap(true, Ordering::SeqCst)
    }
}

/// Sends the `shutdown` command on `stream` and waits for the guest's reply.
///
/// # Errors
/// The send or the reply failed, the guest closed the connection without a reply, or it
/// answered with anything but an `ack` of the command (a `refused` carries its reason).
pub fn send_shutdown<S: Read + Write>(stream: &mut S) -> Result<(), String> {
    frame::exchange(stream, &Frame::new(Kind::Shutdown, NAME, "")).map_err(|e| match e {
        SendError::Refused(why) => format!("the guest refused it: {why}"),
        SendError::Unavailable(why) => why,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A stream that reads `reply` and records what is written.
    struct Pipe {
        reply: Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reply.read(buf)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.sent.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn pipe(reply: Option<Frame>) -> Pipe {
        let mut bytes = Vec::new();
        if let Some(f) = reply {
            frame::write_frame(&mut bytes, &f).expect("encode");
        }
        Pipe {
            reply: Cursor::new(bytes),
            sent: Vec::new(),
        }
    }

    /// Catches a timer that presses the power button although the command was sent (the
    /// guest acked and was still powering off after the timer's delay), a late completion
    /// that sends the command after the timer fell back, and two parties that both win:
    /// the first to settle wins, the other finds it settled, whichever it is, and of many
    /// racing threads exactly one wins.
    #[test]
    fn exactly_one_of_the_completion_and_the_timer_acts() {
        let a = Arbiter::default();
        assert!(a.try_settle(), "the completion won");
        assert!(!a.try_settle(), "the timer finds it settled");
        assert!(!a.try_settle());
        let a = Arbiter::default();
        assert!(a.try_settle(), "the timer won");
        assert!(!a.try_settle(), "a late completion finds it settled");
        let a = std::sync::Arc::new(Arbiter::default());
        let wins: usize = (0..16)
            .map(|_| {
                let a = a.clone();
                std::thread::spawn(move || usize::from(a.try_settle()))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().expect("thread"))
            .sum();
        assert_eq!(wins, 1);
    }

    /// Catches a host that sends anything but the one `shutdown` frame the guest's listener
    /// accepts (the listener refuses every other kind, name and body), and one that does
    /// not treat the guest's `ack` as success.
    #[test]
    fn the_host_sends_one_shutdown_frame_and_an_ack_is_success() {
        let mut p = pipe(Some(Frame::new(Kind::Ack, "shutdown", "")));
        assert_eq!(send_shutdown(&mut p), Ok(()));
        assert_eq!(
            p.sent,
            frame::encode(&Frame::new(Kind::Shutdown, "shutdown", "")).expect("encode")
        );
    }

    /// Catches a refusal, a closed connection, a reply that is not an ack of the command,
    /// or garbage counted as an ack: each is an error the helper answers with the
    /// fallback, and the text says which it was.
    #[test]
    fn a_refusal_no_reply_or_a_wrong_reply_is_an_error_with_its_reason() {
        let refused = send_shutdown(&mut pipe(Some(Frame::new(Kind::Refused, "shutdown", "no"))));
        assert_eq!(refused, Err("the guest refused it: no".to_owned()));
        let closed = send_shutdown(&mut pipe(None)).unwrap_err();
        assert!(closed.contains("without a reply"), "{closed}");
        let wrong = send_shutdown(&mut pipe(Some(Frame::new(Kind::Ack, "other", "")))).unwrap_err();
        assert!(wrong.contains("unexpected reply"), "{wrong}");
        let mut garbage = pipe(None);
        garbage.reply = Cursor::new(vec![0, 0, 0, 3, b'a', b'b', b'c']);
        let e = send_shutdown(&mut garbage).unwrap_err();
        assert!(e.contains("reply"), "{e}");
    }
}
