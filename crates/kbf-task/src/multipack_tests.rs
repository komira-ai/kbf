//! The run path with several packs and large objects: what is PUT, where, and in what order.

use super::*;
use crate::outputs::collect_outputs;
use crate::pack::MAX_LARGE_BYTES;
use crate::task::recording_sink::Put;
use crate::task::recording_sink::flip_first_byte;
use kbf_segments::crc32c;
use kbf_segments::layout::footer_len;
use kbf_task_pack::{LargeEntry, MAX_RECORD_BYTES, read_manifest_with, write_pack};

/// Declares two outputs of the input tree, a file and a directory: three to five distinct
/// non-empty blobs (the files, the Directory messages and the Tree).
const TWO_OUTPUTS: &[&str] = &["true"];
const TWO_PATHS: &[&str] = &["data", "src"];

/// Writes 65 MiB of zeros, one byte past the record cap's 64 MiB: a blob that cannot be a record.
const BIG: &[&str] = &[
    "dd",
    "if=/dev/zero",
    "of=big",
    "bs=1048576",
    "count=65",
    "status=none",
];

/// Limits of small packs: three records a pack, so five blobs need two packs.
fn small_packs(packs: usize) -> Limits {
    Limits {
        pack_records: 3,
        packs,
        large: 0,
        ..Limits::real()
    }
}

/// The digests of the records of `pack`, in offset order.
fn records_of(pack: &[u8]) -> Vec<kbf_types::Digest> {
    let reader = SegmentReader::open(pack).expect("segment");
    let mut entries: Vec<_> = reader.footer().entries().to_vec();
    entries.sort_by_key(|e| e.offset);
    entries.iter().map(|e| e.digest).collect()
}

/// How many distinct non-empty blobs the action leaves, counted from the one pack of a run with
/// room for them all. The test inputs make 3 to 5, which `small_packs` splits into two packs.
async fn blob_count(mem: &Mem, action: &Digest, name: &str) -> usize {
    let dir = scratch(name);
    let sink = Fake::new(false);
    execute(&cli(&dir, action.clone()), mem, &sink)
        .await
        .expect("task");
    let count = records_of(&bodies(&sink)[0]).len() - 1;
    assert!((3..=5).contains(&count), "{count} blobs");
    count
}

/// The bytes of the stored bodies, by PUT order.
fn bodies(sink: &Fake) -> Vec<Vec<u8>> {
    sink.stored.lock().unwrap().clone()
}

/// Catches a small run changing shape: it is exactly one PUT, to pack slot 0, whose manifest is
/// a version 1 manifest (the v1 reader, which refuses version 2, reads it) and whose bytes are
/// what the v1 writer makes of the same manifest and records in digest order, so the packs of a
/// small run are byte for byte what they were before several packs existed.
#[tokio::test]
async fn a_small_run_is_one_put_to_pack_0_identical_to_the_v1_pack() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, COPY, &["out"]);
    let dir = scratch("multi-small");
    let sink = Fake::new(false);
    let manifest = execute(&cli(&dir, digest), &mem, &sink)
        .await
        .expect("task");
    let accepted = sink.accepted();
    assert_eq!(accepted.len(), 1);
    let body = bodies(&sink).remove(0);
    assert_eq!(
        accepted[0],
        Put::Pack {
            slot: 0,
            len: body.len()
        }
    );
    assert_eq!(manifest.packs, 1);
    assert!(manifest.large.is_empty());
    assert_eq!(manifest.version(), 1);
    assert_eq!(read_manifest(&body).expect("v1 reader"), manifest);

    let reader = SegmentReader::open(&body).expect("segment");
    let mut records = records_of(&body);
    let json_digest = sha256(&manifest.to_json().expect("json"));
    records.retain(|d| *d != json_digest);
    records.sort();
    let blobs: Vec<&[u8]> = records
        .iter()
        .map(|d| reader.get(d).expect("get").expect("record"))
        .collect();
    assert_eq!(write_pack(&manifest, &blobs).expect("v1 pack"), body);
}

