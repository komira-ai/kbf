//! The `control` file of a Debian package, read back out of its bytes: an `ar` archive whose
//! `control.tar.gz` holds `./control`. The index lists a package by what its own bytes say, not by
//! what the job that built it meant.

use std::io::Read;

use flate2::read::GzDecoder;

use crate::channels::Error;

/// The magic of an `ar` archive.
const MAGIC: &[u8] = b"!<arch>\n";

/// The length of the header of an `ar` member.
const HEADER: usize = 60;

fn bad(why: &str) -> Error {
    Error::Failed(format!("not a Debian package: {why}"))
}

/// The content of the member `name` of the `ar` archive `bytes`.
fn member<'a>(bytes: &'a [u8], name: &str) -> Result<&'a [u8], Error> {
    let mut rest = bytes
        .strip_prefix(MAGIC)
        .ok_or_else(|| bad("no ar magic"))?;
    while !rest.is_empty() {
        let header = rest.get(..HEADER).ok_or_else(|| bad("a cut header"))?;
        if &header[58..] != b"`\n" {
            return Err(bad("a header without its end mark"));
        }
        let text = |from: usize, to: usize| String::from_utf8_lossy(&header[from..to]);
        let size: usize = text(48, 58)
            .trim()
            .parse()
            .map_err(|_| bad("a size that is not a number"))?;
        let content = rest
            .get(HEADER..HEADER + size)
            .ok_or_else(|| bad("a member cut short"))?;
        if text(0, 16).trim().trim_end_matches('/') == name {
            return Ok(content);
        }
        rest = rest.get(HEADER + size + size % 2..).unwrap_or_default();
    }
    Err(bad(&format!("no {name}")))
}

/// The text of `control` in the package `bytes`.
///
/// # Errors
/// Anything that is not a package built as `release deb` builds one: no `ar` magic, a cut
/// member, no `control.tar.gz`, no `control` in it, or a control file that is not UTF-8.
pub fn control_of(bytes: &[u8]) -> Result<String, Error> {
    let gz = member(bytes, "control.tar.gz")?;
    let mut tar = tar::Archive::new(GzDecoder::new(gz));
    for entry in tar.entries().into_iter().flatten() {
        let mut entry = entry.map_err(|_| bad("control.tar.gz is not a tar, or is cut short"))?;
        let path = entry.path_bytes();
        if path.strip_prefix(b"./").unwrap_or(&path) == b"control" {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|_| bad("a control file that is not UTF-8"))?;
            return Ok(text);
        }
    }
    Err(bad("no control in control.tar.gz"))
}

#[cfg(test)]
mod tests;
