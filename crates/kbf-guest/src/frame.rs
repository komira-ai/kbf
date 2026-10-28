//! The frames a probe guest sends the host over virtio-socket, and the host's replies.
//!
//! The first E1 runs showed that no file read or write inside the guest works on a
//! virtiofs share while the guest rests at the login window (every call fails with
//! `EPERM`). Results and ready markers therefore travel over the virtio socket, which
//! involves no filesystem rule. One frame is a four-byte big-endian length followed by that
//! many bytes of JSON:
//!
//! ```text
//! {"kind": "ready" | "result" | "log" | "shutdown" | "ack" | "refused", "name": "...", "body": "..."}
//! ```
//!
//! The host sends one `shutdown` frame (name `shutdown`, empty body) to the guest's
//! command listener ([`crate::shutdown`]), which answers `ack` and shuts the guest down.
//! The guest sends `ready`, `result` and `log`; `name` is the file name the host writes
//! under its results directory (a `log` is appended to `<name>.log`), and `body` is the
//! file's text. The host answers each frame with `ack` (same name, empty body) or
//! `refused` (same name, the reason in the body). Both ends bound the size of what they
//! read before they allocate it.
//!
//! This module has no socket in it: it reads and writes any byte stream, so the macOS
//! guest (`probe_host`), the host's listener (`kbf-vmm`) and the tests share one codec.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// The largest frame, header excluded, either end accepts. A result body is at most
/// [`MAX_BODY_BYTES`]; JSON escaping can double that.
pub const MAX_FRAME_BYTES: usize = 4 << 20;
/// The largest `body`: the host refuses to read a result file larger than this either.
pub const MAX_BODY_BYTES: usize = 1 << 20;

/// What a frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A marker: the guest reached a point (`ready-system`, `ready-login`).
    Ready,
    /// A probe's answer, written as `<name>`.
    Result,
    /// Text appended to `<name>.log`.
    Log,
    /// The host's command to the guest: shut down cleanly (see [`crate::shutdown`]). A guest
    /// never sends it, and the host's results listener refuses it.
    Shutdown,
    /// The reply: the frame was written (results) or the command was accepted (shutdown).
    Ack,
    /// The reply: the frame was not written or the command not accepted; the body says why.
    Refused,
}

/// One message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub kind: Kind,
    pub name: String,
    pub body: String,
}

impl Frame {
    #[must_use]
    pub fn new(kind: Kind, name: &str, body: &str) -> Self {
        Self {
            kind,
            name: name.to_owned(),
            body: body.to_owned(),
        }
    }
}

/// A frame that cannot be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("a frame of {len} bytes is over the limit of this reader (at most {MAX_FRAME_BYTES})")]
    TooLarge { len: usize },
    #[error("a body of {len} bytes is over the {MAX_BODY_BYTES} byte limit")]
    BodyTooLarge { len: usize },
    #[error("the stream ended after {got} of {want} bytes of a frame")]
    Truncated { got: usize, want: usize },
    #[error("a frame of length 0")]
    Empty,
    #[error("not a frame: {0}")]
    Malformed(String),
    #[error("stream error: {0}")]
    Io(String),
}

/// The bytes of `frame`: length, then JSON.
///
/// # Errors
/// The body or the whole frame is over its limit.
pub fn encode(frame: &Frame) -> Result<Vec<u8>, FrameError> {
    if frame.body.len() > MAX_BODY_BYTES {
        return Err(FrameError::BodyTooLarge {
            len: frame.body.len(),
        });
    }
    let json = serde_json::to_vec(frame).map_err(|e| FrameError::Malformed(e.to_string()))?;
    if json.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge { len: json.len() });
    }
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&u32::try_from(json.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// Reads up to `n` bytes (fewer at the end of the stream).
fn read_up_to<R: Read>(r: &mut R, n: usize) -> Result<Vec<u8>, FrameError> {
    let mut buf = Vec::new();
    r.take(n as u64)
        .read_to_end(&mut buf)
        .map_err(|e| FrameError::Io(e.to_string()))?;
    Ok(buf)
}

/// Reads the next frame; `None` when the stream ended cleanly between two frames.
///
/// # Errors
/// The stream ended inside a frame, the length is 0 or over the limit (nothing is
/// allocated for it), the bytes are not a frame, or the body is over its limit.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Option<Frame>, FrameError> {
    read_frame_within(r, MAX_FRAME_BYTES)
}

