use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::fixtures::{Item, file, scratch_dir, tar_gz};

const CAP: u64 = 1024;
const WITHIN: Duration = Duration::from_secs(60);

/// Unpack `tarball` (named `kbf`) into a fresh directory; the destination file and what
/// the call said.
fn unpack(name: &str, tarball: &[u8], cap: u64) -> (PathBuf, Result<(), ArchiveFault>) {
    let dir = scratch_dir(name);
    let source = dir.join("kbf.tar.gz");
    fs::write(&source, tarball).expect("seed");
    let dest = dir.join("kbf");
    let result = unpack_one(&source, "kbf", &dest, cap, WITHIN);
    (dest, result)
}

/// Catches the one entry not coming out whole and executable: its bytes and mode 0755.
#[test]
fn the_one_file_is_unpacked_whole_and_executable() {
    let (dest, result) = unpack("archive-ok", &tar_gz(&[file("kbf", b"binary bytes")]), CAP);
    result.expect("unpacked");
    assert_eq!(fs::read(&dest).expect("there"), b"binary bytes");
    assert_eq!(
        fs::metadata(&dest).expect("stat").permissions().mode() & 0o7777,
        0o755
    );
    // An executable left in the target directory is read by cargo-llvm-cov as an object.
    fs::remove_file(&dest).expect("remove");
}

/// Catches an archive that is accepted though it is not exactly one regular file named for
/// the component: a second file, none, a name with a directory (`bin/kbf`, `./kbf`,
/// `../kbf`, an absolute one) or another name, a symlink, hard link, directory, FIFO or
/// device entry, and an entry over the cap. Each is refused with its own code.
///
/// Mutants seen red: the name comparison dropped; the entry-type check dropped (the
/// symlink entry passes); the count of entries not checked.
#[test]
fn an_archive_that_is_not_one_regular_file_with_the_name_is_refused() {
    let item = |name: &'static str, kind: u8| Item {
        name: name.as_bytes(),
        kind,
        data: b"x",
    };
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "two",
            tar_gz(&[file("kbf", b"a"), file("other", b"b")]),
            "archive-entries",
        ),
        ("none", tar_gz(&[]), "archive-entries"),
        (
            "dir-prefix",
            tar_gz(&[file("bin/kbf", b"a")]),
            "archive-name",
        ),
        ("dot-slash", tar_gz(&[file("./kbf", b"a")]), "archive-name"),
        ("dotdot", tar_gz(&[item("../kbf", b'0')]), "archive-name"),
        ("absolute", tar_gz(&[item("/kbf", b'0')]), "archive-name"),
        (
            "other-name",
            tar_gz(&[file("kbf-daemon", b"a")]),
            "archive-name",
        ),
        (
            "symlink",
            tar_gz(&[item("kbf", b'2')]),
            "archive-not-regular",
        ),
        (
            "hardlink",
            tar_gz(&[item("kbf", b'1')]),
            "archive-not-regular",
        ),
        (
            "directory",
            tar_gz(&[item("kbf", b'5')]),
            "archive-not-regular",
        ),
        ("fifo", tar_gz(&[item("kbf", b'6')]), "archive-not-regular"),
        (
            "chardev",
            tar_gz(&[item("kbf", b'3')]),
            "archive-not-regular",
        ),
        (
            "oversize",
            tar_gz(&[file("kbf", &[7u8; 1025])]),
            "archive-oversize",
        ),
        (
            "not-gzip",
            b"this is not a tarball".to_vec(),
            "archive-malformed",
        ),
        ("empty-file", Vec::new(), "archive-malformed"),
    ];
    for (name, bytes, want) in cases {
        let (dest, result) = unpack(&format!("archive-{name}"), &bytes, CAP);
        let err = result.expect_err(name);
        assert_eq!(err.code(), want, "{name}: {err}");
        // Only a second entry is found after the first was written; nothing else is
        // ever written (the scratch directory goes with the refusal).
        assert_eq!(dest.exists(), name == "two", "{name}");
        if dest.exists() {
            // An executable left in the target directory is read by cargo-llvm-cov as an
            // object.
            fs::remove_file(&dest).expect("remove");
        }
    }
}

/// Catches the cap being exceeded by an entry whose header lies about its size: the
/// header says 10 bytes and the archive holds more (a tar reader that trusts the header
/// would stop early); with the cap at 4 the entry is refused by its header before any
/// byte is written.
#[test]
fn the_cap_is_checked_before_a_byte_is_written() {
    let (dest, result) = unpack(
        "archive-cap-header",
        &tar_gz(&[file("kbf", b"0123456789")]),
        4,
    );
    assert_eq!(result.expect_err("over").code(), "archive-oversize");
    assert!(!dest.exists());
}

