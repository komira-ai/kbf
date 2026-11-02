use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use crate::fixtures::{Item, file, scratch_dir, tar_gz};

const CAP: u64 = 1024;
const WITHIN: Duration = Duration::from_secs(60);
const DECLARATION: &str = "updater.d/kbf.toml";

/// What the stage asks of a component's tarball: the binary `kbf` (required, 0755, at most
/// `cap`) into `dir/kbf`, and its declaration (optional, 0644) into `dir/updater.d/kbf.toml`.
fn unpack_into(
    dir: &Path,
    tarball: &Path,
    cap: u64,
    within: Duration,
) -> Result<Unpacked, ArchiveFault> {
    let binary = dir.join("kbf");
    let declaration = dir.join(DECLARATION);
    let members = [
        Member {
            name: "kbf",
            dest: &binary,
            mode: 0o755,
            cap,
            required: true,
        },
        Member {
            name: &declaration_member("kbf"),
            dest: &declaration,
            mode: 0o644,
            cap: DECLARATION_CAP,
            required: false,
        },
    ];
    unpack_members(tarball, &members, within)
}

/// Unpack `tarball` as the stage does into a fresh directory `<dir>/out`; that directory and
/// what the call said.
fn unpack(name: &str, tarball: &[u8], cap: u64) -> (PathBuf, Result<Unpacked, ArchiveFault>) {
    let dir = scratch_dir(name);
    let source = dir.join("kbf.tar.gz");
    fs::write(&source, tarball).expect("seed");
    let out = dir.join("out");
    fs::create_dir(&out).expect("out");
    let result = unpack_into(&out, &source, cap, WITHIN);
    (out, result)
}

