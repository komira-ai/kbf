use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;
use kbf_testkit::dir::TestDir;

const TOOL: &[u8] = b"\x7fELF a pinned tool's bytes";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-fetch-unit"), name)
}

/// A tar of a directory `tool-1.0`, a decoy file, `tool-1.0/tool` holding `TOOL` and a
/// symlink `tool-1.0/link`.
fn tar() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_mode(0o755);
    dir.set_size(0);
    b.append_data(&mut dir, "tool-1.0/", &[][..]).expect("dir");
    for (path, data) in [("tool-1.0/README", &b"decoy"[..]), ("tool-1.0/tool", TOOL)] {
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(data.len() as u64);
        b.append_data(&mut h, path, data).expect("file");
    }
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    b.append_link(&mut link, "tool-1.0/link", "tool")
        .expect("link");
    b.into_inner().expect("tar")
}

/// A zip of the same shape as `tar()`: a directory, a decoy file, `tool-1.0/tool` holding
/// `TOOL` and a symlink `tool-1.0/link`.
fn zip() -> Vec<u8> {
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    w.add_directory("tool-1.0/", options).expect("dir");
    for (path, data) in [("tool-1.0/README", &b"decoy"[..]), ("tool-1.0/tool", TOOL)] {
        w.start_file(path, options).expect("file");
        w.write_all(data).expect("data");
    }
    w.add_symlink("tool-1.0/link", "tool", options)
        .expect("link");
    w.finish().expect("zip").into_inner()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).expect("gzip");
    e.finish().expect("gzip")
}

fn xz(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    lzma_rs::xz_compress(&mut &bytes[..], &mut out).expect("xz");
    out
}

fn zst(bytes: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
}

fn member(path: &str) -> PathBuf {
    PathBuf::from(path)
}

fn zip_member_of(path: &str) -> Unpack {
    Unpack::Zip {
        member: member(path),
    }
}

fn tar_gz(path: &str) -> Unpack {
    Unpack::Tar {
        xz: false,
        member: member(path),
    }
}

/// `install` of `bytes` (pinned at their own digest) into a fresh `out`.
fn install_own(name: &str, bytes: &[u8], unpack: &Unpack) -> (Result<(), Error>, PathBuf) {
    let _out_own = scratch(name);
    let out = _out_own.join("bin").join("tool");
    (install(bytes, &Sha256::of(bytes), unpack, &out), out)
}

#[test]
fn a_sha256_is_64_lowercase_hex_digits() {
    // Catches: a parser that accepts a short, upper-case or non-hex pin (a typo then
    // pins nothing), or that refuses the real form.
    let hex = "9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f";
    let digest: Sha256 = hex.parse().expect("parse");
    assert_eq!(digest.to_string(), hex);
    assert_eq!(format!("{digest:?}"), hex);
    for bad in [
        &hex[..62],
        &hex.to_uppercase(),
        &format!("{}g", &hex[..63]),
        "",
    ] {
        assert_eq!(
            bad.parse::<Sha256>(),
            Err(NotASha256(bad.to_owned())),
            "{bad}"
        );
    }
    assert_eq!(
        NotASha256("x".to_owned()).to_string(),
        "not a SHA-256: \"x\" (expected 64 lowercase hex digits)"
    );
}

#[test]
fn a_wrong_hash_is_refused_before_anything_is_unpacked_or_written() {
    // Catches: skipping the compare (the archive below unpacks fine, so the tool would
    // be installed), or comparing after unpacking (the second case is no archive at
    // all, so a late compare reports an unpack error instead of the mismatch).
    let dir = scratch("wrong-hash");
    let out = dir.join("tool");
    let archive = gzip(&tar());
    let pinned = Sha256::of(b"the bytes the pin was made from");
    for (bytes, unpack) in [
        (&archive[..], tar_gz("tool-1.0/tool")),
        (b"not gzip", tar_gz("x")),
    ] {
        assert_eq!(
            install(bytes, &pinned, &unpack, &out),
            Err(Error::Mismatch {
                expected: pinned,
                actual: Sha256::of(bytes)
            })
        );
        assert!(!out.exists(), "nothing at --out");
        assert!(!part_path(&out).exists(), "no part file");
    }
    assert_eq!(
        std::fs::read_dir(&dir).expect("ls").count(),
        0,
        "nothing written"
    );
}

