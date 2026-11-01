use super::*;
use kbf_segments::{SegmentWriter, WriteError, sha256};
use kbf_types::DigestFunction;

/// A digest of `size` bytes whose hash orders by `n`.
fn blob(n: u32, size: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..4].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, size)
}

/// The planner's input for `digests`: each with its own size.
fn input(digests: &[Digest]) -> Vec<(Digest, u64)> {
    digests.iter().map(|d| (*d, d.size_bytes)).collect()
}

/// Small limits: a record is at most 1000 bytes, a large object 5000, two large slots.
fn limits(pack_bytes: u64, pack_records: usize, packs: usize) -> Limits {
    Limits {
        pack_bytes,
        pack_records,
        record_bytes: 1000,
        large_bytes: 5000,
        packs,
        large: 2,
    }
}

/// `count` blobs of `size` bytes, numbered from 1.
fn blobs(count: u32, size: u64) -> Vec<Digest> {
    (1..=count).map(|n| blob(n, size)).collect()
}

/// The sizes of the packs of a plan, by slot.
fn counts(plan: &Plan) -> Vec<usize> {
    plan.packs.iter().map(Vec::len).collect()
}

/// A footer with no records is the fan-out and the trailer, and each record adds an entry.
const FOOTER: u64 = 1056;

/// Catches a small run being spread over several packs, or pack 0 missing: a few blobs make one
/// pack, slot 0, with the blobs in digest order, and no large object. This is the shape of a
/// v1 run.
#[test]
fn a_small_set_lands_in_pack_0_alone() {
    let all = blobs(3, 10);
    let plan = plan(&input(&all), 4096, &Limits::real()).expect("plans");
    assert_eq!(plan.packs, [all]);
    assert!(plan.large.is_empty());
}

/// Catches the record cap being off by one: a blob of exactly the cap stays in a pack, and one
/// byte more goes to a large slot.
#[test]
fn a_blob_of_exactly_the_record_cap_stays_in_a_pack_and_one_byte_more_goes_large() {
    let at = blob(1, MAX_RECORD_BYTES);
    let over = blob(2, MAX_RECORD_BYTES + 1);
    let plan = plan(&input(&[at, over]), 4096, &Limits::real()).expect("plans");
    assert_eq!(plan.packs, [vec![at]]);
    assert_eq!(plan.large, [over]);
}

/// Catches the record limit being ignored, counted without the manifest in pack 0, or pack 0
/// being filled before the others: with 4 records a pack and 9 blobs, packs 1 and 2 take 4
/// each, in digest order, and pack 0 takes the last blob.
#[test]
fn the_record_limit_splits_packs() {
    let all = blobs(9, 1);
    let plan = plan(&input(&all), 0, &limits(1 << 20, 4, 3)).expect("plans");
    assert_eq!(counts(&plan), [1, 4, 4]);
    assert_eq!(plan.packs[1], all[0..4].to_vec());
    assert_eq!(plan.packs[2], all[4..8].to_vec());
    assert_eq!(plan.packs[0], all[8..9].to_vec());
}

/// Catches pack 0 being allowed the full record count: with 4 records a pack, three blobs and
/// the manifest fit pack 0, and a fourth moves all four to pack 1, leaving pack 0 with the
/// manifest alone.
#[test]
fn pack_0_keeps_a_record_for_the_manifest() {
    let three = blobs(3, 1);
    let at = plan(&input(&three), 0, &limits(1 << 20, 4, 2)).expect("plans");
    assert_eq!(at.packs, [three]);
    let four = blobs(4, 1);
    let over = plan(&input(&four), 0, &limits(1 << 20, 4, 2)).expect("plans");
    assert_eq!(over.packs, [vec![], four]);
}

