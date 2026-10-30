//! The run path with several packs and large objects: what is PUT, where, and in what order.

use super::*;
use crate::pack::MAX_LARGE_BYTES;
use crate::task::recording_sink::Put;
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

/// Declares the file `big` and the two small outputs of the input tree (`BIG` writes the file).
const BIG_AND_TREE: &[&str] = &["big", "data", "src"];

/// The most records a pack may hold so that `blobs` one-byte blobs need exactly three packs
/// (pack 0 keeps one record for the manifest): found by asking the plan.
fn records_for_three_packs(blobs: usize) -> usize {
    let sizes: Vec<_> = (0..blobs as u32)
        .map(|n| {
            let mut hash = [0u8; 32];
            hash[..4].copy_from_slice(&n.to_be_bytes());
            (
                kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, hash, 1),
                1,
            )
        })
        .collect();
    (2..=blobs)
        .find(|&per_pack| {
            let limits = Limits {
                pack_records: per_pack,
                packs: 8,
                large: 0,
                ..Limits::real()
            };
            plan(&sizes, 0, &limits).map(|p| p.packs.len()) == Ok(3)
        })
        .expect("a record limit that gives three packs")
}

/// Catches the packs being PUT in ascending order, or the large object not first, or pack 0 not
/// last, when there are three packs (an ascending loop is invisible with two): a large object and
/// outputs that need packs 2, 1 and 0 are PUT as large 0, pack 2, pack 1, pack 0, and pack 0's
/// manifest says `packs = 3` and lists the large object.
#[tokio::test]
async fn three_packs_and_a_large_object_are_put_in_commit_order() {
    let mut mem = Mem::default();
    let small = action(&mut mem, TWO_OUTPUTS, TWO_PATHS);
    let blobs = blob_count(&mem, &small, "multi-three-count").await;
    let digest = action(&mut mem, BIG, BIG_AND_TREE);
    let dir = scratch("multi-three");
    let sink = Fake::with_slots(3, 1);
    let limits = Limits {
        pack_records: records_for_three_packs(blobs),
        packs: 3,
        large: 1,
        ..Limits::real()
    };
    let manifest = execute_with(&cli(&dir, digest), &mem, &sink, &limits)
        .await
        .expect("task");
    assert_eq!(manifest.packs, 3);
    assert_eq!(manifest.large.len(), 1);
    assert_eq!(
        sink.tried(),
        [(true, 0), (false, 2), (false, 1), (false, 0)]
    );
    let bodies = bodies(&sink);
    assert_eq!(
        read_manifest_with(&bodies[2], MAX_LARGE_BYTES).expect("pack 0"),
        manifest
    );
}

/// Catches the bytes of a small run changing: the manifest's JSON begins with the frozen text
/// of the v1 wire form (key order, no `packs` or `large`, empty lists written), the one record
/// after it is the output's bytes, and the pack is the manifest, that record and a footer of two
/// entries, ending with the segment magic. These are written out here and do not move when
/// `write_pack` does.
#[tokio::test]
async fn a_small_run_is_frozen_to_the_v1_wire_form() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, &["true"], &["data"]);
    let dir = scratch("multi-frozen");
    let sink = Fake::new(false);
    let flags = cli(&dir, digest);
    let manifest = execute(&flags, &mem, &sink).await.expect("task");
    let body = bodies(&sink).remove(0);
    let json_len = manifest.to_json().expect("json").len();
    let json = String::from_utf8(body[..json_len].to_vec()).expect("utf-8");
    let frozen = format!(
        concat!(
            r#"{{"version":1,"lease":"lease-1","action":"{}","exit_code":0,"#,
            r#""output_files":[{{"path":"data","digest":"{}","is_executable":false}}],"#,
            r#""output_directories":[],"output_symlinks":[],"timing":{{"fetch_ms":"#
        ),
        flags.action,
        sha256(b"hi\n"),
    );
    assert!(json.starts_with(&frozen), "{json}");
    assert!(json.ends_with("}}"), "{json}");
    assert_eq!(&body[json_len..json_len + 3], b"hi\n");
    // Two entries of 52 bytes, the fan-out of 1,024 and the 32-byte trailer.
    assert_eq!(body.len(), json_len + 3 + 1024 + 2 * 52 + 32);
    assert_eq!(&body[body.len() - 8..], b"KBFSEG\r\n");
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
