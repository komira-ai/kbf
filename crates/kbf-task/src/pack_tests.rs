use std::path::PathBuf;

use kbf_segments::{SegmentReader, WriteError, sha256};
use kbf_task_pack::{
    EMPTY_DIGEST, MAX_PACK_RECORDS, MAX_PATH_BYTES, MAX_RECORD_BYTES, OutputDirectory, OutputFile,
    OutputSymlink, Timing, Usage, check_shape, read_manifest,
};
use kbf_types::{Digest, DigestFunction};

use super::*;

const OUT: &[u8] = b"out";
const FILE: &[u8] = b"file";
const TREE: &[u8] = b"tree";

/// A manifest that names every kind of output, over the blobs `OUT`, `FILE` and `TREE`.
fn manifest() -> TaskManifest {
    TaskManifest {
        lease: "lease-1".to_owned(),
        action: sha256(b"action"),
        exit_code: -3,
        stdout: Some(sha256(OUT)),
        stderr: None,
        output_files: vec![OutputFile {
            path: "bin/a".to_owned(),
            digest: sha256(FILE),
            is_executable: true,
        }],
        output_directories: vec![OutputDirectory {
            path: "d".to_owned(),
            tree: sha256(TREE),
        }],
        output_symlinks: vec![OutputSymlink {
            path: "l".to_owned(),
            target: "bin/a".to_owned(),
        }],
        timing: Timing {
            fetch_ms: 1,
            run_ms: 2,
            pack_ms: 3,
        },
        usage: Usage {
            cpu_usage_us: 5,
            memory_peak_bytes: 6,
        },
        packs: 1,
        large: Vec::new(),
        inputs: None,
    }
}

/// A change that makes the manifest one ingest refuses.
type Change = Box<dyn Fn(&mut TaskManifest)>;

fn blobs() -> Vec<&'static [u8]> {
    vec![OUT, FILE, TREE]
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../kbf-task-pack/tests/fixtures/task_pack")
}

/// The pack of the hexadecimal text of `task_pack_v1.pack.hex` (64 digits a line).
fn golden_pack() -> Vec<u8> {
    let text = std::fs::read_to_string(fixtures().join("task_pack_v1.pack.hex")).expect("fixture");
    let digits: String = text.split_whitespace().collect();
    hex::decode(digits).expect("hex")
}

/// The records of `pack` after the manifest, in offset order.
fn blobs_of(pack: &[u8]) -> Vec<Vec<u8>> {
    let reader = SegmentReader::open(pack).expect("segment");
    let mut entries: Vec<_> = reader.footer().entries().to_vec();
    entries.sort_by_key(|e| e.offset);
    entries
        .iter()
        .skip(1)
        .map(|e| {
            let range = e.range();
            pack[range.start as usize..range.end as usize].to_vec()
        })
        .collect()
}

fn refused(change: impl FnOnce(&mut TaskManifest)) -> PackError {
    let mut m = manifest();
    change(&mut m);
    build(&m, &blobs()).expect_err("a manifest ingest refuses")
}

/// Catches a pack whose first record is not the manifest, or that ingest's own reader cannot
/// read back: the manifest is the record at offset 0 and the footer passes `check_shape`.
#[test]
fn the_pack_starts_with_the_manifest_and_passes_the_shape_check() {
    let m = manifest();
    let pack = build(&m, &blobs()).expect("pack");
    assert_eq!(read_manifest(&pack).expect("read"), m);
    let json = m.to_json().expect("json");
    assert_eq!(&pack[..json.len()], json.as_slice());
    let reader = SegmentReader::open(&pack).expect("segment");
    let entry = check_shape(reader.footer(), pack.len() as u64).expect("shape");
    assert_eq!(entry.offset, 0);
    assert_eq!(reader.footer().entries().len(), 4);
}