#[test]
fn a_mismatch_names_both_digests() {
    // Catches: a message that leaves out the digest a maintainer needs to update the pin.
    let e = Error::Mismatch {
        expected: Sha256([0; 32]),
        actual: Sha256([0xff; 32]),
    };
    let text = e.to_string();
    assert!(
        text.contains(&"ff".repeat(32)) && text.contains(&"00".repeat(32)),
        "{text}"
    );
}

#[test]
fn a_matching_raw_download_is_installed_executable() {
    // Catches: a raw download altered on the way to disk, a file left without the
    // execute bits (the job's next step runs it), or a part file left beside it.
    let (result, out) = install_own("raw", TOOL, &Unpack::Raw);
    result.expect("install");
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
    let mode = std::fs::metadata(&out).expect("stat").permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    assert!(!part_path(&out).exists());
}

#[test]
fn each_format_yields_the_tool() {
    // Catches: a format decoded with the wrong codec, a tar member picked by the wrong
    // name (the decoy README comes first), or only the first of several zstd frames.
    let two_frames = [zst(&TOOL[..4]), zst(&TOOL[4..])].concat();
    let xz_tar = Unpack::Tar {
        xz: true,
        member: member("tool-1.0/tool"),
    };
    for (name, bytes, unpack) in [
        ("zst", zst(TOOL), Unpack::Zst),
        ("zst-frames", two_frames, Unpack::Zst),
        ("tar-gz", gzip(&tar()), tar_gz("tool-1.0/tool")),
        ("tar-xz", xz(&tar()), xz_tar),
        ("zip", zip(), zip_member_of("tool-1.0/tool")),
    ] {
        let (result, out) = install_own(name, &bytes, &unpack);
        result.expect(name);
        assert_eq!(std::fs::read(&out).expect("read"), TOOL, "{name}");
    }
}

#[test]
fn an_archive_that_does_not_decode_is_an_unpack_error_with_nothing_written() {
    // Catches: a decode failure that installs the raw bytes (or an empty file) instead
    // of failing; one case per decoder, and a tar whose header is not a tar's.
    let not_tar = [1u8; 1024];
    let xz_garbage = Unpack::Tar {
        xz: true,
        member: member("x"),
    };
    for (name, bytes, unpack) in [
        ("zst", b"not zstd".to_vec(), Unpack::Zst),
        ("zst-cut", zst(TOOL)[..8].to_vec(), Unpack::Zst),
        ("gz", b"not gzip".to_vec(), tar_gz("x")),
        ("xz", b"not xz".to_vec(), xz_garbage),
        ("tar", gzip(&not_tar), tar_gz("x")),
        ("zip", b"not a zip".to_vec(), zip_member_of("x")),
    ] {
        // Not `name` alone: each_format_yields_the_tool installs a "zst" at the same
        // time, in the same process, and its file made this test fail at random.
        let (result, out) = install_own(&format!("undecodable-{name}"), &bytes, &unpack);
        assert!(
            matches!(result, Err(Error::Unpack(_))),
            "{name}: {result:?}"
        );
        assert!(!out.exists() && !part_path(&out).exists(), "{name}");
    }
}

#[test]
fn a_missing_member_or_one_that_is_not_a_regular_file_is_refused() {
    // Catches: an installer that writes an empty file for a member the archive lacks
    // (a renamed directory upstream), or for a symlink or directory entry.
    let archive = gzip(&tar());
    let cases = [
        (
            "tool-1.0/cargo-deny",
            Error::NoMember(member("tool-1.0/cargo-deny")),
        ),
        ("tool-1.0/link", Error::NotAFile(member("tool-1.0/link"))),
        ("tool-1.0", Error::NotAFile(member("tool-1.0"))),
    ];
    for (path, expected) in cases {
        let (result, out) = install_own(path, &archive, &tar_gz(path));
        assert_eq!(result, Err(expected));
        assert!(!out.exists() && !part_path(&out).exists(), "{path}");
    }
    assert_eq!(
        Error::NoMember(member("a/b")).to_string(),
        "the archive has no \"a/b\""
    );
}