/// Catches the byte limit being ignored or applied without the footer: each pack is 152
/// bytes a record of 100 (the blob and its 52-byte index entry) on a footer of 1056, so a pack
/// of 1512 bytes holds exactly 3, and pack 0, which also keeps 50 bytes and an entry for the
/// manifest, holds 2.
#[test]
fn the_byte_limit_splits_packs() {
    let all = blobs(7, 100);
    let plan = plan(&input(&all), 50, &limits(FOOTER + 3 * 152, 1000, 3)).expect("plans");
    assert_eq!(counts(&plan), [1, 3, 3]);
    let two = plan_two(&all[..2]);
    assert_eq!(two.packs, [all[..2].to_vec()]);
    let three = plan_two(&all[..3]);
    assert_eq!(counts(&three), [0, 3]);
}

/// The plan of `all` with room for 2 records and manifest bytes in pack 0 and 3 records in the
/// others, at 100 bytes a blob.
fn plan_two(all: &[Digest]) -> Plan {
    plan(&input(all), 50, &limits(FOOTER + 3 * 152, 1000, 3)).expect("plans")
}

/// Catches the manifest reserve being ignored or applied as less than it is: two blobs of 100
/// bytes fit pack 0 with no reserve, and not with 50 reserved bytes, which move them to pack 1.
#[test]
fn pack_0_always_keeps_the_manifest_reserve() {
    let all = blobs(2, 100);
    let none = plan(&input(&all), 0, &limits(1450, 1000, 2)).expect("plans");
    assert_eq!(none.packs, vec![all.clone()]);
    let kept = plan(&input(&all), 50, &limits(1450, 1000, 2)).expect("plans");
    assert_eq!(kept.packs, [vec![], all]);
}

/// Catches the plan depending on the order the outputs were collected in: the same blobs in
/// five orders give one plan, and the slots read in the order 1.., then 0, are in ascending digest
/// order.
#[test]
fn the_order_is_deterministic_regardless_of_input_order() {
    let all = blobs(10, 100);
    let reference = plan(&input(&all), 50, &limits(FOOTER + 3 * 152, 1000, 4)).expect("plans");
    let mut reversed = all.clone();
    reversed.reverse();
    let mut rotated = all.clone();
    rotated.rotate_left(4);
    let mut interleaved: Vec<Digest> = all.iter().step_by(2).copied().collect();
    interleaved.extend(all.iter().skip(1).step_by(2).copied());
    let mut doubled = all.clone();
    doubled.extend(all.iter().rev().copied());
    for order in [reversed, rotated, interleaved, doubled] {
        let again = plan(&input(&order), 50, &limits(FOOTER + 3 * 152, 1000, 4)).expect("plans");
        assert_eq!(again, reference);
    }
    let mut sequence: Vec<Digest> = reference.packs[1..].iter().flatten().copied().collect();
    sequence.extend(reference.packs[0].iter().copied());
    assert_eq!(sequence, all);
}

/// Catches a blob named by several outputs being placed several times: it is once in the plan,
/// and the large blob too.
#[test]
fn duplicates_are_dropped() {
    let a = blob(1, 10);
    let big = blob(2, 2000);
    let plan = plan(&input(&[a, big, a, big, a]), 0, &limits(1 << 20, 100, 2)).expect("plans");
    assert_eq!(plan.packs, [vec![a]]);
    assert_eq!(plan.large, [big]);
}

/// Catches an empty blob becoming a record (the segment would hold a zero-size record, which
/// ingest refuses): it is dropped, and a run of empty blobs only is pack 0 with the manifest.
#[test]
fn empty_blobs_are_dropped() {
    let a = blob(1, 10);
    let empty = blob(2, 0);
    let plan_a = plan(&input(&[empty, a]), 0, &Limits::real()).expect("plans");
    assert_eq!(plan_a.packs, [vec![a]]);
    let only = plan(&input(&[empty]), 0, &Limits::real()).expect("plans");
    assert_eq!(only.packs, [Vec::<Digest>::new()]);
    assert!(only.large.is_empty());
    let none = plan(&[], 0, &Limits::real()).expect("plans");
    assert_eq!(none.packs, [Vec::<Digest>::new()]);
}

