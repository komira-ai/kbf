//! The adoption of segments in place: real segments, built as a storage member builds them,
//! under the three key layouts of the members' cold store, adopted into the in-memory pack
//! index and read back through the reader.

use std::cmp::Reverse;
use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::footers::FooterReads;
use kbf_objstore::{KeyError, ObjectStoreError, PageSize};
use kbf_pack::{
    AdoptConfig, AdoptError, AdoptMode, AdoptReport, Blob, Fetch, ProblemKind, ReaderConfig,
};
use kbf_pack_index::{MAX_BATCH, NewPack, PackBlob, PackIndex, PackIndexError};
use kbf_segments::{IndexEntry, crc32c, layout, sha256};

use crate::adopt_support::{
    config, deny, five, foreign_record, found, go, log_key, make, node_key, problem, put, saving,
    segment, tally, tampered, three, writer_of,
};
use crate::support::{Rig, blob, digests};

/// Asserts that a dry run records nothing in the index, and reports what recording would do:
/// the segments, blobs and bytes it read and the rows and bytes that would be added.
#[tokio::test]
async fn a_dry_run_records_nothing_and_counts_what_recording_would_add() {
    let rig = Rig::new();
    let (keys, blobs) = three(&rig).await;
    let adopter = make(&rig, config(AdoptMode::DryRun));

    let run = go(&adopter).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert!(report.finished);
    assert_eq!(tally(&report), [3, 3, 5, 3, 0, 5, 0]);
    let total: u64 = blobs.iter().map(|blob| blob.digest.size).sum();
    assert_eq!(report.bytes, total);
    assert_eq!(report.bytes_new, total);
    assert!(rig.index.held_at_record().is_empty());
    for key in &keys {
        assert_eq!(rig.index.pack(key).unwrap(), None);
    }
    let located = rig.index.locate(&digests(&blobs)).unwrap();
    assert!(located.iter().all(Option::is_none));
}

/// Asserts that an apply records one pack per segment under the segment's own key, with the
/// writer its key layout names, that the reader then finds every blob where it lies, and that
/// the object store holds the same objects as before.
#[tokio::test]
async fn apply_records_each_segment_where_it_lies_and_the_reader_finds_every_blob() {
    let rig = Rig::new();
    let (keys, blobs) = three(&rig).await;
    let adopter = make(&rig, config(AdoptMode::Apply));

    let run = go(&adopter).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(tally(&report), [3, 3, 5, 3, 0, 5, 0]);
    assert_eq!(report.conflicts, 0);
    let writers: Vec<String> = keys.iter().map(|key| writer_of(&rig, key)).collect();
    let layouts = [
        "legacy/0000000000000001",
        "legacy/epoch-0000000000000004",
        "legacy/0000000000000006/epoch-0000000000000007",
    ];
    assert_eq!(writers, layouts);
    let reader = rig.reader(None, ReaderConfig::default());
    let got = reader.read(&digests(&blobs)).await.unwrap();
    assert_eq!(got, found(&blobs));
    let mut held = rig.store.keys_under("").await;
    held.sort();
    let mut made = keys.clone();
    made.sort();
    assert_eq!(held, made);
}

/// Asserts that a second apply over the same store creates no pack and no row, counts the
/// packs as known, and leaves every blob where the first apply put it.
#[tokio::test]
async fn a_second_apply_creates_nothing() {
    let rig = Rig::new();
    let (_, blobs) = three(&rig).await;
    let adopter = make(&rig, config(AdoptMode::Apply));
    let first = go(&adopter).await.report;
    let before = rig.index.locate(&digests(&blobs)).unwrap();

    let second = go(&adopter).await;

    assert!(second.failure.is_none());
    assert_eq!(first.packs_new, 3);
    assert_eq!(tally(&second.report), [3, 3, 5, 0, 3, 0, 0]);
    assert_eq!(rig.index.locate(&digests(&blobs)).unwrap(), before);
}

/// Asserts that a dry run after an apply finds every pack already recorded, asks the index
/// about no digest, and reports no row to add.
#[tokio::test]
async fn a_dry_run_after_an_apply_finds_every_pack_known() {
    let rig = Rig::new();
    three(&rig).await;
    go(&make(&rig, config(AdoptMode::Apply))).await;

    let run = go(&make(&rig, config(AdoptMode::DryRun))).await;

    assert_eq!(tally(&run.report), [3, 3, 5, 0, 3, 0, 0]);
    assert_eq!(run.report.bytes_new, 0);
    assert!(rig.index.locate_batches().is_empty());
}

