use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::*;
use kbf_host::testkit::scratch;

const RECORD_BYTES: &[u8] = b"kbf raft log record 1\nlog_id 000000000000002a\nserver 1\nvoters 1\n";
const SEG_1: &str = "00000000000000000001-00000000000000000001.log";
const SEG_2: &str = "00000000000000000002-00000000000000000040.log";

/// A metadata directory as a stopped kbf-server leaves it, with two `*.tmp` files a
/// write cut short leaves; and where the archive goes.
fn meta_dir(name: &str) -> (PathBuf, PathBuf) {
    let root = scratch(&format!("meta_archive_{name}"));
    let meta = root.join("meta");
    fs::create_dir(&meta).expect("meta dir");
    for (file, bytes) in [
        (RECORD, RECORD_BYTES),
        ("log.lock", b"".as_slice()),
        ("hardstate", b"term 3 vote 1"),
        (SEG_1, b"entries one to sixty-three".as_slice()),
        (SEG_2, &[0x5a; 3000]),
        ("hardstate.tmp", b"half a hard state"),
        ("log-record.tmp", b"half a record"),
    ] {
        fs::write(meta.join(file), bytes).expect("a meta file");
    }
    (meta, root.join("backup.tar"))
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The manifest inside an archive, read with the tar crate alone.
fn manifest_of(archive: &Path) -> String {
    let mut tar = tar::Archive::new(File::open(archive).expect("open"));
    for e in tar.entries().expect("entries") {
        let mut e = e.expect("entry");
        if e.path().expect("path").to_str() == Some(MANIFEST) {
            let mut s = String::new();
            e.read_to_string(&mut s).expect("read");
            return s;
        }
    }
    panic!("no manifest in {archive:?}");
}

/// Catches: a manifest that leaves a file out or lists a `*.tmp` (one a server would
/// remove at start, so a verify of a restored copy could never match it), an archive
/// holding a `*.tmp`, a line with a wrong hash, and entries in an order that varies
/// with the directory's (two backups of one directory must list the same way). The
/// mutant "archive every name, `*.tmp` included" goes red on both the entry list and
/// the manifest.
#[test]
fn the_manifest_covers_every_file_and_no_tmp() {
    let (meta, out) = meta_dir("covers");
    let w = write(&meta, &out).expect("write");
    // Byte order: digits before letters, `-` before `.`.
    let files = [SEG_1, SEG_2, "hardstate", RECORD, "log.lock"];
    assert_eq!(w.files, files.len());
    let mut want: Vec<PathBuf> = files
        .iter()
        .map(|f| PathBuf::from(format!("meta/{f}")))
        .collect();
    want.push(PathBuf::from(MANIFEST));
    assert_eq!(entry_names(&out), want);
    let lines: Vec<String> = files
        .iter()
        .map(|f| {
            let bytes = fs::read(meta.join(f)).expect("read");
            format!("{}  meta/{f}", sha(&bytes))
        })
        .collect();
    assert_eq!(manifest_of(&out), lines.join("\n") + "\n");
    let bytes: u64 = files
        .iter()
        .map(|f| fs::metadata(meta.join(f)).expect("stat").len())
        .sum();
    assert_eq!(w.bytes, bytes);
    assert_eq!(w.sha256, sha(&fs::read(&out).expect("archive")));
    assert!(
        !out.with_file_name(".backup.tar.partial").exists(),
        "the hidden file outlived the write"
    );
}

/// Catches: a restore that changes a byte, drops a file, or counts the wrong things
/// (the segment count is what the RTO table records beside the bytes).
#[test]
fn a_round_trip_restores_every_file_byte_for_byte() {
    let (meta, out) = meta_dir("round_trip");
    write(&meta, &out).expect("write");
    let into = out.with_file_name("restored");
    let r = extract(&out, &into).expect("extract");
    assert_eq!((r.files, r.segments), (5, 2));
    assert_eq!(r.log_id, "000000000000002a");
    for f in ["hardstate", RECORD, "log.lock", SEG_1, SEG_2] {
        assert_eq!(
            fs::read(into.join(f)).ok(),
            fs::read(meta.join(f)).ok(),
            "{f}"
        );
    }
    assert!(!into.join("hardstate.tmp").exists());
    assert_eq!(fs::read_dir(&into).expect("list").count(), 5);
}

/// Catches: a verify that trusts the archive's bytes without hashing them (the mutant
/// "skip the hash compare" restores the flipped byte and exits 0), and one that leaves
/// the bad copy behind for someone to start a server over.
#[test]
fn verify_catches_a_flipped_byte() {
    let (meta, out) = meta_dir("flipped");
    write(&meta, &out).expect("write");
    let mut bytes = fs::read(&out).expect("archive");
    let at = bytes
        .windows(b"term 3 vote 1".len())
        .position(|w| w == b"term 3 vote 1")
        .expect("the hard state's bytes");
    bytes[at] ^= 0x01;
    fs::write(&out, bytes).expect("flip");
    let into = out.with_file_name("restored");
    let e = extract(&out, &into).expect_err("a flipped byte verified");
    assert!(
        matches!(&e, MetaError::Checksum { name } if name == "hardstate"),
        "{e}"
    );
    assert_eq!(e.exit_code(), 1);
    assert!(!into.exists(), "the unchecked copy was left behind");
}

/// Catches: a verify that passes an archive with no log record. A server over that
/// copy refuses the directory ("holds files but no log-record"), so the backup is no
/// backup.
#[test]
fn verify_catches_a_missing_log_record() {
    let (meta, out) = meta_dir("no_record");
    fs::remove_file(meta.join(RECORD)).expect("drop the record");
    write(&meta, &out).expect("write");
    let into = out.with_file_name("restored");
    let e = extract(&out, &into).expect_err("an archive without a record verified");
    assert!(matches!(e, MetaError::NoRecordInArchive), "{e}");
    assert_eq!(e.exit_code(), 1);
    assert!(!into.exists());
}

/// Catches: a restore over a directory that already holds something (a live log, or
/// an earlier restore), which would mix two logs' files.
#[test]
fn verify_refuses_a_non_empty_target() {
    let (meta, out) = meta_dir("non_empty");
    write(&meta, &out).expect("write");
    let into = out.with_file_name("restored");
    fs::create_dir(&into).expect("target");
    fs::write(into.join("hardstate"), b"someone else's").expect("a file");
    let e = extract(&out, &into).expect_err("a non-empty target was used");
    assert!(matches!(e, MetaError::NotEmpty(_)), "{e}");
    assert_eq!(e.exit_code(), 2);
    assert_eq!(
        fs::read(into.join("hardstate")).ok(),
        Some(b"someone else's".to_vec())
    );

    // An empty directory that exists is used, and kept when the archive is bad.
    let empty = out.with_file_name("empty");
    fs::create_dir(&empty).expect("empty");
    extract(&out, &empty).expect("an empty target");
    fs::write(&out, b"not a tar").expect("spoil");
    let empty2 = out.with_file_name("empty2");
    fs::create_dir(&empty2).expect("empty2");
    assert!(extract(&out, &empty2).is_err());
    assert!(
        empty2.is_dir(),
        "a directory the verb did not make was removed"
    );
}

/// Builds an archive by hand: `files` as given (paths as written), then `manifest`.
fn hand_made(out: &Path, files: &[(&str, &[u8])], manifest: Option<&str>) {
    let mut tar = tar::Builder::new(File::create(out).expect("create"));
    let mut put = |path: &str, bytes: &[u8]| {
        let mut h = header(bytes.len() as u64, 0);
        h.set_path(path).expect("path");
        h.set_cksum();
        tar.append(&h, bytes).expect("append");
    };
    for (p, b) in files {
        put(p, b);
    }
    if let Some(m) = manifest {
        put(MANIFEST, m.as_bytes());
    }
    tar.finish().expect("finish");
}

/// A hand-made archive: its name, its entries and its manifest.
type Case<'a> = (&'a str, Vec<(&'a str, &'a [u8])>, Option<String>);