/// Catches several packs being PUT in the wrong order, to the wrong slots, with pack 0 not
/// last, a manifest that does not count the packs, or a blob lost or doubled between the packs:
/// blobs at three records a pack (two beside the manifest in pack 0) need two packs, so the PUTs are slot 1 then slot 0, pack 0's
/// manifest says `packs = 2` and reads back through `read_manifest_with`, pack 1 has no manifest,
/// and the records of the two packs are the blobs, each once.
#[tokio::test]
async fn outputs_that_need_two_packs_are_put_to_slot_1_then_slot_0() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, TWO_OUTPUTS, TWO_PATHS);
    let blobs = blob_count(&mem, &digest, "multi-two-count").await;
    let dir = scratch("multi-two");
    let sink = Fake::with_slots(2, 0);
    let manifest = execute_with(&cli(&dir, digest), &mem, &sink, &small_packs(2))
        .await
        .expect("task");
    let accepted = sink.accepted();
    let bodies = bodies(&sink);
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        accepted,
        [
            Put::Pack {
                slot: 1,
                len: bodies[0].len()
            },
            Put::Pack {
                slot: 0,
                len: bodies[1].len()
            }
        ]
    );
    assert_eq!(manifest.packs, 2);
    assert!(manifest.large.is_empty());
    assert_eq!(manifest.version(), 2);
    assert_eq!(
        read_manifest_with(&bodies[1], MAX_LARGE_BYTES).expect("pack 0"),
        manifest
    );
    // The manifest is written as version 2.
    let json: serde_json::Value =
        serde_json::from_slice(&manifest.to_json_with(MAX_LARGE_BYTES).expect("json"))
            .expect("json");
    assert_eq!(json["version"], 2);
    assert_eq!(json["packs"], 2);

    let pack_1 = records_of(&bodies[0]);
    let pack_0 = records_of(&bodies[1]);
    assert_eq!(pack_1.len(), 3);
    assert_eq!(pack_0.len(), 1 + blobs - 3);
    let manifest_digest = sha256(&manifest.to_json_with(MAX_LARGE_BYTES).expect("json"));
    assert_eq!(pack_0[0], manifest_digest);
    assert!(!pack_1.contains(&manifest_digest));
    let mut all: Vec<_> = pack_1.iter().chain(&pack_0[1..]).copied().collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), blobs);
}

/// Catches the same outputs being placed in a different plan from one run to the next: two runs
/// of one action PUT the same bytes to the same slots.
#[tokio::test]
async fn the_same_outputs_give_the_same_packs() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, TWO_OUTPUTS, TWO_PATHS);
    let mut runs = Vec::new();
    for name in ["multi-same-a", "multi-same-b"] {
        let dir = scratch(name);
        let sink = Fake::with_slots(2, 0);
        execute_with(&cli(&dir, digest.clone()), &mem, &sink, &small_packs(2))
            .await
            .expect("task");
        let stored = bodies(&sink);
        // Records only: the manifest holds the run's timings.
        runs.push((records_of(&stored[0]), records_of(&stored[1])[1..].to_vec()));
    }
    assert_eq!(runs[0], runs[1]);
}

/// Catches outputs that need more packs than the key set has being stored: nothing is PUT, the
/// failure is a pack failure (exit 12) with the detail `output-too-large`, and a set of exactly
/// enough packs works.
#[tokio::test]
async fn outputs_that_need_more_packs_than_slots_fail_as_output_too_large() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, TWO_OUTPUTS, TWO_PATHS);
    let dir = scratch("multi-few-packs");
    let sink = Fake::with_slots(1, 0);
    let failure = execute_with(&cli(&dir, digest), &mem, &sink, &small_packs(1))
        .await
        .expect_err("two packs in one slot");
    assert!(matches!(failure, Failure::Pack(_)), "{failure}");
    assert_eq!(failure.exit_code(), EXIT_PACK);
    assert!(
        failure.to_string().contains("output-too-large"),
        "{failure}"
    );
    assert!(sink.accepted().is_empty());
    assert!(sink.tried().is_empty());
}