/// Asserts that a run stopped at its segment limit, resumed from the last checkpoint it
/// saved, ends with the report and the index of one run to the end.
#[tokio::test]
async fn a_run_stopped_at_its_limit_and_resumed_ends_as_one_run_does() {
    let whole = Rig::new();
    let (keys, blobs) = five(&whole).await;
    let uninterrupted = go(&make(&whole, config(AdoptMode::Apply))).await.report;
    let split = Rig::new();
    five(&split).await;
    let limited = AdoptConfig {
        limit_segments: Some(2),
        ..config(AdoptMode::Apply)
    };

    let (first, saved) = saving(&make(&split, limited), None).await;

    assert!(first.failure.is_none());
    assert!(!first.report.finished);
    assert_eq!(first.report.segments, 2);
    assert_eq!(first.report.last_key.as_deref(), Some(keys[1].as_str()));
    assert_eq!(saved.last(), Some(&first.report));
    let resumed = make(&split, config(AdoptMode::Apply));
    let (second, _) = saving(&resumed, saved.last().cloned()).await;
    assert!(second.failure.is_none());
    let report = second.report;
    assert_eq!(report.resumed_after.as_deref(), Some(keys[1].as_str()));
    assert!(report.finished);
    let carried = AdoptReport {
        resumed_after: None,
        ..report
    };
    assert_eq!(carried, uninterrupted);
    let wanted = digests(&blobs);
    let whole_rows = whole.index.locate(&wanted).unwrap();
    assert_eq!(split.index.locate(&wanted).unwrap(), whole_rows);
}

/// Asserts that a run given a checkpoint that finished starts afresh: it lists from the first
/// key and counts from zero, so an object that appeared below the keys seen is found.
#[tokio::test]
async fn a_finished_checkpoint_starts_a_run_afresh() {
    let rig = Rig::new();
    three(&rig).await;
    let finished = AdoptReport {
        prefix: "kbf/".to_owned(),
        last_key: Some(log_key(6, 7, 8)),
        finished: true,
        objects: 99,
        ..AdoptReport::default()
    };

    let (run, _) = saving(&make(&rig, config(AdoptMode::DryRun)), Some(finished)).await;

    assert!(run.failure.is_none());
    assert_eq!(run.report.objects, 3);
    assert_eq!(run.report.segments, 3);
    assert_eq!(run.report.resumed_after, None);
}

/// Asserts that a checkpoint with no last key resumes from the first key and carries its
/// counters on.
#[tokio::test]
async fn a_checkpoint_with_no_last_key_resumes_from_the_start_with_its_counters() {
    let rig = Rig::new();
    three(&rig).await;
    let before = AdoptReport {
        prefix: "kbf/".to_owned(),
        objects: 7,
        ..AdoptReport::default()
    };

    let (run, _) = saving(&make(&rig, config(AdoptMode::DryRun)), Some(before)).await;

    assert!(run.failure.is_none());
    assert_eq!(run.report.objects, 10);
    assert_eq!(run.report.segments, 3);
    assert_eq!(run.report.resumed_after, None);
}

/// Asserts that a checkpoint of a run in another mode, over another prefix, of another source
/// store, or with a last key that is no object key is refused before anything is listed or
/// recorded.
#[tokio::test]
async fn a_checkpoint_of_another_run_is_refused_and_nothing_is_read() {
    let rig = Rig::new();
    three(&rig).await;
    let adopter = make(&rig, config(AdoptMode::DryRun));
    let other_mode = AdoptReport {
        mode: AdoptMode::Apply,
        prefix: "kbf/".to_owned(),
        ..AdoptReport::default()
    };
    let other_prefix = AdoptReport {
        prefix: "other/".to_owned(),
        ..AdoptReport::default()
    };
    let other_source = AdoptReport {
        prefix: "kbf/".to_owned(),
        source: "elsewhere".to_owned(),
        ..AdoptReport::default()
    };
    let bad_key = AdoptReport {
        prefix: "kbf/".to_owned(),
        last_key: Some("not an object key".to_owned()),
        ..AdoptReport::default()
    };

    for checkpoint in [other_mode, other_prefix, other_source] {
        let (run, saved) = saving(&adopter, Some(checkpoint)).await;
        assert!(matches!(run.failure, Some(AdoptError::ResumeMismatch)));
        assert_eq!(run.report.objects, 0);
        assert!(saved.is_empty());
    }
    let (run, _) = saving(&adopter, Some(bad_key)).await;
    assert!(matches!(run.failure, Some(AdoptError::ResumeKey(_))));
    assert_eq!(run.report.objects, 0);
    assert!(rig.store.ranges().is_empty());
}

/// Asserts that a checkpoint of a run over the same source store resumes, and that the report
/// names the store.
#[tokio::test]
async fn a_checkpoint_of_the_same_source_store_resumes_and_the_report_names_it() {
    let rig = Rig::new();
    three(&rig).await;
    let named = AdoptConfig {
        source: "elsewhere".to_owned(),
        ..config(AdoptMode::DryRun)
    };
    let before = AdoptReport {
        prefix: "kbf/".to_owned(),
        source: "elsewhere".to_owned(),
        objects: 7,
        ..AdoptReport::default()
    };

    let (run, _) = saving(&make(&rig, named), Some(before)).await;

    assert!(run.failure.is_none());
    assert_eq!(run.report.source, "elsewhere");
    assert_eq!(run.report.objects, 10);
}