/// [`read_frame`] with a smaller frame limit: a listener that expects only short frames
/// refuses a longer one from its length alone, before it reads or allocates anything for it.
///
/// # Errors
/// As [`read_frame`], with `max_frame` in place of [`MAX_FRAME_BYTES`].
pub fn read_frame_within<R: Read>(
    r: &mut R,
    max_frame: usize,
) -> Result<Option<Frame>, FrameError> {
    let head = read_up_to(r, 4)?;
    if head.is_empty() {
        return Ok(None);
    }
    let Ok(head) = <[u8; 4]>::try_from(head.as_slice()) else {
        return Err(FrameError::Truncated {
            got: head.len(),
            want: 4,
        });
    };
    let len = u32::from_be_bytes(head) as usize;
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > max_frame {
        return Err(FrameError::TooLarge { len });
    }
    let json = read_up_to(r, len)?;
    if json.len() < len {
        return Err(FrameError::Truncated {
            got: json.len(),
            want: len,
        });
    }
    let frame: Frame =
        serde_json::from_slice(&json).map_err(|e| FrameError::Malformed(e.to_string()))?;
    if frame.body.len() > MAX_BODY_BYTES {
        return Err(FrameError::BodyTooLarge {
            len: frame.body.len(),
        });
    }
    Ok(Some(frame))
}

/// Writes `frame` and flushes.
///
/// # Errors
/// The frame cannot be encoded or the stream refuses it.
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> Result<(), FrameError> {
    let bytes = encode(frame)?;
    w.write_all(&bytes)
        .and_then(|()| w.flush())
        .map_err(|e: io::Error| FrameError::Io(e.to_string()))
}

/// Why a frame did not reach the host's results directory.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// No connection, or no usable reply: another attempt may work.
    #[error("{0}")]
    Unavailable(String),
    /// The host read the frame and refused it: another attempt would be refused too.
    #[error("the host refused it: {0}")]
    Refused(String),
}

/// Sends `frame` on `stream` and waits for the host's reply.
///
/// # Errors
/// [`SendError::Refused`] when the host refused the frame; [`SendError::Unavailable`] for
/// a stream error, a stream that ended without a reply, or a reply that is not an `ack`
/// for this frame.
pub fn exchange<S: Read + Write>(stream: &mut S, frame: &Frame) -> Result<(), SendError> {
    ask(stream, frame).map(|_| ())
}