/// Catches a blob over the record cap being put in a pack (a daemon never reads one from a
/// pack), or put after the packs, or not named in the manifest: it is PUT first, to large slot
/// 0, as the 65 MiB it is, and then pack 0, whose manifest lists it in `large` with the digest of
/// the bytes that were streamed. The blob is a file read in place, never loaded: the sink reads
/// it through the same reader as the real PUT.
#[tokio::test]
async fn an_output_over_the_record_cap_is_put_first_to_large_slot_0() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, BIG, &["big"]);
    let dir = scratch("multi-large");
    let sink = Fake::with_slots(2, 1);
    let manifest = execute(&cli(&dir, digest), &mem, &sink)
        .await
        .expect("task");
    let big = manifest.output_files[0].digest;
    assert_eq!(big.size_bytes, 65 << 20);
    assert!(big.size_bytes > MAX_RECORD_BYTES);
    assert_eq!(
        manifest.large,
        [LargeEntry {
            slot: 0,
            digest: big
        }]
    );
    assert_eq!(manifest.packs, 1);
    let bodies = bodies(&sink);
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        sink.accepted(),
        [
            Put::Large {
                slot: 0,
                len: 65 << 20,
                sha256: hex::encode(big.hash)
            },
            Put::Pack {
                slot: 0,
                len: bodies[0].len()
            }
        ]
    );
    assert_eq!(
        read_manifest_with(&bodies[0], MAX_LARGE_BYTES).expect("pack 0"),
        manifest
    );
    // The big blob is not a record of the pack.
    assert!(!records_of(&bodies[0]).contains(&big));
}

/// Catches a large blob being stored when the key set has no large slot: the failure is
/// `output-too-large` (exit 12) and nothing is PUT, not even a pack.
#[tokio::test]
async fn a_large_output_without_a_large_slot_fails_as_output_too_large() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, BIG, &["big"]);
    let dir = scratch("multi-no-large");
    let sink = Fake::with_slots(2, 0);
    let failure = execute(&cli(&dir, digest), &mem, &sink)
        .await
        .expect_err("no large slot");
    assert_eq!(failure.exit_code(), EXIT_PACK);
    assert!(
        failure.to_string().contains("output-too-large"),
        "{failure}"
    );
    assert!(sink.tried().is_empty());
}

/// Catches a pack being PUT after a large PUT failed: the key exists (412), the failure is a put
/// (exit 13), and the only attempt was the large object.
#[tokio::test]
async fn a_failed_large_put_fails_the_task_and_no_pack_follows() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, BIG, &["big"]);
    let dir = scratch("multi-large-fails");
    let sink = Fake::with_slots(2, 1).refusing_large();
    let failure = execute(&cli(&dir, digest), &mem, &sink)
        .await
        .expect_err("refused");
    assert!(matches!(failure, Failure::Put(_)), "{failure}");
    assert_eq!(failure.exit_code(), EXIT_PUT);
    assert_eq!(sink.tried(), [(true, 0)]);
    assert!(sink.accepted().is_empty());
}

/// Catches pack 0 being PUT after another pack failed: with every pack PUT refused, the first
/// attempt (pack 1) is the only one, so pack 0, the commit point, is never written.
#[tokio::test]
async fn a_failed_pack_put_stops_before_pack_0() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, TWO_OUTPUTS, TWO_PATHS);
    let dir = scratch("multi-pack-fails");
    let sink = Fake::with_slots(2, 0).refusing_packs();
    let failure = execute_with(&cli(&dir, digest), &mem, &sink, &small_packs(2))
        .await
        .expect_err("refused");
    assert_eq!(failure.exit_code(), EXIT_PUT);
    assert_eq!(sink.tried(), [(false, 1)]);
}