/// Asserts that a segment deleted between the listing and the read of its footer is counted as
/// gone, is not adopted, and does not stop the others.
#[tokio::test]
async fn a_segment_deleted_after_the_listing_is_counted_as_gone() {
    let rig = Rig::new();
    let (keys, blobs) = three(&rig).await;
    rig.store.vanish(&keys[1]);

    let run = go(&make(&rig, config(AdoptMode::Apply))).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(report.objects, 3);
    assert_eq!(report.gone, 1);
    assert_eq!(report.segments, 2);
    assert_eq!(report.packs_new, 2);
    assert!(report.problems.is_empty());
    assert_eq!(rig.index.pack(&keys[1]).unwrap(), None);
    let reader = rig.reader(None, ReaderConfig::default());
    let got = reader.read(&digests(&blobs)).await.unwrap();
    assert_eq!(got[2], Fetch::Missing);
    assert_eq!(got[0], found(&blobs)[0]);
}

/// Asserts that a footer the store failed to read is counted, listed as a problem and not
/// adopted, and that the others are.
#[tokio::test]
async fn a_footer_the_store_failed_to_read_is_counted_and_listed() {
    let rig = Rig::new();
    let (keys, _) = three(&rig).await;
    let mut sorted = keys.clone();
    sorted.sort();
    let one_at_a_time = FooterReads {
        concurrency: NonZeroUsize::new(1).unwrap(),
        ..FooterReads::default()
    };
    let slow = AdoptConfig {
        reads: one_at_a_time,
        ..config(AdoptMode::Apply)
    };
    rig.store.fail_gets(1);

    let run = go(&make(&rig, slow)).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(report.objects, 3);
    assert_eq!(report.read_failed, 1);
    assert_eq!(report.segments, 2);
    assert_eq!(report.packs_new, 2);
    let listed = problem(&sorted[0], ProblemKind::Unreadable);
    assert_eq!(report.problems, [listed]);
    assert_eq!(rig.index.pack(&sorted[0]).unwrap(), None);
}

/// Asserts that an object too short to be a segment, one with a foreign tail, a segment torn
/// short and a segment whose footer lists a digest twice are each counted and skipped, and
/// that segments under keys in none of the layouts the members write are counted and left
/// alone, whatever way the key is wrong.
#[tokio::test]
async fn objects_that_are_not_segments_and_keys_in_no_layout_are_counted_and_skipped() {
    let rig = Rig::new();
    let whole = segment(&[blob("good")]);
    put(&rig, &node_key(1, 1, 1), whole.clone()).await;
    put(&rig, &node_key(1, 1, 2), Bytes::from_static(b"tiny")).await;
    put(&rig, &node_key(1, 1, 3), Bytes::from(vec![b'x'; 100])).await;
    put(&rig, &node_key(1, 1, 4), whole.slice(..whole.len() - 5)).await;
    let record = b"dup";
    let entry = IndexEntry {
        digest: sha256(record),
        offset: 0,
        crc32c: crc32c(record),
    };
    let footer = layout::encode_footer(record.len() as u64, &[entry, entry]);
    let mut twice = record.to_vec();
    twice.extend(footer);
    put(&rig, &node_key(1, 1, 5), Bytes::from(twice)).await;
    let hex = "0000000000000001";
    let foreign = [
        "kbf/readme".to_owned(),
        "kbf/cas/zz/yy".to_owned(),
        format!("kbf/cas/{hex}/{hex}/zz"),
        format!("kbf/zz/cas/{hex}/{hex}"),
        "kbf/cas/000000000000000A/0000000000000001".to_owned(),
        format!("kbf/cas/{hex}/{hex}/{hex}/{hex}"),
    ];
    for key in &foreign {
        put(&rig, key, whole.clone()).await;
    }

    let run = go(&make(&rig, config(AdoptMode::Apply))).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(report.objects, 11);
    assert_eq!(report.segments, 7);
    assert_eq!(report.short, 1);
    assert_eq!(report.not_a_segment, 3);
    assert_eq!(report.foreign, 6);
    assert_eq!(report.packs_new, 1);
    assert_eq!(report.blobs, 1);
    assert!(report.problems.is_empty());
    for key in &foreign {
        assert_eq!(rig.index.pack(key).unwrap(), None, "{key}");
    }
}

/// Asserts that a blob in two segments keeps the location of the first segment adopted, and
/// that the second segment's record counts the row it did not make.
#[tokio::test]
async fn a_digest_in_two_segments_keeps_the_first_location() {
    let rig = Rig::new();
    let (x, y, z) = (blob("x"), blob("y"), blob("z"));
    let (first, second) = (node_key(1, 1, 1), node_key(1, 1, 2));
    put(&rig, &first, segment(&[x.clone(), y.clone()])).await;
    put(&rig, &second, segment(&[y.clone(), z.clone()])).await;

    let run = go(&make(&rig, config(AdoptMode::Apply))).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(report.packs_new, 2);
    assert_eq!(report.rows_new, 3);
    assert_eq!(report.rows_existing, 1);
    let located = rig.index.locate(&[y.digest, z.digest]).unwrap();
    assert_eq!(located[0].as_ref().unwrap().pack_key, first);
    assert_eq!(located[1].as_ref().unwrap().pack_key, second);
}

