//! The id of one frontend process.

use std::io::Read as _;

/// A random UUID (version 4), new for each process: `GET role` reports it, so a caller
/// can tell the process it asked before from one that took its place.
///
/// # Errors
/// `/dev/urandom` cannot be read.
pub(crate) fn new_id() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h = hex::encode(bytes);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}
