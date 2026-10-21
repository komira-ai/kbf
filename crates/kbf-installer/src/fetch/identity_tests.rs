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

/// A file that cannot be read is a refusal naming it (the read of a process's memory at offset
/// 0 fails though the file is regular).
#[cfg(target_os = "linux")]
#[test]
fn a_file_that_fails_to_read_is_refused() {
    // By number: `/proc/self` is itself a link, which `read_pem` refuses for another reason.
    let mem = PathBuf::from(format!("/proc/{}/mem", std::process::id()));
    let got = read_pem(&mem);
    assert!(
        matches!(&got, Err(IdentityFault::File { why, .. }) if !why.contains("link")),
        "{got:?}"
    );
}

/// A link anywhere in the path is refused, not only one at the end: `kbf` owns the directory the
/// key is in, and a directory of its that it swaps for a link to another directory would have
/// root read (and lend to the child) a file of that other directory.
///
/// Catches a path walked through a link in a directory component (only the last component was
/// checked), and a plain path refused with it.
///
/// Mutant: `NO_SYMLINKS` left out of the resolve flags of `read_pem`.
#[cfg(target_os = "linux")]
#[test]
fn a_pem_behind_a_linked_directory_is_not_followed() {
    let dir = scratch_dir("identity-dirlink");
    let real = dir.join("real");
    fs::create_dir(&real).expect("directory");
    file(&real, "key.pem", b"secret that is not the key");
    symlink(&real, dir.join("alias")).expect("link");
    assert_eq!(
        read_pem(&real.join("key.pem")).expect("the plain path reads"),
        b"secret that is not the key"
    );
    let got = read_pem(&dir.join("alias").join("key.pem"));
    assert!(matches!(got, Err(IdentityFault::File { .. })), "{got:?}");
}

/// A file with another name is refused: a hard link `kbf` made to a file it could not read
/// itself would have root lend that file, and a link cannot be told from the file it links to
/// except by the count. With the other name gone the same file is read, so it was the count.
///
/// Catches the link count not asked.
///
/// Mutant: the `nlink` check removed from `read_pem`.
#[test]
fn a_pem_with_another_name_is_refused() {
    let dir = scratch_dir("identity-hardlink");
    let first = file(&dir, "a.pem", b"pem");
    let second = dir.join("b.pem");
    fs::hard_link(&first, &second).expect("hard link");
    for path in [&first, &second] {
        let got = read_pem(path);
        assert!(
            matches!(&got, Err(IdentityFault::File { why, .. }) if why.contains("link")),
            "{}: {got:?}",
            path.display()
        );
    }
    fs::remove_file(&second).expect("unlink");
    assert_eq!(read_pem(&first).expect("one name now"), b"pem");
}

/// The path is absolute: a relative one would be resolved against whatever directory the
/// helper happens to be in, and settings only ever hold absolute paths.
///
/// Catches a relative path opened (this crate's own manifest is a readable file of a sane size).
///
/// Mutant: the absolute-path check removed from `read_pem`.
#[test]
fn a_relative_path_is_refused() {
    assert!(
        Path::new("Cargo.toml").exists(),
        "the test runs in the crate's directory"
    );
    let got = read_pem(Path::new("Cargo.toml"));
    assert!(
        matches!(&got, Err(IdentityFault::File { why, .. }) if why.contains("absolute")),
        "{got:?}"
    );
}

/// Where the resolving open is not there (`ENOSYS` before Linux 5.6, `EPERM` from a sandbox that
/// filters the call) the file is opened the old way, its last component not followed; any other
/// failure of it is the answer, not a reason to open some other way.
///
/// Catches a fallback that is missing (a host with an old kernel could not lend an identity), a
/// fallback that follows a final link, and a fallback taken for a real refusal (the old way would
/// then open a file the resolving open refused).
///
/// Mutants: `PERM` dropped from the fallback's errors; the fallback taken for every error.
#[cfg(target_os = "linux")]
#[test]
fn without_the_resolving_open_the_last_component_is_still_not_followed() {
    use rustix::io::Errno;
    let dir = scratch_dir("identity-fallback");
    let plain = file(&dir, "plain.pem", b"pem");
    let link = dir.join("link.pem");
    symlink(&plain, &link).expect("link");
    for gone in [Errno::NOSYS, Errno::PERM] {
        let read = |path: &Path| open_plain_with(path, &|_| Err(gone));
        assert!(read(&plain).is_ok(), "{gone:?}: the plain file opens");
        let got = read(&link);
        assert_eq!(
            got.err().and_then(|e| e.raw_os_error()),
            Some(libc::ELOOP),
            "{gone:?}: a final link is refused"
        );
    }
    let got = open_plain_with(&plain, &|_| Err(Errno::ACCESS));
    assert_eq!(
        got.err().and_then(|e| e.raw_os_error()),
        Some(libc::EACCES),
        "another failure is the answer"
    );
}