/// The text of a refusal that must be `TooLarge`.
fn too_large(result: Result<Plan, PlanError>) -> String {
    result.expect_err("planned").to_string()
}

/// Catches more large blobs than large slots being accepted, and the count being off by one:
/// two fit two slots, three do not.
#[test]
fn more_large_blobs_than_slots_is_too_large() {
    let two = [blob(1, 2000), blob(2, 3000)];
    assert_eq!(
        plan(&input(&two), 0, &limits(1 << 20, 100, 2))
            .expect("two fit")
            .large,
        two
    );
    let three = [blob(1, 2000), blob(2, 3000), blob(3, 4000)];
    let text = too_large(plan(&input(&three), 0, &limits(1 << 20, 100, 2)));
    assert!(text.starts_with("output-too-large"), "{text}");
    assert!(text.contains("2 large slots"), "{text}");
}

/// Catches a blob over the large cap being given a large slot, and the cap being off by one.
#[test]
fn a_blob_over_the_large_cap_is_too_large() {
    let at = blob(1, 5000);
    assert_eq!(
        plan(&input(&[at]), 0, &limits(1 << 20, 100, 2))
            .expect("at the cap")
            .large,
        [at]
    );
    let over = blob(2, 5001);
    let text = too_large(plan(&input(&[over]), 0, &limits(1 << 20, 100, 2)));
    assert!(text.contains("5001"), "{text}");
    assert!(text.contains("large object"), "{text}");
}

/// Catches more packs than slots being planned, and the count being off by one: nine blobs
/// at 4 records a pack need 3 packs, which 3 slots hold and 2 do not; one slot holds only a run
/// that fits pack 0.
#[test]
fn more_packs_than_slots_is_too_large() {
    let all = blobs(9, 1);
    plan(&input(&all), 0, &limits(1 << 20, 4, 3)).expect("3 slots");
    let text = too_large(plan(&input(&all), 0, &limits(1 << 20, 4, 2)));
    assert!(text.contains("more than the 2 packs"), "{text}");
    let one = too_large(plan(&input(&all[..4]), 0, &limits(1 << 20, 4, 1)));
    assert!(one.contains("more than the 1 packs"), "{one}");
    plan(&input(&all[..3]), 0, &limits(1 << 20, 4, 1)).expect("fits pack 0");
}

/// Catches a blob that fits no pack at all looping or being dropped: it is refused, with its size.
#[test]
fn a_blob_that_fits_no_empty_pack_is_too_large() {
    let text = too_large(plan(&input(&[blob(1, 100)]), 0, &limits(1100, 100, 4)));
    assert!(
        text.contains("100 bytes does not fit an empty pack"),
        "{text}"
    );
}

/// Catches a caller's size being believed over the digest's.
#[test]
fn a_size_that_is_not_the_digests_is_refused() {
    let err = plan(&[(blob(1, 10), 11)], 0, &Limits::real()).unwrap_err();
    assert_eq!(
        err,
        PlanError::SizeMismatch {
            given: 11,
            digest: 10
        }
    );
    assert!(err.to_string().contains("11"), "{err}");
}

