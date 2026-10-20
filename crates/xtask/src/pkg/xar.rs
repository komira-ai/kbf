//! A flat installer package: a xar archive, the container `pkgbuild` writes and
//! Installer opens.
//!
//! ```text
//! header (28 bytes): "xar!", header size 28 (u16), version 1 (u16), compressed and
//!                    uncompressed length of the table of contents (u64 each),
//!                    checksum algorithm 1 = SHA-1 (u32)
//! toc:               the XML table of contents, in a zlib stream
//! heap:              the SHA-1 of the compressed toc (20 bytes), then each member
//! ```
//!
//! Numbers are big-endian. Each member is stored as it is (`application/octet-stream`;
//! the payload inside it is compressed already) with its SHA-1 twice in the table, as
//! the archived and the extracted checksum. Owner root:wheel, times the epoch, and the
//! creation time the epoch too: the bytes depend on the members alone.

use std::fmt::Write as _;
use std::io::Write as _;

use flate2::Compression;

use super::sha1::{sha1, sha1_hex};

/// The length of a SHA-1 digest: where the first member starts in the heap.
const DIGEST: usize = 20;

const EPOCH: &str = "1970-01-01T00:00:00Z";

/// The archive holding `members`, each a file of the top directory.
pub fn xar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut toc = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xar><toc>\
         <creation-time>{EPOCH}</creation-time>\
         <checksum style=\"sha1\"><offset>0</offset><size>{DIGEST}</size></checksum>"
    );
    let mut offset = DIGEST;
    for (id, (name, bytes)) in members.iter().enumerate() {
        let sum = sha1_hex(bytes);
        write!(
            toc,
            "<file id=\"{id}\"><name>{name}</name><type>file</type><mode>0644</mode>\
             <uid>0</uid><gid>0</gid><user>root</user><group>wheel</group>\
             <atime>{EPOCH}</atime><mtime>{EPOCH}</mtime><ctime>{EPOCH}</ctime>\
             <data><length>{len}</length><encoding style=\"application/octet-stream\"/>\
             <offset>{offset}</offset><size>{len}</size>\
             <extracted-checksum style=\"sha1\">{sum}</extracted-checksum>\
             <archived-checksum style=\"sha1\">{sum}</archived-checksum></data></file>",
            id = id + 1,
            len = bytes.len(),
        )
        .expect("string write");
        offset += bytes.len();
    }
    toc.push_str("</toc></xar>");
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), Compression::best());
    z.write_all(toc.as_bytes()).expect("zlib into memory");
    let z = z.finish().expect("zlib into memory");
    let mut out = b"xar!".to_vec();
    out.extend(28u16.to_be_bytes());
    out.extend(1u16.to_be_bytes());
    out.extend((z.len() as u64).to_be_bytes());
    out.extend((toc.len() as u64).to_be_bytes());
    out.extend(1u32.to_be_bytes());
    out.extend_from_slice(&z);
    out.extend(sha1(&z));
    for (_, bytes) in members {
        out.extend_from_slice(bytes);
    }
    out
}