/// Catches: a verify that checks only the files present (a manifest line for a file
/// the archive lost passes), only the files listed (an extra file passes), or none
/// when the manifest itself is gone; and an entry outside `meta/` written where its
/// name points.
#[test]
fn verify_catches_a_lost_an_unlisted_and_a_stray_entry() {
    let root = scratch("meta_archive_hand_made");
    let a = root.join("a.tar");
    let rec = format!("{}  meta/{RECORD}\n", sha(RECORD_BYTES));
    let lost = format!("{rec}{}  meta/hardstate\n", sha(b"x"));
    let cases: [Case<'_>; 5] = [
        ("lost", vec![("meta/log-record", RECORD_BYTES)], Some(lost)),
        (
            "unlisted",
            vec![("meta/log-record", RECORD_BYTES), ("meta/hardstate", b"x")],
            Some(rec.clone()),
        ),
        ("no manifest", vec![("meta/log-record", RECORD_BYTES)], None),
        (
            "outside meta/",
            vec![("hardstate", b"x")],
            Some(rec.clone()),
        ),
        (
            "nested",
            vec![("meta/sub/hardstate", b"x")],
            Some(rec.clone()),
        ),
    ];
    for (i, (case, files, manifest)) in cases.into_iter().enumerate() {
        hand_made(&a, &files, manifest.as_deref());
        let into = root.join(format!("into{i}"));
        let e = extract(&a, &into).expect_err(case);
        let ok = match case {
            "lost" => matches!(&e, MetaError::Missing { name } if name == "hardstate"),
            "unlisted" => matches!(&e, MetaError::Unlisted { name } if name == "hardstate"),
            "no manifest" => matches!(e, MetaError::NoManifest(_)),
            _ => matches!(e, MetaError::Archive(_)),
        };
        assert!(ok, "{case}: {e}");
        assert_eq!(e.exit_code(), 1, "{case}");
        assert!(!into.exists(), "{case}: a bad restore was left");
        assert!(
            !root.join("hardstate").exists(),
            "{case}: written outside the target"
        );
    }
}