/// An action over an input root of `count` files `f0`.. with distinct contents, running `argv`.
fn files_action(mem: &mut Mem, count: usize, argv: &[&str], outputs: &[&str]) -> Digest {
    let files = (0..count)
        .map(|i| FileNode {
            name: format!("f{i}"),
            digest: Some(mem.add(format!("file {i}\n").into_bytes())),
            is_executable: false,
            ..Default::default()
        })
        .collect();
    let root = mem.add(
        Directory {
            files,
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let command = mem.add(
        Command {
            arguments: argv.iter().map(|a| (*a).to_owned()).collect(),
            environment_variables: vec![EnvironmentVariable {
                name: "PATH".to_owned(),
                value: "/usr/bin:/bin".to_owned(),
            }],
            output_paths: outputs.iter().map(|o| (*o).to_owned()).collect(),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    mem.add(
        Action {
            command_digest: Some(command),
            input_root_digest: Some(root),
            ..Default::default()
        }
        .encode_to_vec(),
    )
}

/// Catches the packs being PUT in ascending order, or the large object not first, or pack 0 not
/// last, when there are three packs (an ascending loop is invisible with two). The input has four
/// files of distinct contents and the command writes a 65 MiB file, so the outputs are exactly
/// four small blobs and one large one; at two records a pack (one beside the manifest in pack 0)
/// the small blobs need packs 1 and 2 and leave pack 0 with the manifest alone. The PUTs are large
/// 0, pack 2, pack 1, pack 0, and pack 0's manifest says `packs = 3` and lists the large object.
#[tokio::test]
async fn three_packs_and_a_large_object_are_put_in_commit_order() {
    let mut mem = Mem::default();
    let digest = files_action(&mut mem, 4, BIG, &["big", "f0", "f1", "f2", "f3"]);
    let dir = scratch("multi-three");
    let sink = Fake::with_slots(3, 1);
    let limits = Limits {
        pack_records: 2,
        packs: 3,
        large: 1,
        ..Limits::real()
    };
    let manifest = execute_with(&cli(&dir, digest), &mem, &sink, &limits)
        .await
        .expect("task");
    assert_eq!(manifest.output_files.len(), 5);
    assert_eq!(manifest.stdout, None);
    assert_eq!(manifest.stderr, None);
    assert_eq!(manifest.packs, 3);
    assert_eq!(manifest.large.len(), 1);
    assert_eq!(
        sink.tried(),
        [(true, 0), (false, 2), (false, 1), (false, 0)]
    );
    let bodies = bodies(&sink);
    assert_eq!(records_of(&bodies[0]).len(), 2);
    assert_eq!(records_of(&bodies[1]).len(), 2);
    assert_eq!(records_of(&bodies[2]).len(), 1);
    assert_eq!(
        read_manifest_with(&bodies[2], MAX_LARGE_BYTES).expect("pack 0"),
        manifest
    );
}

/// The pack of the hexadecimal text of the fixture `name`, a path from this crate's directory.
fn fixture_pack(name: &str) -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name);
    let text = std::fs::read_to_string(path).expect("fixture");
    hex::decode(text.split_whitespace().collect::<String>()).expect("hex")
}

/// Catches any change to the bytes of a v1 run's pack: the manifest of the shared v1 fixture and
/// its records go through the run's packing (`store_outputs`, the function `execute` ends with),
/// and the pack PUT must equal `tests/fixtures/v1_digest_order.pack.hex` byte for byte, manifest,
/// records, index and trailer. The fixture is the shared fixture's manifest followed by the same
/// six records in ascending digest order (the order the plan gives), with the index laid out as
/// `kbf-segments` documents it (fan-out, entries sorted by digest, trailer with the CRC-32C of
/// the index), and `the_frozen_pack_is_the_documented_layout` checks that the fixture is exactly
/// that layout, built here without `write_pack` or `SegmentWriter`.
#[test]
fn a_v1_run_is_frozen_byte_for_byte() {
    let shared = fixture_pack("../kbf-task-pack/tests/fixtures/task_pack/task_pack_v1.pack.hex");
    let mut manifest = read_manifest(&shared).expect("manifest");
    let reader = SegmentReader::open(&shared).expect("segment");
    let mut entries = reader.footer().entries().to_vec();
    entries.sort_by_key(|e| e.offset);
    let records: Vec<&[u8]> = entries[1..]
        .iter()
        .map(|e| reader.get(&e.digest).expect("get").expect("record"))
        .collect();
    assert_eq!(records.len(), 6);
    let sink = Fake::new(false);
    let limits = Limits {
        packs: 1,
        large: 0,
        ..Limits::real()
    };
    store_outputs(&mut manifest, &Blobs::from_bytes(&records), &limits, &sink).expect("stored");
    let frozen = fixture_pack("tests/fixtures/v1_digest_order.pack.hex");
    assert_eq!(bodies(&sink), [frozen]);
}

/// Catches an extra pack that cannot be built being found only after other packs were PUT: with
/// the plan allowed records of 200 MiB, the 65 MiB output is planned into pack 1 and the small
/// output into pack 2; the pack of the big record is refused (a record is at most 64 MiB), and the
/// task fails as `output-too-large` (exit 12) with no PUT tried, though pack 2, which builds, would
/// have been PUT first.
#[tokio::test]
async fn an_extra_pack_that_cannot_be_built_fails_before_any_put() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, BIG, &["big", "data"]);
    let dir = scratch("multi-unbuildable");
    let sink = Fake::with_slots(3, 0);
    let limits = Limits {
        pack_records: 1,
        record_bytes: 200 << 20,
        packs: 3,
        large: 0,
        ..Limits::real()
    };
    let failure = execute_with(&cli(&dir, digest), &mem, &sink, &limits)
        .await
        .expect_err("unbuildable");
    assert_eq!(failure.exit_code(), EXIT_PACK);
    assert!(
        failure.to_string().contains("output-too-large"),
        "{failure}"
    );
    assert!(sink.tried().is_empty());
}