/// Asserts that a dry run asks the index about the digests of a segment in batches of at most
/// the index's limit, counts the digests that have a row already and the bytes of those that
/// have none.
#[tokio::test]
async fn a_dry_run_looks_digests_up_in_batches_and_counts_the_rows_that_exist() {
    let rig = Rig::new();
    let blobs: Vec<Blob> = (0..=MAX_BATCH).map(|i| blob(&format!("b-{i}"))).collect();
    put(&rig, &node_key(1, 1, 1), segment(&blobs)).await;
    let had = [0, 500, MAX_BATCH];
    let placed: Vec<PackBlob> = had
        .iter()
        .map(|&i| PackBlob {
            digest: blobs[i].digest,
            offset: i as u64 * 100,
        })
        .collect();
    let elsewhere = NewPack {
        key: "elsewhere/pack".to_owned(),
        writer: "w".to_owned(),
        size: 1 << 20,
        blobs: placed,
    };
    rig.index.record_pack(&elsewhere).unwrap();

    let run = go(&make(&rig, config(AdoptMode::DryRun))).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(report.packs_new, 1);
    assert_eq!(report.blobs, MAX_BATCH as u64 + 1);
    assert_eq!(report.rows_new, MAX_BATCH as u64 - 2);
    assert_eq!(report.rows_existing, 3);
    let all: u64 = blobs.iter().map(|blob| blob.digest.size).sum();
    let owned: u64 = had.iter().map(|&i| blobs[i].digest.size).sum();
    assert_eq!(report.bytes_new, all - owned);
    assert_eq!(rig.index.locate_batches(), [MAX_BATCH, 1]);
}

/// Asserts that a sample reads one blob back from each of the first segments with a ranged
/// read of just that blob, and that a blob whose bytes are not its footer's digest is counted,
/// listed, and keeps its segment out of the index in apply mode, while the others are adopted.
#[tokio::test]
async fn a_footer_that_does_not_match_its_bytes_is_caught_by_the_sample() {
    let rig = Rig::new();
    let all = [blob("alpha"), blob("bravo"), blob("charlie")];
    let keys = [node_key(1, 1, 1), node_key(1, 1, 2), node_key(1, 1, 3)];
    put(&rig, &keys[0], segment(&all[0..1])).await;
    put(&rig, &keys[1], tampered(&all[1..2])).await;
    put(&rig, &keys[2], segment(&all[2..3])).await;
    let sampled = AdoptConfig {
        verify_sample: 10,
        ..config(AdoptMode::Apply)
    };

    let run = go(&make(&rig, sampled)).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(report.sampled, 3);
    assert_eq!(report.sample_bad, 1);
    let listed = problem(&keys[1], ProblemKind::Mismatch);
    assert_eq!(report.problems, [listed]);
    assert_eq!(report.packs_new, 2);
    assert_eq!(report.blobs, 2);
    assert_eq!(rig.index.pack(&keys[1]).unwrap(), None);
    // The run read three footers and the three sampled blobs, nothing else.
    let ranges = rig.store.ranges();
    assert_eq!(ranges.len(), 6);
    assert!(ranges.contains(&(keys[1].clone(), 0, all[1].digest.size)));
    let reader = rig.reader(None, ReaderConfig::default());
    let got = reader.read(&digests(&all)).await.unwrap();
    let want = [
        Fetch::Found(all[0].bytes.clone()),
        Fetch::Missing,
        Fetch::Found(all[2].bytes.clone()),
    ];
    assert_eq!(got, want);
}

/// Asserts that a dry run with a sample reports the same mismatch and leaves the segment out
/// of the packs it would add, and that with no sample nothing is read from a segment but its
/// footer.
#[tokio::test]
async fn a_dry_run_reports_the_mismatch_and_without_a_sample_only_footers_are_read() {
    let rig = Rig::new();
    let keys = [node_key(1, 1, 1), node_key(1, 1, 2)];
    put(&rig, &keys[0], segment(&[blob("alpha")])).await;
    put(&rig, &keys[1], tampered(&[blob("bravo")])).await;
    let sampled = AdoptConfig {
        verify_sample: 2,
        ..config(AdoptMode::DryRun)
    };

    let run = go(&make(&rig, sampled)).await;

    assert_eq!(run.report.sampled, 2);
    assert_eq!(run.report.sample_bad, 1);
    assert_eq!(run.report.packs_new, 1);
    let unsampled = go(&make(&rig, config(AdoptMode::DryRun))).await;
    assert_eq!(unsampled.report.sampled, 0);
    assert_eq!(unsampled.report.packs_new, 2);
    // Two footers and two blobs read back, then two footers.
    assert_eq!(rig.store.ranges().len(), 6);
}