/// Catches an empty blob becoming a record (ingest refuses a zero-size record), wherever it
/// arrives in the blob list.
#[test]
fn an_empty_blob_gets_no_record() {
    let mut with_empty = blobs();
    with_empty.insert(0, b"");
    with_empty.push(b"");
    let pack = build(&manifest(), &with_empty).expect("pack");
    let reader = SegmentReader::open(&pack).expect("segment");
    assert_eq!(reader.footer().entries().len(), 4);
    check_shape(reader.footer(), pack.len() as u64).expect("shape");
}

/// Catches a manifest the tool writes that its own reader refuses: each valid fixture of the
/// shared types is built into a pack and read back unchanged.
#[test]
fn every_valid_fixture_manifest_builds_and_reads_back() {
    let mut stems = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(fixtures().join("valid"))
        .expect("fixtures")
        .map(|e| e.expect("entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let m = TaskManifest::parse(&std::fs::read(&path).expect("read")).expect("valid");
        let pack = build(&m, &[]).expect("pack");
        assert_eq!(read_manifest(&pack).expect("read"), m);
        stems.push(
            path.file_stem()
                .expect("stem")
                .to_string_lossy()
                .into_owned(),
        );
    }
    assert_eq!(
        stems,
        [
            "dotted_names",
            "empty_output_file",
            "failed_action",
            "minimal"
        ]
    );
}

/// Catches drift from the bytes ingest was specified against: the golden pack is rebuilt from its
/// own manifest and blobs and must come out byte for byte.
#[test]
fn the_golden_pack_is_rebuilt_byte_for_byte() {
    let golden = golden_pack();
    let m = read_manifest(&golden).expect("golden");
    let blobs = blobs_of(&golden);
    let views: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    assert_eq!(build(&m, &views).expect("pack"), golden);
}

/// Catches a manifest ingest would refuse reaching the store: each way a path, a symlink target,
/// a number, a digest or the lease is refused by the shared rules is refused as `invalid-manifest`.
#[test]
fn a_manifest_ingest_would_refuse_is_an_invalid_manifest() {
    let long = "a".repeat(MAX_PATH_BYTES + 1);
    let past = u64::try_from(i64::MAX).expect("positive") + 1;
    let cases: Vec<(&str, Change)> = vec![
        ("empty lease", Box::new(|m| m.lease = String::new())),
        (
            "lease with a slash",
            Box::new(|m| m.lease = "a/b".to_owned()),
        ),
        (
            "lease of 129 bytes",
            Box::new(|m| m.lease = "a".repeat(129)),
        ),
        (
            "empty path",
            Box::new(|m| m.output_files[0].path = String::new()),
        ),
        (
            "nul in path",
            Box::new(|m| m.output_files[0].path = "a\0b".to_owned()),
        ),
        (
            "leading slash",
            Box::new(|m| m.output_directories[0].path = "/d".to_owned()),
        ),
        (
            "dotdot component",
            Box::new(|m| m.output_files[0].path = "a/../b".to_owned()),
        ),
        (
            "path too long",
            Box::new(|m| m.output_symlinks[0].path = "a".repeat(MAX_PATH_BYTES + 1)),
        ),
        (
            "duplicate path",
            Box::new(|m| m.output_symlinks[0].path = "bin/a".to_owned()),
        ),
        (
            "empty target",
            Box::new(|m| m.output_symlinks[0].target = String::new()),
        ),
        (
            "nul in target",
            Box::new(|m| m.output_symlinks[0].target = "a\0".to_owned()),
        ),
        (
            "target too long",
            Box::new(move |m| m.output_symlinks[0].target = long.clone()),
        ),
        ("timing past i64", Box::new(move |m| m.timing.run_ms = past)),
        (
            "usage past i64",
            Box::new(move |m| m.usage.memory_peak_bytes = past),
        ),
        (
            "zero size not empty",
            Box::new(|m| m.stdout = Some(Digest::new(DigestFunction::Sha256, [7; 32], 0))),
        ),
        (
            "empty stdout listed",
            Box::new(|m| m.stdout = Some(EMPTY_DIGEST)),
        ),
    ];
    for (name, change) in cases {
        let err = refused(change);
        assert_eq!(err.detail, INVALID_MANIFEST, "{name}");
        assert!(
            err.to_string().starts_with("invalid-manifest: "),
            "{name}: {err}"
        );
    }
}

/// Catches the 16,384-record cap being skipped: one more distinct blob than the cap allows
/// (the manifest is a record too) is refused as `output-too-large`.
#[test]
fn more_records_than_the_cap_is_output_too_large() {
    let held: Vec<[u8; 4]> = (0..u32::try_from(MAX_PACK_RECORDS).expect("fits"))
        .map(u32::to_be_bytes)
        .collect();
    let views: Vec<&[u8]> = held.iter().map(<[u8; 4]>::as_slice).collect();
    let err = build(&manifest(), &views).expect_err("one record too many");
    assert_eq!(err.detail, OUTPUT_TOO_LARGE);
    assert!(err.to_string().contains("output-too-large"), "{err}");
    // One fewer blob is within the cap.
    build(&manifest(), &views[1..]).expect("at the cap");
}

/// Catches a blob over the record cap being packed.
#[test]
fn a_blob_over_the_record_cap_is_output_too_large() {
    let big = vec![0u8; usize::try_from(MAX_RECORD_BYTES).expect("fits") + 1];
    let err = build(&manifest(), &[&big]).expect_err("record too large");
    assert_eq!(err.detail, OUTPUT_TOO_LARGE);
}

/// Catches a manifest over the manifest cap being packed: 1,100 paths of 4,000 bytes make more
/// than 4 MiB of JSON.
#[test]
fn a_manifest_over_the_manifest_cap_is_output_too_large() {
    let mut m = manifest();
    m.output_files = (0..1100)
        .map(|i| OutputFile {
            path: format!("{i:04}{}", "a".repeat(4000)),
            digest: sha256(FILE),
            is_executable: false,
        })
        .collect();
    m.output_directories.clear();
    m.output_symlinks.clear();
    let err = build(&m, &blobs()).expect_err("manifest too large");
    assert_eq!(err.detail, OUTPUT_TOO_LARGE);
}

/// Catches a refusal of the writer or the reader landing under the wrong detail: size caps are
/// `output-too-large`, every other refusal has its own.
#[test]
fn each_refusal_has_the_detail_of_its_class() {
    let too_large = [
        PackError::from(WritePackError::RecordTooLarge { size: 1 }),
        PackError::from(WritePackError::Segment(WriteError::Full { size: 1 })),
        PackError::from(ManifestError::TooLarge { len: 1 }),
        PackError::from(TaskPackError::Shape(ShapeError::TooLarge { len: 1 })),
        PackError::from(TaskPackError::Shape(ShapeError::TooManyRecords {
            count: 1,
        })),
        PackError::from(TaskPackError::Shape(ShapeError::RecordTooLarge {
            offset: 0,
            size: 1,
        })),
        PackError::from(TaskPackError::Shape(ShapeError::ManifestTooLarge {
            size: 1,
        })),
    ];
    for err in too_large {
        assert_eq!(err.detail, OUTPUT_TOO_LARGE, "{err}");
    }
    let segment = SegmentReader::open(b"not a segment").expect_err("junk");
    let invalid_pack = [
        PackError::from(TaskPackError::Shape(ShapeError::NoRecords)),
        PackError::from(TaskPackError::Segment(segment)),
        PackError::from(TaskPackError::Manifest(ManifestError::Version(2))),
    ];
    for err in invalid_pack {
        assert_eq!(err.detail, INVALID_PACK, "{err}");
    }
    let invalid_manifest = [
        PackError::from(WritePackError::Manifest(ManifestError::Version(2))),
        PackError::from(ManifestError::Version(2)),
    ];
    for err in invalid_manifest {
        assert_eq!(err.detail, INVALID_MANIFEST, "{err}");
    }
}