#[test]
fn a_missing_zip_member_or_one_that_is_not_a_regular_file_is_refused() {
    // Catches: a zip installer that writes an empty file for a member the archive lacks,
    // for a directory entry, or for a symlink (the unix mode bits say so), and one that
    // picks the decoy beside the tool.
    let archive = zip();
    let cases = [
        (
            "tool-1.0/cargo-deny",
            Error::NoMember(member("tool-1.0/cargo-deny")),
        ),
        ("tool-1.0/link", Error::NotAFile(member("tool-1.0/link"))),
        ("tool-1.0/", Error::NotAFile(member("tool-1.0/"))),
    ];
    for (path, expected) in cases {
        let (result, out) = install_own(&format!("zip-{path}"), &archive, &zip_member_of(path));
        assert_eq!(result, Err(expected), "{path}");
        assert!(!out.exists() && !part_path(&out).exists(), "{path}");
    }
}

#[test]
fn a_zip_entry_that_cannot_be_read_is_an_unpack_error() {
    // Catches: a lookup failure other than "no such member" reported as NoMember, or
    // swallowed: the entry's compression method is changed to one this build lacks.
    let mut archive = zip();
    // The central directory's entries start at PK\x01\x02; the compression method is the
    // two bytes at offset 10.
    let mut at = 0;
    while let Some(i) = archive[at..].windows(4).position(|w| w == b"PK\x01\x02") {
        archive[at + i + 10] = 12;
        at += i + 4;
    }
    let (result, out) = install_own("zip-method", &archive, &zip_member_of("tool-1.0/tool"));
    assert!(matches!(result, Err(Error::Unpack(_))), "{result:?}");
    assert!(!out.exists() && !part_path(&out).exists());
}

#[test]
fn an_install_that_cannot_rename_leaves_no_part_file() {
    // Catches: a part file left behind when the last step fails (the next run, or a
    // reader of the directory, then sees a half-installed tool): --out is a directory,
    // so the rename over it fails after the part file was written.
    let _out_own = scratch("rename-fails");
    let out = _out_own.join("tool");
    std::fs::create_dir_all(out.join("inside")).expect("dir");
    let result = install(TOOL, &Sha256::of(TOOL), &Unpack::Raw, &out);
    let Err(Error::Install { path, why }) = result else {
        panic!("{result:?}");
    };
    assert_eq!(path, out);
    assert!(!why.is_empty());
    assert!(!part_path(&out).exists(), "part file removed");
    assert!(out.join("inside").is_dir(), "the old --out is untouched");
}

#[test]
fn an_install_replaces_an_old_tool_and_a_stale_part_file() {
    // Catches: refusing to replace an existing --out (a cached older version), or
    // failing on a part file a killed run left.
    let _out_own = scratch("replace");
    let out = _out_own.join("tool");
    std::fs::write(&out, b"old").expect("old");
    std::fs::write(part_path(&out), b"half").expect("stale part");
    install(TOOL, &Sha256::of(TOOL), &Unpack::Raw, &out).expect("install");
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
    assert!(!part_path(&out).exists());
}

#[test]
fn extract_and_member_must_agree() {
    // Catches: a tar format accepted without the member to take (what would it
    // install?), or a member silently ignored for a format that has none.
    let m = || Some(member("d/tool"));
    assert_eq!(Unpack::new(None, None), Ok(Unpack::Raw));
    assert_eq!(Unpack::new(Some(Format::Zst), None), Ok(Unpack::Zst));
    assert_eq!(Unpack::new(Some(Format::TarGz), m()), Ok(tar_gz("d/tool")));
    assert_eq!(
        Unpack::new(Some(Format::TarXz), m()),
        Ok(Unpack::Tar {
            xz: true,
            member: member("d/tool")
        })
    );
    assert_eq!(
        Unpack::new(Some(Format::Zip), m()),
        Ok(zip_member_of("d/tool"))
    );
    for format in [Format::TarGz, Format::TarXz, Format::Zip] {
        let e = Unpack::new(Some(format), None).expect_err("no member");
        assert!(e.contains("need --member"), "{e}");
    }
    for format in [None, Some(Format::Zst)] {
        let e = Unpack::new(format, m()).expect_err("member out of place");
        assert!(e.contains("needs --extract tar.gz, tar.xz or zip"), "{e}");
    }
}

#[test]
fn the_part_file_sits_beside_out() {
    // Catches: a part file in another directory (the rename then crosses file systems,
    // and is no longer atomic).
    assert_eq!(
        part_path(Path::new("/a/b/tool")),
        Path::new("/a/b/tool.part")
    );
}