/// The names of what `dir` holds, every level, sorted.
fn listing(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for item in fs::read_dir(dir).expect("list") {
        let path = item.expect("item").path();
        let name = path.strip_prefix(dir).expect("under").display().to_string();
        if path.is_dir() {
            found.extend(listing(&path).into_iter().map(|n| format!("{name}/{n}")));
        }
        found.push(name);
    }
    found.sort();
    found
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

/// An executable left in the target directory is read by cargo-llvm-cov as an object.
fn remove_binary(out: &Path) {
    let _ = fs::remove_file(out.join("kbf"));
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// Catches a single-member tarball (every release kbf has made) unpacking differently: its
/// bytes and mode 0755, nothing else written (no `updater.d/`), nothing ignored.
#[test]
fn the_one_file_is_unpacked_whole_and_executable() {
    let (out, result) = unpack("archive-ok", &tar_gz(&[file("kbf", b"binary bytes")]), CAP);
    assert_eq!(
        result.expect("unpacked"),
        Unpacked {
            taken: names(&["kbf"]),
            ignored: Vec::new(),
        }
    );
    assert_eq!(fs::read(out.join("kbf")).expect("there"), b"binary bytes");
    assert_eq!(mode_of(&out.join("kbf")), 0o755);
    assert_eq!(listing(&out), names(&["kbf"]));
    remove_binary(&out);
}

/// Catches the declaration a release carries beside its binary not reaching its place: both
/// members come out whole, the binary 0755 and the declaration 0644 under `updater.d/`,
/// whichever order the tarball has them in.
#[test]
fn the_binary_and_its_declaration_are_both_unpacked() {
    let toml = b"name = \"kbf\"\n";
    for (why, tarball) in [
        (
            "binary-first",
            tar_gz(&[file("kbf", b"binary"), file(DECLARATION, toml)]),
        ),
        (
            "declaration-first",
            tar_gz(&[file(DECLARATION, toml), file("kbf", b"binary")]),
        ),
    ] {
        let (out, result) = unpack(&format!("archive-two-{why}"), &tarball, CAP);
        let unpacked = result.expect(why);
        assert!(unpacked.ignored.is_empty(), "{why}");
        let mut taken = unpacked.taken.clone();
        taken.sort();
        assert_eq!(taken, names(&["kbf", DECLARATION]), "{why}");
        assert_eq!(fs::read(out.join("kbf")).expect("binary"), b"binary");
        assert_eq!(mode_of(&out.join("kbf")), 0o755);
        assert_eq!(fs::read(out.join(DECLARATION)).expect("declaration"), toml);
        assert_eq!(mode_of(&out.join(DECLARATION)), 0o644);
        assert_eq!(mode_of(&out.join("updater.d")), 0o755);
        assert_eq!(listing(&out), names(&["kbf", "updater.d", DECLARATION]));
        remove_binary(&out);
    }
}

/// Catches a member no one asked for being refused or written: a README, a file under a
/// directory, a directory entry and another component's declaration are each passed over
/// and listed, and only the binary is written.
#[test]
fn a_member_not_asked_for_is_ignored_and_listed() {
    let dir_entry = Item {
        name: b"docs/",
        kind: b'5',
        data: b"",
    };
    let tarball = tar_gz(&[
        file("README", b"read me"),
        dir_entry,
        file("docs/install.md", b"how"),
        file("kbf", b"binary"),
        file("updater.d/tierstore.toml", b"name = \"tierstore\"\n"),
    ]);
    let (out, result) = unpack("archive-extra", &tarball, CAP);
    assert_eq!(
        result.expect("extras are ignored"),
        Unpacked {
            taken: names(&["kbf"]),
            ignored: names(&[
                "README",
                "docs/",
                "docs/install.md",
                "updater.d/tierstore.toml"
            ]),
        }
    );
    assert_eq!(listing(&out), names(&["kbf"]));
    remove_binary(&out);
}

/// Catches an unsafe member being let through because it is not one the unpack wants, or
/// because it is: a name that is absolute, has a `..` component or is empty, and a symbolic
/// link, hard link, device, FIFO or other special member refuse the archive whether they
/// stand alone or beside a good binary, and the refusal names the member. Nothing is ever
/// written for the unsafe member.
#[test]
fn every_member_is_checked_and_an_unsafe_one_is_refused_by_name() {
    let item = |name: &'static str, kind: u8| Item {
        name: name.as_bytes(),
        kind,
        data: b"",
    };
    let cases: Vec<(&str, Item, &str, &str)> = vec![
        (
            "absolute",
            item("/etc/updater.d/kbf.toml", b'0'),
            "archive-unsafe-path",
            "its member \"/etc/updater.d/kbf.toml\" has a name that leaves the directory it is unpacked into",
        ),
        (
            "absolute-binary",
            item("/kbf", b'0'),
            "archive-unsafe-path",
            "its member \"/kbf\" has a name that leaves the directory it is unpacked into",
        ),
        (
            "dotdot",
            item("../kbf", b'0'),
            "archive-unsafe-path",
            "its member \"../kbf\" has a name that leaves the directory it is unpacked into",
        ),
        (
            "dotdot-inside",
            item("updater.d/../../kbf.toml", b'0'),
            "archive-unsafe-path",
            "its member \"updater.d/../../kbf.toml\" has a name that leaves the directory it is unpacked into",
        ),
        (
            "empty-name",
            item("", b'0'),
            "archive-unsafe-path",
            "its member \"\" has a name that leaves the directory it is unpacked into",
        ),
        (
            "symlink",
            item("evil", b'2'),
            "archive-not-regular",
            "its member \"evil\" is a symbolic link, not a regular file",
        ),
        (
            "symlink-declaration",
            item(DECLARATION, b'2'),
            "archive-not-regular",
            "its member \"updater.d/kbf.toml\" is a symbolic link, not a regular file",
        ),
        (
            "hardlink",
            item("evil", b'1'),
            "archive-not-regular",
            "its member \"evil\" is a hard link, not a regular file",
        ),
        (
            "chardev",
            item("null", b'3'),
            "archive-not-regular",
            "its member \"null\" is a device, not a regular file",
        ),
        (
            "blockdev",
            item("sda", b'4'),
            "archive-not-regular",
            "its member \"sda\" is a device, not a regular file",
        ),
        (
            "fifo",
            item("pipe", b'6'),
            "archive-not-regular",
            "its member \"pipe\" is a FIFO, not a regular file",
        ),
        (
            "contiguous",
            item("odd", b'7'),
            "archive-not-regular",
            "its member \"odd\" is a member of another kind, not a regular file",
        ),
    ];
    for (name, bad, code, message) in cases {
        for (place, tarball) in [
            ("alone", tar_gz(&[Item { ..bad }])),
            ("first", tar_gz(&[Item { ..bad }, file("kbf", b"binary")])),
            ("after", tar_gz(&[file("kbf", b"binary"), Item { ..bad }])),
        ] {
            let (out, result) = unpack(&format!("archive-unsafe-{name}-{place}"), &tarball, CAP);
            let err = result.expect_err(name);
            assert_eq!(err.code(), code, "{name} {place}: {err}");
            assert_eq!(err.to_string(), message, "{name} {place}");
            // Only the binary before it may have been written (its directory goes with
            // the refusal); never the unsafe member.
            let want = if place == "after" {
                names(&["kbf"])
            } else {
                Vec::new()
            };
            assert_eq!(listing(&out), want, "{name} {place}");
            remove_binary(&out);
        }
    }
}

/// Catches an archive that lacks a member the unpack requires being accepted, or refused
/// without saying which: a tarball of the declaration alone, of another name, of a name with
/// a path (`bin/kbf`, `./kbf`) and of nothing at all.
#[test]
fn a_missing_required_member_is_refused_by_name() {
    for (why, tarball) in [
        ("declaration-alone", tar_gz(&[file(DECLARATION, b"x")])),
        ("other-name", tar_gz(&[file("kbf-daemon", b"a")])),
        ("dir-prefix", tar_gz(&[file("bin/kbf", b"a")])),
        ("dot-slash", tar_gz(&[file("./kbf", b"a")])),
    ] {
        let (out, result) = unpack(&format!("archive-missing-{why}"), &tarball, CAP);
        let err = result.expect_err(why);
        assert_eq!(err.code(), "archive-name", "{why}");
        assert_eq!(err.to_string(), "it holds no member named \"kbf\"", "{why}");
        let _ = fs::remove_dir_all(out.join("updater.d"));
    }
    let (_, result) = unpack("archive-missing-none", &tar_gz(&[]), CAP);
    assert_eq!(result.expect_err("none").code(), "archive-entries");
}

/// Catches a wanted member that is not a plain file of its size once being taken: a
/// directory under the binary's or the declaration's name, a second copy of either, and
/// either over its cap (the declaration's is [`DECLARATION_CAP`]).
#[test]
fn a_wanted_member_must_be_one_regular_file_within_its_cap() {
    let dir_named = |name: &'static str| Item {
        name: name.as_bytes(),
        kind: b'5',
        data: b"",
    };
    let big = vec![b'#'; usize::try_from(DECLARATION_CAP).expect("small") + 1];
    let cases: Vec<(&str, Vec<u8>, &str, &str)> = vec![
        (
            "binary-dir",
            tar_gz(&[dir_named("kbf")]),
            "archive-not-regular",
            "its member \"kbf\" is a directory, not a regular file",
        ),
        (
            "declaration-dir",
            tar_gz(&[file("kbf", b"a"), dir_named(DECLARATION)]),
            "archive-not-regular",
            "its member \"updater.d/kbf.toml\" is a directory, not a regular file",
        ),
        (
            "binary-twice",
            tar_gz(&[file("kbf", b"a"), file("kbf", b"b")]),
            "archive-duplicate",
            "its member \"kbf\" is in it twice",
        ),
        (
            "declaration-twice",
            tar_gz(&[
                file(DECLARATION, b"a"),
                file("kbf", b"a"),
                file(DECLARATION, b"b"),
            ]),
            "archive-duplicate",
            "its member \"updater.d/kbf.toml\" is in it twice",
        ),
        (
            "binary-oversize",
            tar_gz(&[file("kbf", &[7u8; 1025])]),
            "archive-oversize",
            "it is larger than 1024 bytes (its entry, or what it decompresses to)",
        ),
        (
            "declaration-oversize",
            tar_gz(&[file("kbf", b"a"), file(DECLARATION, &big)]),
            "archive-oversize",
            "it is larger than 65536 bytes (its entry, or what it decompresses to)",
        ),
    ];
    for (name, bytes, code, message) in cases {
        let (out, result) = unpack(&format!("archive-wanted-{name}"), &bytes, CAP);
        let err = result.expect_err(name);
        assert_eq!(err.code(), code, "{name}: {err}");
        assert_eq!(err.to_string(), message, "{name}");
        remove_binary(&out);
    }
}