/// Asserts that a sampled blob that cannot be read back is a mismatch like one with the wrong
/// bytes: counted, listed, and its segment is not recorded.
#[tokio::test]
async fn a_sampled_blob_that_cannot_be_read_back_is_a_mismatch() {
    let rig = Rig::new();
    let key = node_key(1, 1, 1);
    put(&rig, &key, segment(&[blob("alpha")])).await;
    let sampled = AdoptConfig {
        verify_sample: 1,
        ..config(AdoptMode::Apply)
    };
    rig.store.fail_gets_after(1, 1);

    let run = go(&make(&rig, sampled)).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(report.sampled, 1);
    assert_eq!(report.sample_bad, 1);
    let listed = problem(&key, ProblemKind::Mismatch);
    assert_eq!(report.problems, [listed]);
    assert_eq!(report.packs_new, 0);
    assert_eq!(rig.index.pack(&key).unwrap(), None);
}

/// Asserts that the sample reads a different blob of each segment in turn, so that a blob that
/// is not the first of its footer is read back too: here the second blob of the second segment.
#[tokio::test]
async fn the_sample_reads_a_different_blob_of_each_segment_in_turn() {
    let rig = Rig::new();
    let keys = [node_key(1, 1, 1), node_key(1, 1, 2)];
    let whole = [blob("alpha"), blob("bravo")];
    let mut damaged = [blob("charlie"), blob("delta")];
    // A footer lists its blobs by digest, and `tampered` changes the record written first:
    // writing the greater digest first makes the changed blob the second of the footer.
    damaged.sort_by_key(|blob| Reverse(blob.digest));
    put(&rig, &keys[0], segment(&whole)).await;
    put(&rig, &keys[1], tampered(&damaged)).await;
    let sampled = AdoptConfig {
        verify_sample: 2,
        ..config(AdoptMode::Apply)
    };

    let run = go(&make(&rig, sampled)).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(report.sampled, 2);
    assert_eq!(report.sample_bad, 1);
    let listed = problem(&keys[1], ProblemKind::Mismatch);
    assert_eq!(report.problems, [listed]);
    assert!(rig.index.pack(&keys[0]).unwrap().is_some());
    assert_eq!(rig.index.pack(&keys[1]).unwrap(), None);
}

/// Asserts that a segment of only empty blobs, a segment with no blobs and a segment with an
/// empty blob and another are adopted, in a dry run (which counts each segment's digests by
/// themselves) and in an apply, that no sample read is made of an empty blob, and that the
/// reader finds the empty blob.
#[tokio::test]
async fn segments_of_empty_blobs_and_with_no_blobs_are_adopted() {
    let rig = Rig::new();
    let both = [blob(""), blob("x")];
    let keys = [node_key(1, 1, 1), node_key(1, 1, 2), node_key(1, 1, 3)];
    put(&rig, &keys[0], segment(&both[0..1])).await;
    put(&rig, &keys[1], segment(&[])).await;
    put(&rig, &keys[2], segment(&both)).await;
    let dry = go(&make(&rig, config(AdoptMode::DryRun))).await.report;
    assert_eq!(tally(&dry), [3, 3, 3, 3, 0, 3, 0]);
    let sampled = AdoptConfig {
        verify_sample: 5,
        ..config(AdoptMode::Apply)
    };

    let run = go(&make(&rig, sampled)).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(tally(&report), [3, 3, 3, 3, 0, 2, 1]);
    assert_eq!(report.sampled, 1);
    let reader = rig.reader(None, ReaderConfig::default());
    let got = reader.read(&digests(&both)).await.unwrap();
    assert_eq!(got, found(&both));
}

/// Asserts that a pack the index holds under a segment's key with other contents is a
/// conflict that is counted and listed and does not stop the run, and that no row of the
/// conflicting segment is made.
#[tokio::test]
async fn a_conflict_is_reported_and_the_run_goes_on() {
    let rig = Rig::new();
    let (keys, blobs) = three(&rig).await;
    rig.index.record_pack(&foreign_record(&keys[0])).unwrap();

    let run = go(&make(&rig, config(AdoptMode::Apply))).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(report.conflicts, 1);
    assert_eq!(report.packs_new, 2);
    assert_eq!(report.segments, 3);
    let listed = problem(&keys[0], ProblemKind::Conflict);
    assert_eq!(report.problems, [listed]);
    let located = rig.index.locate(&digests(&blobs)).unwrap();
    assert!(located[..2].iter().all(Option::is_none));
    assert!(located[2..].iter().all(Option::is_some));
}