/// Catches a tarball that cannot be opened being reported as anything but malformed.
#[test]
fn a_missing_tarball_is_malformed() {
    let dir = scratch_dir("archive-missing");
    let err = unpack_one(
        &dir.join("none.tar.gz"),
        "kbf",
        &dir.join("kbf"),
        CAP,
        WITHIN,
    )
    .expect_err("gone");
    assert_eq!(err.code(), "archive-malformed");
}

/// Catches a destination that already exists being overwritten (the unpack uses
/// `create_new`): an earlier file of the scratch directory is never replaced.
#[test]
fn an_existing_destination_is_never_replaced() {
    let dir = scratch_dir("archive-exists");
    let source = dir.join("kbf.tar.gz");
    fs::write(&source, tar_gz(&[file("kbf", b"new")])).expect("seed");
    let dest = dir.join("kbf");
    fs::write(&dest, b"old").expect("seed");
    let err = unpack_one(&source, "kbf", &dest, CAP, WITHIN).expect_err("exists");
    assert_eq!(err.code(), "archive-malformed");
    assert_eq!(fs::read(&dest).expect("there"), b"old");
}

/// Gzip of `bytes` as one member.
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(bytes).expect("gzip");
    gz.finish().expect("gzip")
}

/// The tar a `tar_gz` of one file `kbf` holds, uncompressed.
fn tar_of_one() -> Vec<u8> {
    let packed = tar_gz(&[file("kbf", b"binary")]);
    let mut gz = flate2::read::GzDecoder::new(packed.as_slice());
    let mut tar = Vec::new();
    io::Read::read_to_end(&mut gz, &mut tar).expect("tar");
    tar
}

/// Catches data hidden where the tar reader stops looking: bytes after the gzip member, a
/// second gzip member, a non-zero byte after the tar's end, and a damaged gzip trailer. A
/// reader that stops at the tar's end accepts all four; each is refused, and a tar padded
/// with zeros (as `tar` pads a record) is still fine.
///
/// Mutants seen red: the check of the bytes after the tar's end dropped (the non-zero
/// tail passes); the check of the compressed stream's end dropped (the trailing bytes and
/// the second member pass).
#[test]
fn nothing_follows_the_tar_or_the_gzip_member() {
    let clean = tar_gz(&[file("kbf", b"binary")]);
    let mut trailing = clean.clone();
    trailing.extend_from_slice(b"\0\0\0\0");
    let mut second = clean.clone();
    second.extend_from_slice(&clean);
    let mut tail = tar_of_one();
    tail.extend_from_slice(b"hidden");
    let mut damaged = clean.clone();
    let end = damaged.len();
    damaged[end - 8] ^= 0xff;
    let mut padded = tar_of_one();
    padded.resize(10240, 0);

    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("trailing-bytes", trailing, "archive-trailing"),
        ("second-member", second, "archive-trailing"),
        ("data-after-tar", gzip(&tail), "archive-trailing"),
        ("bad-crc", damaged, "archive-malformed"),
    ];
    for (name, bytes, want) in cases {
        let (dest, result) = unpack(&format!("archive-{name}"), &bytes, CAP);
        let err = result.expect_err(name);
        assert_eq!(err.code(), want, "{name}: {err}");
        let _ = fs::remove_file(dest);
    }

    let (dest, result) = unpack("archive-padded", &gzip(&padded), CAP);
    result.expect("a zero-padded tar is fine");
    fs::remove_file(dest).expect("remove");
}

/// Catches a gzip bomb: an archive whose second entry decompresses to far more than the
/// cap (the first is small and right) is stopped at the cap plus framing, not read to its
/// end; and an unpack that is out of time stops with its own code.
///
/// Mutants seen red: the decompressed bound dropped (the bomb is read in full); the
/// deadline check dropped (the zero budget is ignored).
#[test]
fn the_decompressed_size_and_the_time_are_bounded() {
    let bomb = tar_gz(&[file("kbf", b"x"), file("big", &vec![0u8; 8 << 20])]);
    let (dest, result) = unpack("archive-bomb", &bomb, CAP);
    assert_eq!(
        result.expect_err("bomb").code(),
        "archive-oversize",
        "the second entry is the bomb"
    );
    let _ = fs::remove_file(dest);

    let dir = scratch_dir("archive-slow");
    let source = dir.join("kbf.tar.gz");
    fs::write(&source, tar_gz(&[file("kbf", b"binary")])).expect("seed");
    let err =
        unpack_one(&source, "kbf", &dir.join("kbf"), CAP, Duration::ZERO).expect_err("no time");
    assert_eq!(err.code(), "archive-timeout");
    assert!(!dir.join("kbf").exists());
}