/// Catches the footer arithmetic drifting from the segment writer's: every planned pack is
/// accepted whole by a real [`SegmentWriter`] of the pack limit (pack 0 after a manifest-sized
/// record), and the first blob planned after a pack is refused by it as full, so no pack is
/// planned smaller than it could be.
#[test]
fn planned_packs_are_exactly_what_the_segment_writer_accepts() {
    let pack_bytes = FOOTER + 5 * 52 + 5 * 300 + 7;
    let reserve = 120;
    let data: Vec<Vec<u8>> = (0u32..23)
        .map(|n| {
            let mut bytes = n.to_be_bytes().to_vec();
            bytes.resize(100 + 10 * (n as usize % 20), 7);
            bytes
        })
        .collect();
    let all: Vec<Digest> = data.iter().map(|b| sha256(b)).collect();
    let lim = limits(pack_bytes, 1000, 8);
    let planned = plan(&input(&all), reserve, &lim).expect("plans");
    assert!(planned.packs.len() > 2);
    let by_digest = |d: &Digest| data.iter().find(|b| sha256(b) == *d).expect("known");
    // The packs in the order they are filled: slots 1.., then 0.
    let mut order: Vec<usize> = (1..planned.packs.len()).collect();
    order.push(0);
    let mut next_first = order
        .iter()
        .skip(1)
        .map(|slot| planned.packs[*slot].first().copied())
        .collect::<Vec<_>>();
    next_first.push(None);
    for (slot, first_after) in order.iter().zip(next_first) {
        let mut writer = SegmentWriter::new(pack_bytes);
        if *slot == 0 {
            writer
                .push(&vec![9u8; usize::try_from(reserve).expect("small")])
                .expect("manifest");
        }
        for d in &planned.packs[*slot] {
            writer.push(by_digest(d)).expect("a planned blob fits");
        }
        if let Some(d) = first_after {
            let refused = writer.push(by_digest(&d)).unwrap_err();
            assert!(matches!(refused, WriteError::Full { .. }), "{refused:?}");
        }
    }
}

/// Catches the real limits drifting from the design's: 128 MiB and 16,384 records a pack, 64 MiB a
/// record, 512 MiB a large object, 8 packs and 8 large objects.
#[test]
fn the_real_limits_are_the_designs() {
    assert_eq!(
        Limits::real(),
        Limits {
            pack_bytes: 128 << 20,
            pack_records: 16_384,
            record_bytes: 64 << 20,
            large_bytes: 512 << 20,
            packs: 8,
            large: 8,
        }
    );
}

/// The zig toolchain's tree as the planner sees it: 19,312 files of 328.3 MiB, of which
/// bin/zig, 164.3 MiB, is over the record cap. The rest is 164 MiB in small blobs, over the pack
/// limit by bytes, so it takes two packs; the big blob takes a large slot. Catches the model
/// of a real toolchain needing more than 2 packs and 1 large object, or a pack over a limit.
#[test]
fn the_zig_tree_is_two_packs_and_one_large_object() {
    let small: Vec<Digest> = (1..=19_312).map(|n| blob(n, 8_900)).collect();
    let zig = blob(20_000, (1_048_576 * 1643) / 10);
    assert!(zig.size_bytes > MAX_RECORD_BYTES);
    let mut all = small.clone();
    all.push(zig);
    let planned = plan(&input(&all), 64 << 10, &Limits::real()).expect("plans");
    assert_eq!(planned.packs.len(), 2);
    assert_eq!(planned.large, [zig]);
    // 14,992 blobs fill pack 1 by bytes; the other 4,320 are pack 0's with the manifest.
    assert_eq!(counts(&planned), [4_320, 14_992]);
    let first_of_pack_0 = planned.packs[0][0];
    assert_eq!(first_of_pack_0, small[14_992]);
    for (slot, pack) in planned.packs.iter().enumerate() {
        let reserve = if slot == 0 { 64 << 10 } else { 0 };
        let bytes: u64 = pack.iter().map(|d| d.size_bytes).sum();
        let records = pack.len() + usize::from(slot == 0);
        assert!(bytes + reserve + footer_len(records as u64) <= MAX_PACK_BYTES);
        assert!(records <= MAX_PACK_RECORDS);
    }
}

/// The same tree with 1 KiB files: the record limit, not the bytes, splits it. Catches the
/// 16,384-record cap being missed (a pack ingest would refuse): pack 1 takes 16,384 and pack 0
/// the other 2,928.
#[test]
fn a_tree_of_many_small_files_is_split_by_the_record_limit() {
    let all: Vec<Digest> = (1..=19_312).map(|n| blob(n, 1_024)).collect();
    let planned = plan(&input(&all), 64 << 10, &Limits::real()).expect("plans");
    assert_eq!(counts(&planned), [2_928, 16_384]);
    assert!(planned.large.is_empty());
}
