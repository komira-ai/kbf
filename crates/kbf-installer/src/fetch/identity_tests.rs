//! The lent identity: how it is read from disk, framed, lent and taken. The key is the secret
//! a compromised child could steal (contract S11), so these tests pin the ways it must not
//! travel: no link followed, no file but a regular one, nothing printed, nothing on disk.

use std::fs::{self, File};
#[cfg(target_os = "linux")]
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use super::*;
use crate::fixtures::scratch_dir;

fn sample() -> Identity {
    Identity {
        cert: b"-----BEGIN CERTIFICATE-----\ncert\n-----END CERTIFICATE-----\n".to_vec(),
        key: b"-----BEGIN PRIVATE KEY-----\nsecret\n-----END PRIVATE KEY-----\n".to_vec(),
        ca: b"-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----\n".to_vec(),
    }
}

fn file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).expect("write");
    path
}

/// What the parent frames is what the child gets back, section by section.
///
/// Catches a framing that swaps or merges sections (a key taken for a certificate).
#[test]
fn the_identity_survives_framing() {
    let id = sample();
    assert_eq!(
        Identity::decode(id.encode().as_slice()).expect("decodes"),
        id
    );
}

/// The child refuses anything that is not exactly three sections inside the caps: it reads
/// from a descriptor a parent filled, but a parent's bug must not become a child that parses
/// an arbitrary blob.
///
/// Catches a decoder that accepts a short section, trailing bytes, an empty section or a blob
/// over the cap.
///
/// Mutants seen red: the trailing-bytes check removed; the empty-section check removed.
#[test]
fn a_malformed_blob_is_refused() {
    let good = sample().encode();
    let mut trailing = good.clone();
    trailing.push(0);
    let cut = good[..good.len() - 1].to_vec();
    let mut empty = good.clone();
    empty[..4].copy_from_slice(&0_u32.to_be_bytes());
    let mut huge = good.clone();
    huge[..4].copy_from_slice(&(MAX_PEM_BYTES as u32 + 1).to_be_bytes());
    let over = vec![7_u8; MAX_IDENTITY_BYTES + 1];
    for (what, blob) in [
        ("trailing", trailing),
        ("cut", cut),
        ("empty", empty),
        ("huge", huge),
        ("over", over),
        ("nothing", Vec::new()),
    ] {
        let got = Identity::decode(blob.as_slice());
        assert!(
            matches!(got, Err(IdentityFault::Malformed(_))),
            "{what}: {got:?}"
        );
    }
}

/// A file is read once, whole, and only if it is a regular file of a sane size.
#[test]
fn a_pem_is_read_only_from_a_plain_file_of_a_sane_size() {
    let dir = scratch_dir("identity-read");
    let ok = file(&dir, "ok.pem", b"pem");
    assert_eq!(read_pem(&ok).expect("reads"), b"pem");
    let empty = file(&dir, "empty.pem", b"");
    let big = file(&dir, "big.pem", &vec![b'a'; MAX_PEM_BYTES as usize + 1]);
    let exact = file(&dir, "exact.pem", &vec![b'a'; MAX_PEM_BYTES as usize]);
    assert!(read_pem(&exact).is_ok());
    for bad in [empty, big, dir.clone(), dir.join("absent.pem")] {
        assert!(
            matches!(read_pem(&bad), Err(IdentityFault::File { .. })),
            "{}",
            bad.display()
        );
    }
}

/// The final component is opened without following a link: `kbf` owns the directory the key
/// is in and could point the name at a root-only file (`/etc/shadow`) to have root lend it.
///
/// Catches the open following a link.
///
/// Mutant seen red: `O_NOFOLLOW` left out of `read_pem`.
#[test]
fn a_linked_pem_is_not_followed() {
    let dir = scratch_dir("identity-link");
    let target = file(&dir, "target.pem", b"secret that is not the key");
    let link = dir.join("link.pem");
    symlink(&target, &link).expect("link");
    let got = read_pem(&link);
    assert!(matches!(got, Err(IdentityFault::File { .. })), "{got:?}");
}

/// A FIFO is not a regular file: opening it for reading must not wait for a writer either.
#[test]
fn a_fifo_is_refused_without_waiting() {
    let dir = scratch_dir("identity-fifo");
    let fifo = dir.join("fifo.pem");
    let c = std::ffi::CString::new(fifo.to_str().expect("utf8")).expect("c string");
    // SAFETY: mkfifo on a path this test just formed.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let started = std::time::Instant::now();
    assert!(matches!(read_pem(&fifo), Err(IdentityFault::File { .. })));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// The key is never printed.
#[test]
fn the_debug_form_leaks_nothing() {
    let shown = format!("{:?}", sample());
    assert!(
        !shown.contains("PRIVATE") && !shown.contains("secret"),
        "{shown}"
    );
}

/// What is lent is what the child reads from the front, and it is sealed: the child cannot
/// rewrite what the parent lent (and it could not create a file anyway).
///
/// Catches a lent descriptor positioned at its end (the child would read nothing) and one
/// that can still be written.
#[cfg(target_os = "linux")]
#[test]
fn the_lent_descriptor_reads_from_the_front_and_is_sealed() {
    let id = sample();
    let fd = id.lend().expect("lends");
    let mut file = File::from(fd);
    assert!(
        file.write_all(b"x").is_err(),
        "the lent file must be sealed"
    );
    let mut blob = Vec::new();
    file.read_to_end(&mut blob).expect("reads");
    assert_eq!(Identity::decode(blob.as_slice()).expect("decodes"), id);
}

/// The lent descriptor is close-on-exec in the parent: a program the parent starts for some
/// other reason does not inherit the key.
#[cfg(target_os = "linux")]
#[test]
fn the_lent_descriptor_is_not_inherited_by_accident() {
    use std::os::fd::AsRawFd;
    let fd = sample().lend().expect("lends");
    // SAFETY: F_GETFD on a descriptor this test holds.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0, "{flags}");
}

/// The descriptors the child may hold are its three streams and what it is lent; a fifth is
/// named in the error. (This process holds many more, which is the point.)
#[test]
fn a_stray_descriptor_is_named() {
    let held = File::open("/").expect("a directory");
    let got = ensure_descriptors(&[0, 1, 2]);
    assert!(
        matches!(got, Err(IdentityFault::Stray(n)) if n > 2),
        "{got:?}"
    );
    drop(held);
}