/// Catches a tarball that is not one at all being reported as anything but malformed.
#[test]
fn an_archive_that_is_not_a_gzipped_tar_is_malformed() {
    for (name, bytes) in [
        ("not-gzip", b"this is not a tarball".to_vec()),
        ("empty-file", Vec::new()),
    ] {
        let (out, result) = unpack(&format!("archive-{name}"), &bytes, CAP);
        assert_eq!(
            result.expect_err(name).code(),
            "archive-malformed",
            "{name}"
        );
        assert!(listing(&out).is_empty(), "{name}");
    }
}

/// Catches the cap being exceeded by an entry whose header lies about its size: the
/// header says 10 bytes and the archive holds more (a tar reader that trusts the header
/// would stop early); with the cap at 4 the entry is refused by its header before any
/// byte is written.
#[test]
fn the_cap_is_checked_before_a_byte_is_written() {
    let (out, result) = unpack(
        "archive-cap-header",
        &tar_gz(&[file("kbf", b"0123456789")]),
        4,
    );
    assert_eq!(result.expect_err("over").code(), "archive-oversize");
    assert!(!out.join("kbf").exists());
}

/// Catches a tarball that cannot be opened being reported as anything but malformed.
#[test]
fn a_missing_tarball_is_malformed() {
    let dir = scratch_dir("archive-no-tarball");
    let err = unpack_into(&dir, &dir.join("none.tar.gz"), CAP, WITHIN).expect_err("gone");
    assert_eq!(err.code(), "archive-malformed");
}

