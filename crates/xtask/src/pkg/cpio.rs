//! A package's `Payload`: the tree as a cpio archive in the portable ("odc", magic
//! `070707`) format, gzip-compressed, which is what `pkgbuild` writes and Installer
//! unpacks.
//!
//! Every entry has owner and group 0, time 0, its own inode number and one link (two for
//! a directory), so no two entries are taken for hard links. The entries are in path
//! order, the root `.` first, then `./usr`, then `./usr/local`: a directory before what
//! is in it.

use std::io::Write;

use flate2::Compression;

use super::tree::{Node, ROOT, Tree};

/// The file-type bits of a directory, and of a regular file, in a mode.
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFREG: u32 = 0o100_000;

/// The archive of `tree`, uncompressed.
pub fn odc(tree: &Tree) -> Vec<u8> {
    let mut out = Vec::new();
    for (inode, (path, node)) in tree.nodes().enumerate() {
        let name = if path == ROOT {
            ROOT.to_owned()
        } else {
            format!("./{path}")
        };
        match node {
            Node::Dir { mode } => entry(&mut out, &name, inode + 1, S_IFDIR | mode, 2, &[]),
            Node::File { mode, bytes } => {
                entry(&mut out, &name, inode + 1, S_IFREG | mode, 1, bytes);
            }
        }
    }
    entry(&mut out, "TRAILER!!!", 0, 0, 1, &[]);
    out
}

/// One member: the 76-byte header (octal ASCII fields), the name and a NUL, the bytes.
fn entry(out: &mut Vec<u8>, name: &str, inode: usize, mode: u32, links: u32, bytes: &[u8]) {
    // magic, device, inode, mode, uid, gid, links, rdev, mtime, name size, size
    write!(
        out,
        "070707{:06o}{inode:06o}{mode:06o}{:06o}{:06o}{links:06o}{:06o}{:011o}{:06o}{:011o}{name}\0",
        0,
        0,
        0,
        0,
        0,
        name.len() + 1,
        bytes.len(),
    )
    .expect("cpio into memory");
    out.extend_from_slice(bytes);
}

/// The `Payload` of `tree`: [`odc`] in a gzip stream with no file name and time 0.
pub fn payload(tree: &Tree) -> Vec<u8> {
    gzip(&odc(tree))
}

/// `bytes` in a gzip stream that is the same for the same bytes on every host.
pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::GzBuilder::new()
        .mtime(0)
        .operating_system(3)
        .write(Vec::new(), Compression::best());
    gz.write_all(bytes).expect("gzip into memory");
    gz.finish().expect("gzip into memory")
}