/// Catches a v1 run that fills its pack to the byte being refused because the manifest's room was
/// over-reserved: a run of one pack and no large object reserves exactly its manifest, so a pack
/// limit of exactly the records, the manifest and the footer places everything in pack 0 and one
/// byte less is `output-too-large`; with more packs or a large object, the slack is added.
#[tokio::test]
async fn a_v1_run_that_exactly_fills_the_pack_is_accepted() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, &["true"], &["data"]);
    let dir = scratch("multi-exact");
    let manifest = execute(&cli(&dir, digest), &mem, &Fake::new(false))
        .await
        .expect("task");
    let one_pack = Limits {
        packs: 1,
        large: 0,
        ..Limits::real()
    };
    let json = manifest.to_json().expect("json").len() as u64;
    assert_eq!(
        manifest_reserve(&manifest, &one_pack).expect("reserve"),
        json
    );
    let more = |packs, large| Limits {
        packs,
        large,
        ..Limits::real()
    };
    for limits in [more(2, 0), more(1, 1)] {
        assert_eq!(
            manifest_reserve(&manifest, &limits).expect("reserve"),
            json + MANIFEST_SLACK
        );
    }
    // One 3-byte record and the manifest: two entries.
    let blob = [(sha256(b"hi\n"), 3)];
    let exact = json + 3 + footer_len(2);
    let fits = |pack_bytes| {
        plan(
            &blob,
            manifest_reserve(&manifest, &one_pack).expect("reserve"),
            &Limits {
                pack_bytes,
                ..one_pack
            },
        )
    };
    assert_eq!(fits(exact).expect("exactly full").packs, [vec![blob[0].0]]);
    let text = fits(exact - 1).unwrap_err().to_string();
    assert!(text.contains("output-too-large"), "{text}");
}

/// Files `f0`.. of 1,000 bytes over the hashing chunk and distinct contents, recorded as outputs
/// of a run, with the manifest that names them.
async fn recorded_files(name: &str, count: usize) -> (TestDir, TaskManifest, Blobs) {
    let dir = scratch(name);
    for i in 0..count {
        let bytes: Vec<u8> = (0..kbf_outputs::CHUNK_BYTES + 1000)
            .map(|n| ((n + i) % 251) as u8)
            .collect();
        std::fs::write(dir.join(format!("f{i}")), bytes).expect("file");
    }
    std::fs::write(dir.join("out"), b"").expect("out");
    std::fs::write(dir.join("err"), b"").expect("err");
    let paths: Vec<String> = (0..count).map(|i| format!("f{i}")).collect();
    let outputs = collect_outputs(
        &dir,
        "",
        &paths,
        &dir.join("out"),
        &dir.join("err"),
        OutputLimits::default(),
        u64::MAX,
    )
    .await
    .expect("collected");
    let manifest = TaskManifest {
        lease: "lease-1".to_owned(),
        action: sha256(b"a"),
        exit_code: 0,
        stdout: None,
        stderr: None,
        output_files: outputs
            .result
            .output_files
            .iter()
            .map(|f| kbf_task_pack::OutputFile {
                path: f.path.clone(),
                digest: digest_of(f.digest.as_ref()).expect("digest"),
                is_executable: false,
            })
            .collect(),
        output_directories: Vec::new(),
        output_symlinks: Vec::new(),
        timing: kbf_task_pack::Timing::default(),
        usage: kbf_task_pack::Usage::default(),
        packs: 1,
        large: Vec::new(),
        inputs: None,
    };
    (dir, manifest, outputs.blobs)
}