/// Catches a destination that already exists being overwritten (the unpack uses
/// `create_new`): an earlier file of the scratch directory is never replaced, the binary's
/// or the declaration's.
#[test]
fn an_existing_destination_is_never_replaced() {
    let dir = scratch_dir("archive-exists");
    let source = dir.join("kbf.tar.gz");
    fs::write(&source, tar_gz(&[file("kbf", b"new")])).expect("seed");
    let dest = dir.join("kbf");
    fs::write(&dest, b"old").expect("seed");
    let err = unpack_into(&dir, &source, CAP, WITHIN).expect_err("exists");
    assert_eq!(err.code(), "archive-malformed");
    assert_eq!(fs::read(&dest).expect("there"), b"old");

    let dir = scratch_dir("archive-exists-declaration");
    let source = dir.join("kbf.tar.gz");
    let tarball = tar_gz(&[file(DECLARATION, b"new"), file("kbf", b"a")]);
    fs::write(&source, tarball).expect("seed");
    fs::create_dir(dir.join("updater.d")).expect("dir");
    fs::write(dir.join(DECLARATION), b"old").expect("seed");
    let err = unpack_into(&dir, &source, CAP, WITHIN).expect_err("exists");
    assert_eq!(err.code(), "archive-malformed");
    assert_eq!(fs::read(dir.join(DECLARATION)).expect("there"), b"old");
}

/// Catches the declaration's directory not being made, or a file in its place being
/// replaced: where `updater.d` is a file, the unpack is refused and the file stays.
#[test]
fn a_declaration_whose_directory_cannot_be_made_is_refused() {
    let dir = scratch_dir("archive-no-dir");
    let source = dir.join("kbf.tar.gz");
    fs::write(
        &source,
        tar_gz(&[file(DECLARATION, b"x"), file("kbf", b"a")]),
    )
    .expect("seed");
    fs::write(dir.join("updater.d"), b"a file").expect("seed");
    let err = unpack_into(&dir, &source, CAP, WITHIN).expect_err("not a directory");
    assert_eq!(err.code(), "archive-malformed");
    assert_eq!(fs::read(dir.join("updater.d")).expect("there"), b"a file");
}