/// [`exchange`], giving the `ack`'s body: the host's answer to a question sent as a frame.
///
/// # Errors
/// As [`exchange`].
pub fn ask<S: Read + Write>(stream: &mut S, frame: &Frame) -> Result<String, SendError> {
    write_frame(stream, frame).map_err(|e| SendError::Unavailable(format!("send: {e}")))?;
    match read_frame(stream) {
        Ok(Some(r)) if r.kind == Kind::Ack && r.name == frame.name => Ok(r.body),
        Ok(Some(r)) if r.kind == Kind::Refused => Err(SendError::Refused(r.body)),
        Ok(Some(r)) => Err(SendError::Unavailable(format!(
            "unexpected reply: {:?} for {}",
            r.kind, r.name
        ))),
        Ok(None) => Err(SendError::Unavailable(
            "the host closed the connection without a reply".to_owned(),
        )),
        Err(e) => Err(SendError::Unavailable(format!("reply: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(kind: Kind) -> Frame {
        Frame::new(kind, "ready-system", "{\"a\": 1}\n")
    }

    /// Catches a codec that loses a field, changes the kind names the guest and host
    /// agree on, or reads one frame into the next: two frames in a row both come back, then
    /// the clean end.
    #[test]
    fn frames_round_trip_and_the_end_between_frames_is_clean() {
        let mut bytes = Vec::new();
        for k in [
            Kind::Ready,
            Kind::Result,
            Kind::Log,
            Kind::Shutdown,
            Kind::Ack,
            Kind::Refused,
        ] {
            write_frame(&mut bytes, &frame(k)).expect("write");
        }
        let json = &bytes[4..4 + u32::from_be_bytes(bytes[..4].try_into().expect("4")) as usize];
        assert_eq!(
            std::str::from_utf8(json).expect("utf8"),
            r#"{"kind":"ready","name":"ready-system","body":"{\"a\": 1}\n"}"#
        );
        let mut r = Cursor::new(bytes);
        for k in [
            Kind::Ready,
            Kind::Result,
            Kind::Log,
            Kind::Shutdown,
            Kind::Ack,
            Kind::Refused,
        ] {
            assert_eq!(read_frame(&mut r), Ok(Some(frame(k))));
        }
        assert_eq!(read_frame(&mut r), Ok(None));
    }

    /// Catches a frame larger than the limits being written, and a body at the limit
    /// being refused.
    #[test]
    fn encoding_refuses_a_body_or_frame_over_the_limit() {
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        assert_eq!(
            encode(&Frame::new(Kind::Result, "r", &big)),
            Err(FrameError::BodyTooLarge {
                len: MAX_BODY_BYTES + 1
            })
        );
        assert!(encode(&Frame::new(Kind::Result, "r", &"x".repeat(MAX_BODY_BYTES))).is_ok());
        // A name can push the JSON over the frame limit even when the body is small.
        let name = "n".repeat(MAX_FRAME_BYTES);
        assert!(matches!(
            encode(&Frame::new(Kind::Result, &name, "")),
            Err(FrameError::TooLarge { .. })
        ));
    }

    /// Catches a reader that allocates for, or waits for, a length over the limit: the
    /// error comes from the four header bytes alone.
    #[test]
    fn a_length_over_the_limit_or_zero_is_refused_before_any_body_is_read() {
        let over = u32::try_from(MAX_FRAME_BYTES + 1)
            .expect("fits")
            .to_be_bytes();
        assert_eq!(
            read_frame(&mut Cursor::new(over.to_vec())),
            Err(FrameError::TooLarge {
                len: MAX_FRAME_BYTES + 1
            })
        );
        assert_eq!(
            read_frame(&mut Cursor::new(vec![0, 0, 0, 0])),
            Err(FrameError::Empty)
        );
    }

    /// Catches a half header or a half body read as a frame, or as a clean end.
    #[test]
    fn a_stream_cut_inside_a_frame_is_truncated() {
        assert_eq!(
            read_frame(&mut Cursor::new(vec![0, 0])),
            Err(FrameError::Truncated { got: 2, want: 4 })
        );
        let mut bytes = encode(&frame(Kind::Ready)).expect("encode");
        let full = bytes.len() - 4;
        bytes.truncate(bytes.len() - 3);
        assert_eq!(
            read_frame(&mut Cursor::new(bytes)),
            Err(FrameError::Truncated {
                got: full - 3,
                want: full
            })
        );
    }

    /// Catches bytes that are not a frame being accepted: not JSON, a kind nobody knows,
    /// a missing field, an extra field, and an oversized body inside a small enough frame.
    #[test]
    fn bytes_that_are_not_a_frame_are_refused() {
        let wrap = |json: &str| {
            let mut v = u32::try_from(json.len())
                .expect("fits")
                .to_be_bytes()
                .to_vec();
            v.extend_from_slice(json.as_bytes());
            Cursor::new(v)
        };
        for bad in [
            "not json",
            r#"{"kind":"nope","name":"a","body":""}"#,
            r#"{"kind":"ready","name":"a"}"#,
            r#"{"kind":"ready","name":"a","body":"","extra":1}"#,
        ] {
            assert!(
                matches!(read_frame(&mut wrap(bad)), Err(FrameError::Malformed(_))),
                "{bad}"
            );
        }
        let json = format!(
            r#"{{"kind":"log","name":"a","body":"{}"}}"#,
            "x".repeat(MAX_BODY_BYTES + 1)
        );
        assert_eq!(
            read_frame(&mut wrap(&json)),
            Err(FrameError::BodyTooLarge {
                len: MAX_BODY_BYTES + 1
            })
        );
    }

    /// A stream that fails every read and write.
    struct Broken;

    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("read refused"))
        }
    }

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write refused"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Catches stream errors turned into a clean end or a panic.
    #[test]
    fn stream_errors_are_reported() {
        assert_eq!(
            read_frame(&mut Broken),
            Err(FrameError::Io("read refused".to_owned()))
        );
        assert_eq!(
            write_frame(&mut Broken, &frame(Kind::Log)),
            Err(FrameError::Io("write refused".to_owned()))
        );
        // A frame that cannot be encoded is not written at all.
        let mut sink = Vec::new();
        let big = Frame::new(Kind::Log, "x", &"y".repeat(MAX_BODY_BYTES + 1));
        assert!(matches!(
            write_frame(&mut sink, &big),
            Err(FrameError::BodyTooLarge { .. })
        ));
        assert!(sink.is_empty());
    }

    /// A stream whose reads come from a canned reply and whose writes are kept.
    struct Talk {
        reply: Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl Talk {
        fn replying(frames: &[Frame]) -> Self {
            let mut reply = Vec::new();
            for f in frames {
                write_frame(&mut reply, f).expect("reply");
            }
            Self {
                reply: Cursor::new(reply),
                sent: Vec::new(),
            }
        }
    }

    impl Read for Talk {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.reply.read(buf)
        }
    }

    impl Write for Talk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.sent.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Catches a question whose answer is lost (the ack's body is the host's answer), and an
    /// answer taken from a reply that is not the ack of this question.
    #[test]
    fn a_question_gets_the_body_of_its_ack_and_nothing_else() {
        let q = Frame::new(Kind::Result, "prime-question", "");
        let mut yes = Talk::replying(&[Frame::new(Kind::Ack, "prime-question", "yes")]);
        assert_eq!(ask(&mut yes, &q), Ok("yes".to_owned()));
        let mut other = Talk::replying(&[Frame::new(Kind::Ack, "other", "yes")]);
        assert!(ask(&mut other, &q).is_err());
        let mut refused = Talk::replying(&[Frame::new(Kind::Refused, "prime-question", "no")]);
        assert_eq!(
            ask(&mut refused, &q),
            Err(SendError::Refused("no".to_owned()))
        );
    }

    /// Catches an exchange that counts anything but an ack of this frame as delivered, and
    /// one that retries a refusal: the refusal text is kept and is a different error.
    #[test]
    fn an_exchange_succeeds_only_on_an_ack_of_the_same_frame() {
        let sent = frame(Kind::Ready);
        let ack = |name: &str| Frame::new(Kind::Ack, name, "");
        let mut ok = Talk::replying(&[ack("ready-system")]);
        assert_eq!(exchange(&mut ok, &sent), Ok(()));
        assert_eq!(ok.sent, encode(&sent).expect("encode"));
        let mut refused = Talk::replying(&[Frame::new(Kind::Refused, "ready-system", "bad name")]);
        assert_eq!(
            exchange(&mut refused, &sent),
            Err(SendError::Refused("bad name".to_owned()))
        );
        let other = exchange(&mut Talk::replying(&[ack("other")]), &sent).expect_err("name");
        assert!(other.to_string().contains("unexpected reply"), "{other}");
        let silent = exchange(&mut Talk::replying(&[]), &sent).expect_err("silent");
        assert!(silent.to_string().contains("without a reply"), "{silent}");
        let mut garbled = Talk {
            reply: Cursor::new(vec![0, 0, 0, 0]),
            sent: Vec::new(),
        };
        let e = exchange(&mut garbled, &sent).expect_err("garbled");
        assert!(e.to_string().starts_with("reply:"), "{e}");
        let e = exchange(&mut Broken, &sent).expect_err("broken");
        assert!(e.to_string().starts_with("send:"), "{e}");
    }

    /// Catches a reader with a smaller limit that reads, or allocates for, a frame over its
    /// limit (the command listener must not read megabytes from a peer), and one that
    /// refuses a frame exactly at the limit.
    #[test]
    fn a_smaller_limit_refuses_from_the_length_and_accepts_a_frame_at_the_limit() {
        let bytes = encode(&frame(Kind::Ready)).expect("encode");
        let len = bytes.len() - 4;
        assert_eq!(
            read_frame_within(&mut Cursor::new(bytes.clone()), len),
            Ok(Some(frame(Kind::Ready)))
        );
        assert_eq!(
            read_frame_within(&mut Cursor::new(bytes[..4].to_vec()), len - 1),
            Err(FrameError::TooLarge { len })
        );
    }

    /// Catches a `shutdown` kind spelled differently from the one the guest's listener and
    /// the host's command agree on.
    #[test]
    fn the_shutdown_command_is_spelled_shutdown_on_the_wire() {
        let bytes = encode(&Frame::new(Kind::Shutdown, "shutdown", "")).expect("encode");
        assert_eq!(
            std::str::from_utf8(&bytes[4..]).expect("utf8"),
            r#"{"kind":"shutdown","name":"shutdown","body":""}"#
        );
    }
}