/// Limits for one pack, and for three packs of two records.
fn pack_shapes() -> [Limits; 2] {
    [
        Limits {
            packs: 1,
            large: 0,
            ..Limits::real()
        },
        Limits {
            pack_records: 2,
            packs: 3,
            large: 0,
            ..Limits::real()
        },
    ]
}

/// Catches a file changed in place after it was hashed (the same size, so nothing else notices)
/// being packed under its old digest and stored: whether the file lands in pack 0 or in another
/// pack, the task fails as `output-changed` (exit 12) and no PUT is tried.
#[tokio::test]
async fn a_file_changed_after_hashing_fails_before_any_put() {
    for limits in pack_shapes() {
        let (dir, mut manifest, blobs) = recorded_files("multi-changed", 4).await;
        flip_first_byte(&dir.join("f0"));
        let sink = Fake::with_slots(3, 0);
        let failure = store_outputs(&mut manifest, &blobs, &limits, &sink).unwrap_err();
        assert_eq!(failure.exit_code(), EXIT_PACK);
        assert!(failure.to_string().contains("output-changed"), "{failure}");
        assert!(sink.tried().is_empty());
    }
}

/// Catches a file that shrank after it was hashed failing in the middle of the PUTs: it cannot be
/// read in full, which is found while packs are checked, exit 12 with no PUT tried.
#[tokio::test]
async fn a_file_that_shrank_after_hashing_fails_before_any_put() {
    for limits in pack_shapes() {
        let (dir, mut manifest, blobs) = recorded_files("multi-shrunk", 4).await;
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("f0"))
            .expect("open")
            .set_len(10)
            .expect("truncate");
        let sink = Fake::with_slots(3, 0);
        let failure = store_outputs(&mut manifest, &blobs, &limits, &sink).unwrap_err();
        assert_eq!(failure.exit_code(), EXIT_PACK);
        assert!(sink.tried().is_empty());
    }
}

/// Catches a change between the check of a pack and its PUT going unseen: every file changes
/// while pack 2 is being PUT, pack 1 is built again from them, and the task fails as
/// `output-changed` (exit 12) before pack 1 is PUT; pack 0 is never PUT.
#[tokio::test]
async fn a_file_changed_before_its_pack_is_put_fails_before_that_put() {
    let (dir, mut manifest, blobs) = recorded_files("multi-changed-late", 4).await;
    let paths: Vec<_> = (0..4).map(|i| dir.join(format!("f{i}"))).collect();
    let sink = Fake::with_slots(3, 0).with_hook(move |_, _| {
        for path in &paths {
            flip_first_byte(path);
        }
    });
    let limits = &pack_shapes()[1];
    let failure = store_outputs(&mut manifest, &blobs, limits, &sink).unwrap_err();
    assert_eq!(failure.exit_code(), EXIT_PACK);
    assert!(failure.to_string().contains("output-changed"), "{failure}");
    assert_eq!(sink.tried(), [(false, 2)]);
}

