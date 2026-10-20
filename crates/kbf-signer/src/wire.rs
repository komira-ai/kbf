//! The frames of the socket.

use std::io::{self, ErrorKind, Read, Write};

/// The most a frame holds after its length: a certificate body is under a kilobyte.
pub(crate) const MAX_FRAME: usize = 4096;

/// Request: the signer's public key.
pub(crate) const PUBLIC_KEY: u8 = 0;
/// Request: sign for the node route.
pub(crate) const SIGN_NODE: u8 = 1;
/// Request: sign for the frontend route.
pub(crate) const SIGN_FRONTEND: u8 = 2;

/// Answer: done.
pub(crate) const OK: u8 = 0;
/// Answer: the request was refused by the table.
pub(crate) const REFUSED: u8 = 1;
/// Answer: the signer failed, or did not know the request.
pub(crate) const FAILED: u8 = 2;

/// Writes one frame.
pub(crate) fn write_frame(w: &mut impl Write, tag: u8, body: &[u8]) -> io::Result<()> {
    let len = u32::try_from(body.len() + 1)
        .ok()
        .filter(|n| *n as usize <= MAX_FRAME)
        .ok_or(io::Error::from(ErrorKind::InvalidInput))?;
    let mut frame = Vec::with_capacity(5 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.push(tag);
    frame.extend_from_slice(body);
    w.write_all(&frame)
}

/// Reads one frame: its tag and body.
pub(crate) fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::from(ErrorKind::InvalidData));
    }
    let mut frame = vec![0u8; len];
    r.read_exact(&mut frame)?;
    let body = frame.split_off(1);
    Ok((frame[0], body))
}