/// Asserts that a dry run over a key the index holds as a pack with other contents counts a
/// conflict as an apply does, lists it, counts the pack as neither new nor known, and with the
/// flag ends as a failure naming the pack.
#[tokio::test]
async fn a_dry_run_reports_the_conflict_an_apply_would() {
    let rig = Rig::new();
    let (keys, _) = three(&rig).await;
    rig.index.record_pack(&foreign_record(&keys[0])).unwrap();

    let run = go(&make(&rig, config(AdoptMode::DryRun))).await;

    assert!(run.failure.is_none());
    let report = run.report;
    assert_eq!(report.conflicts, 1);
    assert_eq!(report.packs_new, 2);
    assert_eq!(report.packs_known, 0);
    assert_eq!(report.segments, 3);
    let listed = problem(&keys[0], ProblemKind::Conflict);
    assert_eq!(report.problems, [listed]);
    let strict = AdoptConfig {
        fail_on_conflict: true,
        ..config(AdoptMode::DryRun)
    };
    let stopped = go(&make(&rig, strict)).await;
    let key = match stopped.failure {
        Some(AdoptError::Conflict { key }) => key,
        other => panic!("expected a conflict, got {other:?}"),
    };
    assert_eq!(key, keys[0]);
    assert_eq!(stopped.report.conflicts, 0);
}

/// Asserts that with the flag a conflict ends the run as a failure naming the pack, with the
/// segments before it recorded and the checkpoint at the last of them, and that the conflict is
/// not counted in that checkpoint: it is counted by the run that meets it again.
#[tokio::test]
async fn a_conflict_ends_the_run_when_asked_to() {
    let rig = Rig::new();
    let (keys, _) = three(&rig).await;
    rig.index.record_pack(&foreign_record(&keys[0])).unwrap();
    let strict = AdoptConfig {
        fail_on_conflict: true,
        ..config(AdoptMode::Apply)
    };

    let (run, saved) = saving(&make(&rig, strict), None).await;

    let key = match run.failure {
        Some(AdoptError::Conflict { key }) => key,
        other => panic!("expected a conflict, got {other:?}"),
    };
    assert_eq!(key, keys[0]);
    assert_eq!(run.report.packs_new, 1);
    assert_eq!(run.report.conflicts, 0);
    assert!(run.report.problems.is_empty());
    assert_eq!(run.report.last_key.as_deref(), Some(keys[2].as_str()));
    assert_eq!(saved.last(), Some(&run.report));
}

/// Asserts that a run resumed from the checkpoint of a run that stopped at a conflict counts
/// the conflict once and lists it once, and goes on to adopt the segments after it.
#[tokio::test]
async fn a_run_resumed_after_stopping_at_a_conflict_counts_it_once() {
    let rig = Rig::new();
    let (keys, blobs) = three(&rig).await;
    rig.index.record_pack(&foreign_record(&keys[0])).unwrap();
    let strict = AdoptConfig {
        fail_on_conflict: true,
        ..config(AdoptMode::Apply)
    };
    let (stopped, saved) = saving(&make(&rig, strict), None).await;
    let failure = stopped.failure;
    assert!(matches!(failure, Some(AdoptError::Conflict { .. })));
    let lenient = make(&rig, config(AdoptMode::Apply));

    let (again, _) = saving(&lenient, saved.last().cloned()).await;

    assert!(again.failure.is_none());
    let report = again.report;
    assert_eq!(report.conflicts, 1);
    let listed = problem(&keys[0], ProblemKind::Conflict);
    assert_eq!(report.problems, [listed]);
    assert_eq!(report.packs_new, 2);
    assert!(report.finished);
    let located = rig.index.locate(&digests(&blobs)).unwrap();
    assert!(located[2..].iter().all(Option::is_some));
}

/// Asserts that the report lists no more problems than it is told to, and counts them all.
#[tokio::test]
async fn the_problems_listed_are_capped_and_the_counts_are_not() {
    let rig = Rig::new();
    let (keys, _) = three(&rig).await;
    rig.index.record_pack(&foreign_record(&keys[0])).unwrap();
    rig.index.record_pack(&foreign_record(&keys[1])).unwrap();
    let capped = AdoptConfig {
        max_problems: 1,
        ..config(AdoptMode::Apply)
    };

    let run = go(&make(&rig, capped)).await;

    assert_eq!(run.report.conflicts, 2);
    assert_eq!(run.report.problems.len(), 1);
}