/// Catches a large object whose file changed while it was PUT being counted as stored: the bytes
/// streamed do not hash to the blob, the PUT fails (exit 13; the object stored is an orphan for
/// the janitor), and pack 0 is never PUT.
#[tokio::test]
async fn a_large_file_changed_during_its_put_fails_the_task_and_pack_0_is_never_put() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, BIG, &["big"]);
    let dir = scratch("multi-large-changed");
    let big = dir.join("in/big");
    let sink = Fake::with_slots(2, 1).with_hook(move |_, _| flip_first_byte(&big));
    let failure = execute(&cli(&dir, digest), &mem, &sink)
        .await
        .expect_err("changed");
    assert_eq!(failure.exit_code(), EXIT_PUT);
    assert!(failure.to_string().contains("changed"), "{failure}");
    assert_eq!(sink.tried(), [(true, 0)]);
}

/// A segment built straight from the `kbf-segments` layout: the records back to back from offset
/// 0, then the fan-out (cumulative counts by first hash byte), the entries sorted by digest (hash,
/// size, offset, CRC-32C of the record), the trailer (length of the records, entry count, the
/// SHA-256 code, CRC-32C of the index so far) and the version and magic.
fn layout_pack(records: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut entries = Vec::new();
    for record in records {
        let hash = sha256(record).hash;
        entries.push((hash, record.len() as u64, body.len() as u64, crc32c(record)));
        body.extend_from_slice(record);
    }
    entries.sort();
    let mut footer = Vec::new();
    let mut cumulative = 0u32;
    for first in 0..=255u8 {
        cumulative += entries.iter().filter(|e| e.0[0] == first).count() as u32;
        footer.extend_from_slice(&cumulative.to_le_bytes());
    }
    for (hash, size, offset, crc) in &entries {
        footer.extend_from_slice(hash);
        footer.extend_from_slice(&size.to_le_bytes());
        footer.extend_from_slice(&offset.to_le_bytes());
        footer.extend_from_slice(&crc.to_le_bytes());
    }
    footer.extend_from_slice(&(body.len() as u64).to_le_bytes());
    footer.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    footer.extend_from_slice(&[1, 0, 0, 0]);
    let crc = crc32c(&footer);
    footer.extend_from_slice(&crc.to_le_bytes());
    footer.extend_from_slice(&1u32.to_le_bytes());
    footer.extend_from_slice(b"KBFSEG\r\n");
    body.extend(footer);
    body
}

/// Catches the committed v1 fixture drifting from the segment layout, or being edited by hand:
/// the shared fixture's manifest followed by its six records in ascending digest order, laid out
/// by `layout_pack` above, is the fixture's bytes. If the layout legitimately changes, replace
/// the fixture with `layout_pack`'s output for these records.
#[test]
fn the_frozen_pack_is_the_documented_layout() {
    let shared = fixture_pack("../kbf-task-pack/tests/fixtures/task_pack/task_pack_v1.pack.hex");
    let json_len = read_manifest(&shared)
        .expect("manifest")
        .to_json()
        .expect("json")
        .len();
    let reader = SegmentReader::open(&shared).expect("segment");
    let mut records: Vec<&[u8]> = reader
        .footer()
        .entries()
        .iter()
        .filter(|e| e.offset != 0)
        .map(|e| reader.get(&e.digest).expect("get").expect("record"))
        .collect();
    records.sort_by_key(|r| sha256(r));
    let mut all = vec![&shared[..json_len]];
    all.extend(records);
    assert_eq!(
        layout_pack(&all),
        fixture_pack("tests/fixtures/v1_digest_order.pack.hex")
    );
}

/// Catches a sink reporting more slots than a manifest can name wrapping a count or starting the
/// run: 9 packs, 9 large and 300 packs are each a setup failure (exit 14) before anything is
/// fetched or tried.
#[tokio::test]
async fn a_sink_with_more_slots_than_a_manifest_names_is_a_setup_failure() {
    let mem = Mem::default();
    for (packs, large) in [(9, 0), (1, 9), (300, 300)] {
        let dir = scratch("multi-many-slots");
        let sink = Fake::with_slots(packs, large);
        let failure = execute(&cli(&dir, reapi_of(b"x")), &mem, &sink)
            .await
            .expect_err("too many slots");
        assert_eq!(failure.exit_code(), EXIT_SETUP);
        assert!(failure.to_string().contains("at most 8"), "{failure}");
        assert!(sink.tried().is_empty());
        assert!(!dir.join("in").exists());
    }
}