/// Catches the declaration being written through a link, or into a directory that is not
/// there: where `updater.d` is a link to another directory the unpack is refused and nothing
/// is written there, and a destination whose parent is missing is refused.
#[test]
fn a_declaration_is_never_written_through_a_link_or_into_a_missing_directory() {
    let dir = scratch_dir("archive-dir-link");
    let elsewhere = dir.join("elsewhere");
    fs::create_dir(&elsewhere).expect("seed");
    let out = dir.join("out");
    fs::create_dir(&out).expect("out");
    std::os::unix::fs::symlink(&elsewhere, out.join("updater.d")).expect("link");
    let source = dir.join("kbf.tar.gz");
    fs::write(
        &source,
        tar_gz(&[file(DECLARATION, b"x"), file("kbf", b"a")]),
    )
    .expect("seed");
    let err = unpack_into(&out, &source, CAP, WITHIN).expect_err("a link");
    assert_eq!(err.code(), "archive-malformed");
    assert!(listing(&elsewhere).is_empty(), "written through the link");

    let deep = dir.join("missing").join("deeper").join("kbf");
    let members = [Member {
        name: "kbf",
        dest: &deep,
        mode: 0o755,
        cap: CAP,
        required: true,
    }];
    let err = unpack_members(&source, &members, WITHIN).expect_err("no parent");
    assert_eq!(err.code(), "archive-malformed");
    assert!(!dir.join("missing").exists());
}

/// Catches the member name of a component's declaration drifting from the one a release
/// carries (`updater.d/<component>.toml`).
#[test]
fn the_declaration_member_is_named_for_the_component_under_updater_d() {
    assert_eq!(declaration_member("tierstore"), "updater.d/tierstore.toml");
    assert_eq!(
        declaration_member("kbf-daemon"),
        "updater.d/kbf-daemon.toml"
    );
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
/// Catches: the check of the bytes after the tar's end dropped (the non-zero
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
        let (out, result) = unpack(&format!("archive-{name}"), &bytes, CAP);
        let err = result.expect_err(name);
        assert_eq!(err.code(), want, "{name}: {err}");
        remove_binary(&out);
    }

    let (out, result) = unpack("archive-padded", &gzip(&padded), CAP);
    result.expect("a zero-padded tar is fine");
    remove_binary(&out);
}

/// Catches a gzip bomb: an archive whose second entry, one no one asked for, decompresses to
/// far more than the caps (the first is small and right) is stopped at the caps plus
/// framing, not read to its end; and an unpack that is out of time stops with its own code.
///
/// Catches: the decompressed bound dropped (the bomb is read in full); the
/// deadline check dropped (the zero budget is ignored).
#[test]
fn the_decompressed_size_and_the_time_are_bounded() {
    let bomb = tar_gz(&[file("kbf", b"x"), file("big", &vec![0u8; 8 << 20])]);
    let (out, result) = unpack("archive-bomb", &bomb, CAP);
    let err = result.expect_err("bomb");
    assert_eq!(
        err.code(),
        "archive-oversize",
        "the second entry is the bomb"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "it is larger than {} bytes (its entry, or what it decompresses to)",
            CAP + DECLARATION_CAP
        )
    );
    remove_binary(&out);

    let dir = scratch_dir("archive-slow");
    let source = dir.join("kbf.tar.gz");
    fs::write(&source, tar_gz(&[file("kbf", b"binary")])).expect("seed");
    let err = unpack_into(&dir, &source, CAP, Duration::ZERO).expect_err("no time");
    assert_eq!(err.code(), "archive-timeout");
    assert!(!dir.join("kbf").exists());
}