/// Asserts that an index failure ends the run with the failure, leaves the segment it failed
/// on uncounted and out of the checkpoint, and that a run resumed from the checkpoint adopts
/// everything.
#[tokio::test]
async fn an_index_failure_ends_the_run_and_a_resumed_run_finishes_it() {
    let rig = Rig::new();
    let (_, blobs) = three(&rig).await;
    rig.index.fail_record(1);
    let adopter = make(&rig, config(AdoptMode::Apply));

    let (run, saved) = saving(&adopter, None).await;

    let failure = run.failure;
    assert!(matches!(
        failure,
        Some(AdoptError::Index(PackIndexError::NotMigrated))
    ));
    assert_eq!(run.report.objects, 0);
    assert_eq!(run.report.segments, 0);
    assert_eq!(run.report.packs_new, 0);
    assert_eq!(run.report.last_key, None);
    assert_eq!(saved.len(), 1);
    let (again, _) = saving(&adopter, saved.last().cloned()).await;
    assert!(again.failure.is_none());
    assert_eq!(again.report.objects, 3);
    assert_eq!(again.report.packs_new, 3);
    let reader = rig.reader(None, ReaderConfig::default());
    let got = reader.read(&digests(&blobs)).await.unwrap();
    assert_eq!(got, found(&blobs));
}

/// Asserts that a segment whose record fails leaves its sampled blob uncounted too, so that a
/// run resumed from the checkpoint counts each blob it reads back once.
#[tokio::test]
async fn a_failed_segment_leaves_its_sample_uncounted_and_a_resumed_run_counts_it_once() {
    let rig = Rig::new();
    three(&rig).await;
    let sampled = AdoptConfig {
        verify_sample: 10,
        ..config(AdoptMode::Apply)
    };
    let adopter = make(&rig, sampled);
    rig.index.fail_record(1);

    let (run, saved) = saving(&adopter, None).await;

    assert!(matches!(run.failure, Some(AdoptError::Index(_))));
    assert_eq!(run.report.sampled, 0);
    let (again, _) = saving(&adopter, saved.last().cloned()).await;
    assert!(again.failure.is_none());
    assert_eq!(again.report.sampled, 3);
    assert_eq!(again.report.sample_bad, 0);
}

/// Asserts that a lookup that fails in a dry run ends the run with the index's failure and
/// leaves the pack, its rows and its bytes uncounted.
#[tokio::test]
async fn a_lookup_that_fails_in_a_dry_run_ends_the_run_and_counts_nothing_of_the_pack() {
    let rig = Rig::new();
    three(&rig).await;
    rig.index.fail_locate(1);

    let run = go(&make(&rig, config(AdoptMode::DryRun))).await;

    let failure = run.failure;
    assert!(matches!(
        failure,
        Some(AdoptError::Index(PackIndexError::NotMigrated))
    ));
    let report = run.report;
    assert_eq!(tally(&report), [0; 7]);
    assert_eq!(report.bytes_new, 0);
    assert_eq!(report.last_key, None);
}

/// Asserts that a listing that fails part-way ends the run with the listing's failure and a
/// checkpoint that is not past any key listed before it, and that a run resumed from that
/// checkpoint ends with every blob adopted.
#[tokio::test]
async fn a_listing_failure_does_not_advance_the_checkpoint_past_it() {
    let rig = Rig::new();
    let (keys, blobs) = five(&rig).await;
    let paged = AdoptConfig {
        reads: FooterReads {
            page: PageSize::new(2).unwrap(),
            ..FooterReads::default()
        },
        ..config(AdoptMode::Apply)
    };
    let adopter = make(&rig, paged);
    rig.store.fail_list_after(1);

    let (run, saved) = saving(&adopter, None).await;

    assert!(matches!(run.failure, Some(AdoptError::Listing(_))));
    assert!(!run.report.finished);
    assert!(run.report.last_key.as_deref() <= Some(keys[1].as_str()));
    assert_eq!(saved.last(), Some(&run.report));
    let (again, _) = saving(&adopter, saved.last().cloned()).await;
    assert!(again.failure.is_none());
    assert!(again.report.finished);
    let reader = rig.reader(None, ReaderConfig::default());
    let got = reader.read(&digests(&blobs)).await.unwrap();
    assert_eq!(got, found(&blobs));
}

/// Asserts that the checkpoint is saved every given number of objects and once more at the
/// end, and that at each save the segment of its last key is already in the index.
#[tokio::test]
async fn a_checkpoint_is_saved_every_few_objects_after_their_records_are_in_the_index() {
    let rig = Rig::new();
    let (keys, _) = five(&rig).await;
    let every_two = AdoptConfig {
        checkpoint_every: NonZeroUsize::new(2).unwrap(),
        ..config(AdoptMode::Apply)
    };
    let adopter = make(&rig, every_two);
    let mut seen = Vec::new();
    let mut check = |report: &AdoptReport| -> io::Result<()> {
        let last = report.last_key.clone().unwrap();
        let recorded = rig.index.pack(&last).unwrap().is_some();
        seen.push((report.objects, report.finished, recorded));
        Ok(())
    };

    let run = adopter.run(None, &mut check).await;

    assert!(run.failure.is_none());
    let expected = [(2, false, true), (4, false, true), (5, true, true)];
    assert_eq!(seen, expected);
    assert_eq!(run.report.last_key.as_deref(), Some(keys[4].as_str()));
}

