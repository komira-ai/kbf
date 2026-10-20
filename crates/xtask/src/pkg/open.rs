//! Reading a package back: the files a `.pkg` of [`super`] installs, as the checks of a
//! release asset ([`crate::pkg_macos_asset`]) and of the install test
//! ([`crate::pkg_macos_install`]) need them.
//!
//! The package is a xar archive ([`super::xar`]) of `Bom`, `Payload` and `PackageInfo`.
//! Every member's SHA-1 is checked against the table of contents and the table against
//! the checksum at the start of the heap; the payload is a gzip-compressed odc cpio
//! archive ([`super::cpio`]). Anything inconsistent is an error naming it. The writers'
//! own tests read their output with a second reader (`tests/read.rs`) that shares no code
//! with this one, and a test here holds the two to the same result.

use std::collections::BTreeMap;
use std::io::Read;

use super::cpio::{S_IFDIR, S_IFREG};
use super::sha1::{sha1, sha1_hex};

/// What a package installs at one path.
#[derive(Debug, PartialEq, Eq)]
pub enum Installed {
    /// A directory and its permission bits.
    Dir { mode: u32 },
    /// A file: its permission bits and bytes.
    File { mode: u32, bytes: Vec<u8> },
}

fn be(bytes: &[u8], at: usize, len: usize) -> Result<u64, String> {
    let word = bytes
        .get(at..at + len)
        .ok_or_else(|| format!("no {len}-byte number at {at}"))?;
    Ok(word.iter().fold(0, |n, b| n << 8 | u64::from(*b)))
}

/// The text between `open` and `close` in `s`.
fn between<'a>(s: &'a str, open: &str, close: &str) -> Result<&'a str, String> {
    let start = s.find(open).ok_or_else(|| format!("no {open}"))? + open.len();
    let end = s[start..]
        .find(close)
        .ok_or_else(|| format!("no {close}"))?;
    Ok(&s[start..start + end])
}

/// The members of the xar archive `bytes` by name, each checked against the table's
/// lengths and checksums.
///
/// # Errors
/// `bytes` is not a xar archive this crate writes, or a length or checksum is wrong.
pub fn members(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    if bytes.get(..4) != Some(b"xar!") {
        return Err("not a xar archive (it starts `xar!`)".to_owned());
    }
    let header = (be(bytes, 4, 2)?, be(bytes, 6, 2)?, be(bytes, 24, 4)?);
    if header != (28, 1, 1) {
        return Err(format!("xar header {header:?}, not (28, 1, 1)"));
    }
    let toc_z_len = usize::try_from(be(bytes, 8, 8)?).map_err(|e| e.to_string())?;
    let toc_len = usize::try_from(be(bytes, 16, 8)?).map_err(|e| e.to_string())?;
    let toc_z = bytes
        .get(28..28 + toc_z_len)
        .ok_or("the table of contents runs past the end")?;
    let heap = &bytes[28 + toc_z_len..];
    if heap.get(..20) != Some(&sha1(toc_z)[..]) {
        return Err("the table of contents does not match its checksum".to_owned());
    }
    let mut toc = String::new();
    flate2::read::ZlibDecoder::new(toc_z)
        .read_to_string(&mut toc)
        .map_err(|e| format!("the table of contents: {e}"))?;
    if toc.len() != toc_len {
        return Err(format!(
            "the table of contents is {} bytes, not {toc_len}",
            toc.len()
        ));
    }
    let mut found = BTreeMap::new();
    for file in toc.split("<file id=").skip(1) {
        let name = between(file, "<name>", "</name>")?;
        let num = |tag: &str| {
            between(file, &format!("<{tag}>"), &format!("</{tag}>"))?
                .parse::<usize>()
                .map_err(|e| format!("{name}: {tag}: {e}"))
        };
        let (offset, length) = (num("offset")?, num("length")?);
        let data = heap
            .get(offset..offset + length)
            .ok_or_else(|| format!("{name} runs past the end"))?;
        for tag in ["archived-checksum", "extracted-checksum"] {
            let want = between(
                file,
                &format!("<{tag} style=\"sha1\">"),
                &format!("</{tag}>"),
            )?;
            if want != sha1_hex(data) {
                return Err(format!("{name}: its {tag} does not match"));
            }
        }
        if found.insert(name.to_owned(), data.to_vec()).is_some() {
            return Err(format!("{name} is in the archive twice"));
        }
    }
    Ok(found)
}

/// The paths of the gzip-compressed odc archive `gz` (the `Payload`), without the
/// leading `./` and without the root, by path.
///
/// # Errors
/// `gz` is not such an archive, holds an entry that is neither a directory nor a regular
/// file, holds a path twice, or has bytes after its trailer.
pub fn payload(gz: &[u8]) -> Result<BTreeMap<String, Installed>, String> {
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_end(&mut raw)
        .map_err(|e| format!("the payload is not gzip: {e}"))?;
    let mut found = BTreeMap::new();
    let mut at = 0;
    loop {
        let head = raw
            .get(at..at + 76)
            .ok_or("a cpio header runs past the end")?;
        let text = std::str::from_utf8(head).map_err(|e| e.to_string())?;
        if text.get(..6) != Some("070707") {
            return Err(format!("cpio magic {:?}, not 070707", &text[..6]));
        }
        let field = |from: usize, len: usize| {
            u32::from_str_radix(&text[from..from + len], 8)
                .map_err(|e| format!("a cpio header field: {e}"))
        };
        let (mode, name_len, size) = (field(18, 6)?, field(59, 6)? as usize, field(65, 11)?);
        let name_end = at + 76 + name_len;
        let name = raw
            .get(at + 76..name_end)
            .ok_or("a cpio name runs past the end")?;
        let name = name
            .strip_suffix(&[0])
            .ok_or("a cpio name without its NUL")?;
        let name = String::from_utf8(name.to_vec()).map_err(|e| e.to_string())?;
        let data = raw
            .get(name_end..name_end + size as usize)
            .ok_or_else(|| format!("{name}: its bytes run past the end"))?;
        at = name_end + size as usize;
        if name == "TRAILER!!!" {
            return if at == raw.len() {
                Ok(found)
            } else {
                Err("bytes after the cpio trailer".to_owned())
            };
        }
        let kind = mode & 0o170_000;
        let permissions = mode & 0o7777;
        let path = name
            .strip_prefix("./")
            .map(str::to_owned)
            .or_else(|| (name == ".").then(String::new))
            .ok_or_else(|| format!("{name}: not under ./"))?;
        let node = match kind {
            S_IFDIR => Installed::Dir { mode: permissions },
            S_IFREG => Installed::File {
                mode: permissions,
                bytes: data.to_vec(),
            },
            _ => return Err(format!("{name}: neither a directory nor a regular file")),
        };
        if !path.is_empty() && found.insert(path, node).is_some() {
            return Err(format!("{name} is in the payload twice"));
        }
    }
}

/// The `Payload` of the package `bytes`, read by [`members`] and [`payload`].
///
/// # Errors
/// As those two; or the package lacks `Payload`.
pub fn installed(bytes: &[u8]) -> Result<BTreeMap<String, Installed>, String> {
    let parts = members(bytes)?;
    payload(parts.get("Payload").ok_or("the package has no Payload")?)
}