/// Catches: a backup that skips what is not a regular file (a directory the log does
/// not use today would be lost without a word), and a hidden file left after it.
#[test]
fn write_refuses_a_directory_and_leaves_nothing() {
    let (meta, out) = meta_dir("subdir");
    fs::create_dir(meta.join("snapshots")).expect("a directory");
    let e = write(&meta, &out).expect_err("a directory was archived or skipped");
    assert!(matches!(e, MetaError::NotFile { .. }), "{e}");
    assert!(!out.exists());
    assert!(!out.with_file_name(".backup.tar.partial").exists());
}

/// Catches: a write that replaces an archive already at `out` (an earlier backup).
#[test]
fn write_never_replaces_a_file() {
    let (meta, out) = meta_dir("replace");
    fs::write(&out, b"an earlier backup").expect("earlier");
    assert!(write(&meta, &out).is_err());
    assert_eq!(fs::read(&out).ok(), Some(b"an earlier backup".to_vec()));
    assert!(!out.with_file_name(".backup.tar.partial").exists());
}

/// One raw entry: its path's bytes, its type and its data.
type Raw<'a> = (&'a [u8], tar::EntryType, &'a [u8]);

/// An archive of raw entries, in order.
fn raw(out: &Path, entries: &[Raw<'_>]) {
    use std::os::unix::ffi::OsStrExt;
    let mut tar = tar::Builder::new(File::create(out).expect("create"));
    for (path, kind, data) in entries {
        let mut h = header(data.len() as u64, 0);
        h.set_entry_type(*kind);
        h.set_path(std::ffi::OsStr::from_bytes(path)).expect("path");
        h.set_cksum();
        tar.append(&h, *data).expect("append");
    }
    tar.finish().expect("finish");
}

/// Catches: a verify that takes an entry it cannot name (not UTF-8, empty after
/// `meta/`, given twice), a directory or other non-file entry, two manifests, a
/// manifest that is not text, or a manifest line that is not `<sha256>  meta/<name>`
/// or names a file twice. Each is a broken archive (exit 1), and nothing is left.
#[test]
fn verify_refuses_each_malformed_entry_and_manifest_line() {
    use tar::EntryType::{Directory, Regular};
    let root = scratch("meta_archive_malformed");
    let a = root.join("a.tar");
    let rec_line = format!("{}  meta/{RECORD}\n", sha(RECORD_BYTES));
    let rec: Raw<'_> = (b"meta/log-record", Regular, RECORD_BYTES);
    let manifest = |text: &'static [u8]| -> Raw<'static> { (MANIFEST.as_bytes(), Regular, text) };
    let leak = |s: String| -> &'static [u8] { s.into_bytes().leak() };
    let twice = leak(format!("{rec_line}{rec_line}"));
    let no_gap = leak(format!("{} meta/{RECORD}\n", sha(RECORD_BYTES)));
    let no_prefix = leak(format!("{}  {RECORD}\n", sha(RECORD_BYTES)));
    let ok = leak(rec_line.clone());
    let cases: Vec<(&str, Vec<Raw<'_>>)> = vec![
        (
            "name not UTF-8",
            vec![rec, (b"meta/\xff", Regular, b"x"), manifest(ok)],
        ),
        (
            "a directory",
            vec![rec, (b"meta/sub", Directory, b""), manifest(ok)],
        ),
        (
            "empty name",
            vec![rec, (b"meta/", Regular, b""), manifest(ok)],
        ),
        ("entry twice", vec![rec, rec, manifest(ok)]),
        ("manifest twice", vec![rec, manifest(ok), manifest(ok)]),
        ("manifest not text", vec![rec, manifest(b"\xff\xfe")]),
        ("line without two spaces", vec![rec, manifest(no_gap)]),
        ("line without meta/", vec![rec, manifest(no_prefix)]),
        ("line twice", vec![rec, manifest(twice)]),
    ];
    for (i, (case, entries)) in cases.into_iter().enumerate() {
        raw(&a, &entries);
        let into = root.join(format!("into{i}"));
        let e = extract(&a, &into).expect_err(case);
        assert!(matches!(e, MetaError::Archive(_)), "{case}: {e}");
        assert_eq!(e.exit_code(), 1, "{case}");
        assert!(!into.exists(), "{case}: a bad restore was left");
    }
}

/// Catches: a target that is a file, or that cannot be read, taken for an empty
/// directory.
#[test]
fn verify_refuses_a_target_it_cannot_use() {
    let (meta, out) = meta_dir("bad_target");
    write(&meta, &out).expect("write");
    let file = out.with_file_name("a-file");
    fs::write(&file, b"x").expect("a file");
    let e = extract(&out, &file).expect_err("a file as the target");
    assert!(matches!(e, MetaError::NotDir(_)), "{e}");
    assert_eq!(e.exit_code(), 2);
    let long = out.with_file_name("n".repeat(300));
    let e = extract(&out, &long).expect_err("a name too long");
    assert!(matches!(e, MetaError::Io { .. }), "{e}");
    let e = extract(&out.with_file_name("no-such.tar"), &out.with_file_name("t"))
        .expect_err("no archive");
    assert!(matches!(e, MetaError::Io { .. }), "{e}");
    assert!(
        !out.with_file_name("t").exists(),
        "a target made for no archive"
    );
}

/// Catches: a backup of a name the manifest cannot carry (not UTF-8, or with a newline:
/// one line per file), and a file that changed size while it was read.
#[test]
fn write_refuses_names_the_manifest_cannot_carry_and_a_changed_file() {
    use std::os::unix::ffi::OsStrExt;
    for (i, name) in [b"bad\xff".as_slice(), b"two\nlines"]
        .into_iter()
        .enumerate()
    {
        let (meta, out) = meta_dir(&format!("bad_name_{i}"));
        fs::write(meta.join(std::ffi::OsStr::from_bytes(name)), b"x").expect("a file");
        let e = write(&meta, &out).expect_err("an unnamable file was archived");
        assert!(matches!(e, MetaError::NotFile { .. }), "{e}");
        assert!(!out.exists());
    }
    let p = Path::new("/meta/hardstate");
    exact(p, 3, 3).expect("same length");
    let e = exact(p, 3, 2).expect_err("a short read");
    assert!(
        matches!(
            e,
            MetaError::Changed {
                want: 3,
                got: 2,
                ..
            }
        ),
        "{e}"
    );
}