/// Asserts that a checkpoint that cannot be saved ends the run with that failure, after the
/// objects handled so far are recorded.
#[tokio::test]
async fn a_checkpoint_that_cannot_be_saved_ends_the_run() {
    let rig = Rig::new();
    let (keys, _) = five(&rig).await;
    let every_one = AdoptConfig {
        checkpoint_every: NonZeroUsize::new(1).unwrap(),
        ..config(AdoptMode::Apply)
    };
    let adopter = make(&rig, every_one);

    let run = adopter.run(None, &mut deny).await;

    assert!(matches!(run.failure, Some(AdoptError::Checkpoint(_))));
    assert_eq!(run.report.objects, 1);
    assert!(rig.index.pack(&keys[0]).unwrap().is_some());
    assert_eq!(rig.index.pack(&keys[1]).unwrap(), None);
}

/// Asserts that segments are handled no faster than the rate given, and that without a rate a
/// run waits for nothing.
#[tokio::test(start_paused = true)]
async fn segments_are_handled_no_faster_than_the_rate() {
    let rig = Rig::new();
    three(&rig).await;
    let slow = AdoptConfig {
        segments_per_second: NonZeroU32::new(2),
        ..config(AdoptMode::DryRun)
    };
    let started = tokio::time::Instant::now();

    let paced = go(&make(&rig, slow)).await;

    assert!(paced.failure.is_none());
    assert_eq!(started.elapsed(), Duration::from_secs(1));
    let unpaced = go(&make(&rig, config(AdoptMode::DryRun))).await;
    assert!(unpaced.failure.is_none());
    assert_eq!(started.elapsed(), Duration::from_secs(1));
}

/// Asserts that `error` says `wanted`.
fn says(error: AdoptError, wanted: &str) {
    let said = error.to_string();
    assert!(said.contains(wanted), "{said}");
}

/// Asserts that each way a run fails says what failed.
#[test]
fn each_failure_says_what_failed() {
    let protocol = ObjectStoreError::Protocol("a page twice".to_owned());
    let listing = AdoptError::Listing(protocol);
    let index = AdoptError::Index(PackIndexError::NotMigrated);
    let conflict = AdoptError::Conflict {
        key: "kbf/k".to_owned(),
    };
    let checkpoint = AdoptError::Checkpoint(io::Error::other("full"));
    let mismatch = AdoptError::ResumeMismatch;
    let bad_key = AdoptError::ResumeKey(KeyError::Empty);

    says(listing, "listing the source store: ");
    says(index, "pack index: ");
    says(conflict, "pack kbf/k is recorded already");
    says(checkpoint, "saving the checkpoint: full");
    says(mismatch, "another mode, prefix or source store");
    says(bad_key, "last key of the checkpoint");
}

/// Asserts that a configuration can be copied and printed, and so can the end of a run.
#[tokio::test]
async fn a_configuration_and_a_run_can_be_copied_and_printed() {
    let rig = Rig::new();
    let configured = AdoptConfig {
        limit_segments: Some(1),
        ..config(AdoptMode::DryRun)
    };
    let copy = configured.clone();

    let run = go(&make(&rig, copy)).await;

    assert_eq!(configured.limit_segments, Some(1));
    let shown = format!("{configured:?}");
    assert!(shown.contains("DryRun"), "{shown}");
    let ended = format!("{run:?}");
    assert!(ended.contains("AdoptRun"), "{ended}");
}

/// Asserts that a report written as JSON reads back equal, with its mode and its problems
/// spelled as the checkpoint file keeps them, that a field the file lacks reads as zero and a
/// field it does not know is ignored.
#[test]
fn a_report_survives_json_and_a_missing_field_reads_as_zero() {
    let kinds = [
        (ProblemKind::Unreadable, "unreadable"),
        (ProblemKind::Conflict, "conflict"),
        (ProblemKind::Mismatch, "mismatch"),
    ];
    let report = AdoptReport {
        mode: AdoptMode::Apply,
        prefix: "kbf/".to_owned(),
        last_key: Some("kbf/a".to_owned()),
        objects: 3,
        problems: kinds.map(|(kind, _)| problem("kbf/a", kind)).to_vec(),
        ..AdoptReport::default()
    };

    let text = serde_json::to_string(&report).unwrap();

    let back: AdoptReport = serde_json::from_str(&text).unwrap();
    assert_eq!(back, report);
    assert!(text.contains(r#""mode":"apply""#), "{text}");
    for (_, spelled) in kinds {
        let needle = format!(r#""kind":"{spelled}""#);
        assert!(text.contains(&needle), "{text}");
    }
    let older = r#"{"mode": "dry-run", "objects": 7, "from_a_later_build": true}"#;
    let old: AdoptReport = serde_json::from_str(older).unwrap();
    assert_eq!(old.mode, AdoptMode::DryRun);
    assert_eq!(old.objects, 7);
    assert_eq!(old.segments, 0);
    let empty = serde_json::from_str::<AdoptReport>("{}").unwrap();
    assert_eq!(empty, AdoptReport::default());
}
