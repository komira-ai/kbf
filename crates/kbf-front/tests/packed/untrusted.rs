//! `Packs::commit_untrusted_pack`: a pack that a build wrote through a presigned write is recorded
//! in the pack index only after the frontend has read its footer, matched it to the manifest the
//! build reported over its lease, and hashed every byte of every record.
//!
//! Every test says the behaviour it asserts and the defect it catches.

use kbf_front::{
    CommitError, IssuedPackKey, MAX_COMMIT_BLOBS, MAX_UNTRUSTED_BLOB_BYTES,
    MAX_UNTRUSTED_MANIFEST_BYTES, MAX_UNTRUSTED_PACK_BYTES, MAX_UNTRUSTED_READ_BYTES,
    UntrustedLimits,
};
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex as _, Recorded};
use kbf_segments::layout::{encode_footer, footer_len};
use kbf_segments::{Footer, IndexEntry, MAX_SEGMENT_BYTES, SegmentWriter, crc32c, sha256};

use crate::support::{Rig, index_form};

const BUILD: &str = "build7";
const KEY: &str = "cas/b/build7/n0nce1";
const DAEMON_KEY: &str = "cas/d/node1/boot1/0000000000000000";
const FIRST: &str = "first blob";
const SECOND: &str = "second blob";

/// The key the frontend issued for the build.
fn issued() -> IssuedPackKey {
    IssuedPackKey::new(BUILD, "n0nce1").expect("a key")
}

/// The manifest of a pack of `size` bytes that lists `entries`, as the build reports it, with its
/// blobs in the order a manifest must list them.
fn manifest_of(entries: &[IndexEntry], size: u64) -> NewPack {
    let mut blobs: Vec<PackBlob> = entries
        .iter()
        .map(|entry| PackBlob {
            digest: index_form(&entry.digest),
            offset: entry.offset,
        })
        .collect();
    blobs.sort_by_key(|blob| (blob.digest.hash, blob.digest.size));
    NewPack {
        key: KEY.to_owned(),
        writer: BUILD.to_owned(),
        size,
        blobs,
    }
}

/// The segment of `texts` as the packer writes it, and the manifest that is true of it.
fn segment_of(texts: &[&str]) -> (Vec<u8>, NewPack) {
    let mut writer = SegmentWriter::new(MAX_SEGMENT_BYTES);
    for text in texts {
        writer.push(text.as_bytes()).expect("pushed");
    }
    let bytes = writer.finish();
    let footer = Footer::parse(&bytes, bytes.len() as u64).expect("a footer");
    let manifest = manifest_of(footer.entries(), bytes.len() as u64);
    (bytes, manifest)
}

/// The index entry of `text` at `offset`, with the CRC of its bytes.
fn entry(text: &str, offset: u64) -> IndexEntry {
    IndexEntry {
        digest: sha256(text.as_bytes()),
        offset,
        crc32c: crc32c(text.as_bytes()),
    }
}

/// A pack of `body` as records and a footer that lists `entries`, and the manifest that lists the
/// same entries.
fn forged(entries: &[IndexEntry], body: &[u8]) -> (Vec<u8>, NewPack) {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|entry| entry.digest);
    let mut bytes = body.to_vec();
    bytes.extend(encode_footer(body.len() as u64, &sorted));
    let manifest = manifest_of(&sorted, bytes.len() as u64);
    (bytes, manifest)
}

/// A manifest that lists `blobs` (text and offset) for a pack of `size` bytes.
fn listed(blobs: &[(&str, u64)], size: u64) -> NewPack {
    let entries: Vec<IndexEntry> = blobs.iter().map(|(text, at)| entry(text, *at)).collect();
    manifest_of(&entries, size)
}

/// The commit of `manifest` with the key issued and the default limits.
async fn commit(rig: &Rig, manifest: NewPack) -> Result<Recorded, CommitError> {
    let limits = UntrustedLimits::default();
    rig.packs
        .commit_untrusted_pack(&issued(), manifest, &limits)
        .await
}

/// Asserts that `manifest` is refused as invalid, with `said` in the reason, before the store is
/// asked for a byte, recording nothing and counting the refusal.
async fn refused_unread(manifest: NewPack, said: &str) {
    let rig = Rig::new().await;

    let refused = commit(&rig, manifest).await;

    let refused = refused.expect_err("refused");
    assert!(matches!(refused, CommitError::Invalid(_)), "{refused:?}");
    assert!(refused.message().contains(said), "{said}: {refused:?}");
    assert!(rig.store.ranges().is_empty(), "the store was not asked");
    assert!(rig.index.pack(KEY).expect("asked").is_none());
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 1);
}

/// Asserts the happy path and its cost: an honest pack is recorded where it lies, as written by
/// the build, its blobs located at the manifest's offsets; every byte of the pack was asked for
/// (the trailer and its next byte, the footer, and each record whole); and the same commit sent
/// again is answered `created: false`. Catches a commit that records at the wrong offsets or under
/// the wrong writer, one that skips a record, and one that is not idempotent.
#[tokio::test]
async fn an_honest_pack_is_recorded_in_place_and_a_replay_changes_nothing() {
    let texts = [FIRST, SECOND, "third blob"];
    let (bytes, manifest) = segment_of(&texts);
    let rig = Rig::new().await;
    rig.store.seed(KEY, &bytes).await;

    let first = commit(&rig, manifest.clone()).await.expect("committed");

    assert_eq!((first.created, first.indexed), (true, 3));
    let info = rig.index.pack(KEY).expect("asked").expect("a row");
    assert_eq!(info.writer, "b/build7");
    let digests: Vec<_> = texts.iter().map(|text| sha256(text.as_bytes())).collect();
    for (digest, place) in digests.iter().zip(rig.index.where_is(&digests)) {
        let place = place.expect("located");
        let at = manifest.blobs.iter().find(|b| b.digest.hash == digest.hash);
        let at = at.expect("listed").offset;
        assert_eq!((place.pack_key.as_str(), place.offset), (KEY, at));
    }
    let ranges = rig.store.ranges();
    let asked: u64 = ranges.iter().map(|(_, _, len)| len).sum();
    assert_eq!(asked, bytes.len() as u64 + 33, "{ranges:?}");
    for blob in &manifest.blobs {
        let record = (KEY.to_owned(), blob.offset, blob.digest.size);
        assert!(ranges.contains(&record), "{record:?} in {ranges:?}");
    }
    assert_eq!(rig.packs.metrics().commits.get(), 1);

    let again = commit(&rig, manifest).await.expect("replayed");
    assert_eq!((again.created, again.indexed), (false, 3));
    assert_eq!(rig.packs.metrics().commits.get(), 1);
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 0);
}

/// Asserts that a record is hashed in ranges of at most 8 MiB: a record of 8 MiB and 5 bytes is
/// read as a range of 8 MiB and a range of 5 bytes. Catches a hash that holds a record whole.
#[tokio::test]
async fn a_large_record_is_hashed_in_ranges_of_at_most_eight_mebibytes() {
    let chunk: u64 = 8 * 1024 * 1024;
    let large = "a".repeat(chunk as usize + 5);
    let (bytes, manifest) = segment_of(&[large.as_str()]);
    let rig = Rig::new().await;
    rig.store.seed(KEY, &bytes).await;

    commit(&rig, manifest).await.expect("committed");

    let ranges = rig.store.ranges();
    let whole = (KEY.to_owned(), 0, chunk);
    let rest = (KEY.to_owned(), chunk, 5);
    assert_eq!(ranges[2..], [whole, rest]);
}

/// Asserts that a build whose manifest lies about the bytes of one blob is refused, for a blob
/// that a daemon's two-blob sample does not look at. Six blobs are stored with one byte of one
/// record changed; the daemon commit, which checks two blobs, records the pack for some blob it
/// does not sample; the same bytes committed as an untrusted pack are refused as a mismatch,
/// whichever blob changed, and nothing is recorded. Catches an untrusted commit that samples.
#[tokio::test]
async fn a_blob_outside_the_daemon_sample_is_still_hashed() {
    let texts: Vec<String> = (0..6).map(|n| format!("blob number {n}")).collect();
    let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
    let (bytes, manifest) = segment_of(&texts);
    let daemons = NewPack {
        key: DAEMON_KEY.to_owned(),
        writer: "node1".to_owned(),
        ..manifest.clone()
    };
    let mut accepted = None;
    for blob in &manifest.blobs {
        let mut lying = bytes.clone();
        lying[blob.offset as usize] ^= 1;
        let rig = Rig::new().await;
        rig.store.seed(DAEMON_KEY, &lying).await;
        if rig
            .packs
            .commit_pack("node1", daemons.clone(), 2)
            .await
            .is_ok()
        {
            accepted = Some(lying);
            break;
        }
    }
    let lying = accepted.expect("a sample of two cannot look at all six blobs");

    let rig = Rig::new().await;
    rig.store.seed(KEY, &lying).await;
    let refused = commit(&rig, manifest).await;

    let refused = refused.expect_err("refused");
    assert!(matches!(refused, CommitError::Mismatch(_)), "{refused:?}");
    assert!(
        refused.message().contains("holds other bytes"),
        "{refused:?}"
    );
    assert!(rig.index.pack(KEY).expect("asked").is_none());
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 1);
}

/// Asserts that a record whose bytes are its digest but whose CRC-32C in the footer is another is
/// refused as a mismatch. Catches a pack that the segment reader would later refuse to serve.
#[tokio::test]
async fn a_record_whose_footer_crc_is_not_its_bytes_is_refused() {
    let mut wrong = entry(FIRST, 0);
    wrong.crc32c ^= 1;
    let (bytes, manifest) = forged(&[wrong], FIRST.as_bytes());
    let rig = Rig::new().await;
    rig.store.seed(KEY, &bytes).await;

    let refused = commit(&rig, manifest).await;

    let refused = refused.expect_err("refused");
    assert!(matches!(refused, CommitError::Mismatch(_)), "{refused:?}");
    assert!(rig.index.pack(KEY).expect("asked").is_none());
}

/// Asserts that a manifest whose records overlap, leave a gap, do not start at 0, or run on past
/// where the footer starts is refused as invalid before any read. Catches two digests sharing
/// bytes, bytes in the records that no digest names, and a record that reaches into the footer.
#[tokio::test]
async fn records_that_overlap_leave_a_gap_or_pass_the_footer_are_refused() {
    let footer = footer_len(2);
    let first = FIRST.len() as u64;
    let second = SECOND.len() as u64;
    let size = footer + first + second;

    let overlapping = listed(&[(FIRST, 0), (SECOND, first - 1)], size);
    refused_unread(overlapping, "overlaps").await;

    let gap = listed(&[(FIRST, 0), (SECOND, first + 3)], size + 3);
    refused_unread(gap, "a gap of records between 10 and 13").await;

    let late = listed(&[(FIRST, 4)], footer_len(1) + first + 4);
    refused_unread(late, "a gap of records between 0 and 4").await;

    let past = listed(&[(FIRST, 0), (SECOND, first)], size - 1);
    refused_unread(past, "the records end at 21, the footer starts at 20").await;

    let short = listed(&[(FIRST, 0), (SECOND, first)], size + 5);
    refused_unread(short, "the records end at 21, the footer starts at 26").await;
}

/// Asserts that a manifest that does not list its blobs in strictly ascending (hash, size) order
/// is refused as invalid before any read: reversed, and a blob listed twice. Catches a manifest
/// that cannot equal a footer, whose index is in that order, and a digest listed twice.
#[tokio::test]
async fn a_manifest_out_of_order_or_with_a_blob_twice_is_refused() {
    let (_, manifest) = segment_of(&[FIRST, SECOND]);
    let mut reversed = manifest.clone();
    reversed.blobs.reverse();
    refused_unread(reversed, "ascending").await;

    let mut twice = manifest;
    twice.blobs[1] = twice.blobs[0];
    refused_unread(twice, "ascending").await;
}

/// Asserts the caps: a pack over 128 MiB, one with too few bytes to hold its footer, one with no
/// blob, and one of 16,385 blobs are refused as invalid before any read. Catches a commit that
/// lets an untrusted writer have the frontend read or hold more than the format allows.
#[tokio::test]
async fn a_pack_over_the_caps_is_refused() {
    assert_eq!(MAX_UNTRUSTED_PACK_BYTES, 128 << 20);
    assert_eq!(MAX_UNTRUSTED_BLOB_BYTES, 64 << 20);
    assert_eq!(MAX_UNTRUSTED_MANIFEST_BYTES, 4 << 20);
    assert_eq!(MAX_UNTRUSTED_READ_BYTES, (128 << 20) + 33);
    let limits = UntrustedLimits::default();
    assert_eq!(limits.max_blobs, MAX_COMMIT_BLOBS);
    assert_eq!(limits.max_pack_bytes, MAX_UNTRUSTED_PACK_BYTES);
    assert_eq!(limits.read_budget, MAX_UNTRUSTED_READ_BYTES);

    let (_, one) = segment_of(&[FIRST]);
    let big = NewPack {
        size: MAX_UNTRUSTED_PACK_BYTES + 1,
        ..one.clone()
    };
    refused_unread(big, "a pack of 134217729 bytes").await;

    let small = NewPack {
        size: footer_len(1) - 1,
        ..one.clone()
    };
    refused_unread(small, &format!("a pack of {} bytes", footer_len(1) - 1)).await;

    let none = NewPack {
        blobs: Vec::new(),
        ..one.clone()
    };
    refused_unread(none, "0 blobs").await;

    let many: Vec<PackBlob> = (0..=MAX_COMMIT_BLOBS as u32)
        .map(|n| {
            let mut hash = [0_u8; 32];
            hash[..4].copy_from_slice(&n.to_be_bytes());
            PackBlob {
                digest: BlobDigest { hash, size: 0 },
                offset: 0,
            }
        })
        .collect();
    let too_many = NewPack {
        blobs: many,
        size: 1 << 20,
        ..one
    };
    refused_unread(too_many, "16385 blobs").await;
}

/// Asserts the per-record cap and the read budget: a blob over the cap and a commit that would
/// read more than the budget are refused before any read, and the same pack with a cap and a
/// budget that just fit it is recorded. Catches a cap or a budget that is off by one, or one that
/// is checked after the reads.
#[tokio::test]
async fn the_record_cap_and_the_read_budget_are_checked_before_reading() {
    let (bytes, manifest) = segment_of(&[FIRST]);
    let rig = Rig::new().await;
    rig.store.seed(KEY, &bytes).await;
    let size = bytes.len() as u64;
    let refusals = [
        UntrustedLimits {
            max_blob_bytes: FIRST.len() as u64 - 1,
            ..UntrustedLimits::default()
        },
        UntrustedLimits {
            read_budget: size + 32,
            ..UntrustedLimits::default()
        },
    ];

    for limits in refusals {
        let refused = rig
            .packs
            .commit_untrusted_pack(&issued(), manifest.clone(), &limits)
            .await;
        let refused = refused.expect_err("refused");
        assert!(matches!(refused, CommitError::Invalid(_)), "{refused:?}");
    }

    assert!(rig.store.ranges().is_empty(), "the store was not asked");
    let fits = UntrustedLimits {
        max_blob_bytes: FIRST.len() as u64,
        read_budget: size + 33,
        ..UntrustedLimits::default()
    };
    let packs = &rig.packs;
    let recorded = packs
        .commit_untrusted_pack(&issued(), manifest, &fits)
        .await;
    assert!(recorded.expect("recorded").created);
}

/// Asserts that a footer whose entry has the manifest's hash and offset but another size is not
/// the manifest: refused as invalid after its two reads, nothing recorded. Catches a comparison
/// that stops at the hash and the offset.
#[tokio::test]
async fn a_footer_entry_of_another_size_is_not_the_manifest() {
    let mut shorter = entry(FIRST, 0);
    shorter.digest.size_bytes -= 1;
    let (bytes, _) = forged(&[shorter], FIRST.as_bytes());
    let manifest = listed(&[(FIRST, 0)], bytes.len() as u64);
    let rig = Rig::new().await;
    rig.store.seed(KEY, &bytes).await;

    let refused = commit(&rig, manifest).await;

    let refused = refused.expect_err("refused");
    assert!(matches!(refused, CommitError::Invalid(_)), "{refused:?}");
    assert!(
        refused.message().contains("is not the manifest"),
        "{refused:?}"
    );
    assert_eq!(rig.store.ranges().len(), 2);
    assert!(rig.index.pack(KEY).expect("asked").is_none());
}

/// Asserts that a footer that is not the manifest is refused as invalid after its two reads and
/// before any record is read: here the footer swaps the offsets of the two records, and its
/// entry count is not the manifest's (one fewer, and the largest a trailer can say). Catches a
/// commit that indexes by the manifest what a footer-reading consumer would find elsewhere, and
/// one that reads a footer of the size an untrusted trailer claims.
#[tokio::test]
async fn a_footer_that_is_not_the_manifest_is_refused() {
    let (first, second) = (FIRST.len() as u64, SECOND.len() as u64);
    let body = format!("{FIRST}{SECOND}");
    let swapped = [entry(SECOND, 0), entry(FIRST, second)];
    let (bytes, _) = forged(&swapped, body.as_bytes());
    let manifest = listed(&[(FIRST, 0), (SECOND, first)], bytes.len() as u64);
    let rig = Rig::new().await;
    rig.store.seed(KEY, &bytes).await;

    let refused = commit(&rig, manifest).await;

    let refused = refused.expect_err("refused");
    assert!(matches!(refused, CommitError::Invalid(_)), "{refused:?}");
    assert!(
        refused.message().contains("is not the manifest"),
        "{refused:?}"
    );
    assert_eq!(rig.store.ranges().len(), 2);
    assert!(rig.index.pack(KEY).expect("asked").is_none());

    let (honest, manifest) = segment_of(&[FIRST, SECOND]);
    let counted = honest.len() - 32 + 8;
    let fewer = u32::from_le_bytes(honest[counted..counted + 4].try_into().expect("4 bytes")) - 1;
    for says in [fewer, u32::MAX] {
        let mut lying = honest.clone();
        lying[counted..counted + 4].copy_from_slice(&says.to_le_bytes());
        let rig = Rig::new().await;
        rig.store.seed(KEY, &lying).await;
        let refused = commit(&rig, manifest.clone()).await;
        let refused = refused.expect_err("refused");
        assert!(matches!(refused, CommitError::Invalid(_)), "{refused:?}");
        assert_eq!(rig.store.ranges().len(), 1, "no footer of {says} entries");
        assert!(rig.index.pack(KEY).expect("asked").is_none());
    }
}

/// Asserts that a trailer or footer that is damaged is refused as invalid, with no record read:
/// the magic, the version, the footer CRC, the index offset (so that the records end where the
/// manifest says they do not), and a byte of the fan-out that the CRC covers. Catches a commit
/// that does not read the footer, or reads it without checking it.
#[tokio::test]
async fn a_damaged_trailer_or_footer_is_refused() {
    let (honest, manifest) = segment_of(&[FIRST, SECOND]);
    let trailer = honest.len() - 32;
    let fan_out = honest.len() - footer_len(2) as usize;
    let damaged = [
        ("magic", honest.len() - 1),
        ("version", trailer + 20),
        ("footer crc", trailer + 16),
        ("index offset", trailer),
        ("fan-out", fan_out),
    ];

    for (what, at) in damaged {
        let mut lying = honest.clone();
        lying[at] ^= 1;
        let rig = Rig::new().await;
        rig.store.seed(KEY, &lying).await;

        let refused = commit(&rig, manifest.clone()).await;

        let refused = refused.expect_err(what);
        assert!(
            matches!(refused, CommitError::Invalid(_)),
            "{what}: {refused:?}"
        );
        assert!(refused.message().contains("footer"), "{what}: {refused:?}");
        assert!(rig.store.ranges().len() <= 2, "{what}: no record was read");
        assert!(rig.index.pack(KEY).expect("asked").is_none(), "{what}");
    }
}

/// Asserts that a pack the store does not hold as the manifest says is refused as missing: no
/// object, one byte too short, and one byte too long. Catches a manifest size that is taken on
/// trust.
#[tokio::test]
async fn a_pack_of_another_size_than_said_is_refused() {
    let (bytes, manifest) = segment_of(&[FIRST, SECOND]);
    let mut longer = bytes.clone();
    longer.push(0);
    let cases = [
        ("nothing", None),
        ("shorter", Some(bytes[..bytes.len() - 1].to_vec())),
        ("longer", Some(longer)),
    ];

    for (what, stored) in cases {
        let rig = Rig::new().await;
        if let Some(stored) = stored {
            rig.store.seed(KEY, &stored).await;
        }

        let refused = commit(&rig, manifest.clone()).await;

        let refused = refused.expect_err(what);
        assert!(
            matches!(refused, CommitError::Missing(_)),
            "{what}: {refused:?}"
        );
        assert!(rig.index.pack(KEY).expect("asked").is_none(), "{what}");
    }
}

/// Asserts that only the key the frontend issued for the build is committed: another nonce,
/// another build's key, a daemon's key, and a manifest that names another writer are each
/// refused as forbidden though the issued key holds a good pack, with no read; and a daemon's
/// commit of a pack named under `cas/b/` is refused as invalid. Catches a build that commits an
/// object it was not issued, and the weak path and the daemon path being taken for each other.
#[tokio::test]
async fn only_the_issued_key_is_committed_and_the_two_paths_do_not_cross() {
    let (bytes, manifest) = segment_of(&[FIRST]);
    let rig = Rig::new().await;
    rig.store.seed(KEY, &bytes).await;
    let others = [
        ("cas/b/build7/other", BUILD),
        ("cas/b/build8/n0nce1", BUILD),
        (DAEMON_KEY, BUILD),
        (KEY, "build8"),
        (KEY, "node1"),
    ];

    for (key, writer) in others {
        let given = NewPack {
            key: key.to_owned(),
            writer: writer.to_owned(),
            ..manifest.clone()
        };
        let refused = commit(&rig, given).await;
        let refused = refused.expect_err("refused");
        assert!(
            matches!(refused, CommitError::Forbidden(_)),
            "{key}: {refused:?}"
        );
    }

    assert!(rig.store.ranges().is_empty(), "the store was not asked");
    assert!(rig.index.pack(KEY).expect("asked").is_none());
    assert_eq!(
        rig.packs.metrics().commit_rejects.get(),
        others.len() as u64
    );

    let daemons = rig.packs.commit_pack(BUILD, manifest, 2).await;
    let daemons = daemons.expect_err("refused");
    assert!(matches!(daemons, CommitError::Invalid(_)), "{daemons:?}");
    assert!(rig.store.ranges().is_empty());
    assert!(rig.index.pack(KEY).expect("asked").is_none());
}

/// Asserts what an issued key can be: `cas/b/<build>/<nonce>` of two parts that are not empty and
/// hold no `/`, and make an object key. Catches a key that names another build's prefix or is
/// not one the store takes.
#[test]
fn an_issued_key_is_a_build_and_a_nonce_under_the_build_prefix() {
    let key = IssuedPackKey::new("build7", "n0nce1").expect("a key");
    assert_eq!((key.key(), key.build()), (KEY, "build7"));

    for (build, nonce) in [
        ("", "n"),
        ("b", ""),
        ("a/b", "n"),
        ("b", "n/m"),
        ("b", ".."),
        ("b c", "n"),
    ] {
        let refused = IssuedPackKey::new(build, nonce).expect_err("refused");
        assert!(
            matches!(refused, CommitError::Invalid(_)),
            "{build}/{nonce}: {refused:?}"
        );
    }
}

/// Asserts the replay of a key: the same pack committed again is answered `created: false`, and
/// the key committed with another pack (the writer replaced the object and reported its new
/// manifest, which the new object satisfies) is a conflict that records nothing new. Catches a
/// key whose recorded blobs can be changed after the commit.
#[tokio::test]
async fn a_replay_with_other_contents_is_a_conflict() {
    let (bytes, manifest) = segment_of(&[FIRST, SECOND, "third blob"]);
    let (other, others) = segment_of(&[FIRST, SECOND, "fourth blob"]);
    let rig = Rig::replacing().await;
    rig.store.seed(KEY, &bytes).await;
    commit(&rig, manifest.clone()).await.expect("committed");
    let same = commit(&rig, manifest).await.expect("replayed");
    assert!(!same.created);

    rig.store.seed(KEY, &other).await;
    let conflict = commit(&rig, others).await;

    let conflict = conflict.expect_err("refused");
    assert!(matches!(conflict, CommitError::Conflict(_)), "{conflict:?}");
    assert_eq!(rig.packs.metrics().commits.get(), 1);
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 1);
}

/// Asserts that a store that fails is not the commit's fault: the commit is unavailable, nothing
/// is recorded, the refusal counter does not move, and the same commit sent again is recorded.
/// Catches a store failure reported as a refusal of the pack.
#[tokio::test]
async fn a_store_that_fails_makes_the_commit_unavailable() {
    let (bytes, manifest) = segment_of(&[FIRST]);
    let rig = Rig::new().await;
    rig.store.seed(KEY, &bytes).await;
    rig.store.reject_reads(1);

    let failed = commit(&rig, manifest.clone()).await;

    let failed = failed.expect_err("the store fails");
    assert!(matches!(failed, CommitError::Unavailable(_)), "{failed:?}");
    assert!(rig.index.pack(KEY).expect("asked").is_none());
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 0);
    let recorded = commit(&rig, manifest).await;
    assert!(recorded.expect("recorded").created);
}

/// Asserts that a store that fails while a record is read, after the trailer and the footer were
/// read, is not the commit's fault either: the commit is unavailable, nothing is recorded and the
/// refusal counter does not move. Catches a store failure on a record reported as a mismatch of
/// the pack.
#[tokio::test]
async fn a_store_that_fails_on_a_record_makes_the_commit_unavailable() {
    let (bytes, manifest) = segment_of(&[FIRST]);
    let rig = Rig::new().await;
    rig.store.seed(KEY, &bytes).await;
    rig.store.reject_read_after(2);

    let failed = commit(&rig, manifest.clone()).await;

    let failed = failed.expect_err("the store fails on the record");
    assert!(matches!(failed, CommitError::Unavailable(_)), "{failed:?}");
    assert_eq!(
        rig.store.ranges().len(),
        3,
        "the trailer, the footer, the record"
    );
    assert!(rig.index.pack(KEY).expect("asked").is_none());
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 0);
    let recorded = commit(&rig, manifest).await;
    assert!(recorded.expect("recorded").created);
}

/// A manifest of a pack of `size` bytes that lists `blobs` as they are (digest and offset), sorted
/// into the order a manifest must list them.
fn raw(blobs: Vec<PackBlob>, size: u64) -> NewPack {
    let mut manifest = manifest_of(&[], size);
    manifest.blobs = blobs;
    manifest
        .blobs
        .sort_by_key(|blob| (blob.digest.hash, blob.digest.size));
    manifest
}

/// A blob of `size` bytes named by `hash` at `offset`.
fn at(hash: u8, size: u64, offset: u64) -> PackBlob {
    PackBlob {
        digest: BlobDigest {
            hash: [hash; 32],
            size,
        },
        offset,
    }
}

/// Asserts that a record of no bytes is refused as invalid before any read, whether it is the
/// empty blob's own digest or not, at offset 0 or after a record. An untrusted pack may not hold
/// a record of no bytes (the frontend's own packs may): offset 0 would be ambiguous and many
/// digests could share one offset. The empty blob needs no record, its digest is known. Catches a manifest that passes the layout check with a record that holds nothing.
#[tokio::test]
async fn a_zero_size_record_is_refused() {
    let first = FIRST.len() as u64;
    let body = footer_len(2) + first;

    let leading = listed(&[("", 0), (FIRST, 0)], body);
    refused_unread(leading, "a record of no bytes").await;

    let trailing = listed(&[(FIRST, 0), ("", first)], body);
    refused_unread(trailing, "a record of no bytes").await;

    let nameless = raw(vec![at(1, 0, 0), at(2, first, 0)], body);
    refused_unread(nameless, "a record of no bytes").await;

    let alone = listed(&[("", 0)], footer_len(1));
    refused_unread(alone, "a record of no bytes").await;
}

/// Asserts that two records of no bytes at one offset are refused as invalid before any read,
/// where the layout check alone would take them (each ends where the next starts). Catches any
/// number of records of no bytes passing as a layout, which would put many digests at offset 0.
#[tokio::test]
async fn two_zero_size_records_at_one_offset_are_refused() {
    let first = FIRST.len() as u64;
    let size = footer_len(3) + first;
    let two = raw(vec![at(1, 0, 0), at(2, 0, 0), at(3, first, 0)], size);
    refused_unread(two, "a record of no bytes").await;

    let after = raw(
        vec![at(1, first, 0), at(2, 0, first), at(3, 0, first)],
        size,
    );
    refused_unread(after, "a record of no bytes").await;
}

/// Asserts that every refusal of an untrusted commit is counted under the reason that names it,
/// and nothing else is: one commit for each of forbidden (a key not issued), shape (records with
/// a gap), missing (no object), footer (a footer that is not the manifest), mismatch (a record
/// whose CRC is not its bytes) and conflict (a key recorded with other contents) moves its own
/// label by one; a store that fails (unavailable) moves none; the pack that is recorded is
/// counted as recorded, and its replay is not counted again; the daemons' shared counters still
/// move for each refusal. Catches a reason counted under another, a refusal not counted, a
/// failure of the store counted as a refusal, and a scrape that does not carry the labels.
#[tokio::test]
async fn an_untrusted_commit_counts_by_reason() {
    let rig = Rig::replacing().await;
    let scrape = || {
        let mut text = String::new();
        rig.packs.render_metrics(&mut text);
        text
    };
    let reason = |text: &str, label: &str, n: u64| {
        let line = format!("kbf_pack_untrusted_commit_rejects_total{{reason=\"{label}\"}} {n}\n");
        assert!(text.contains(&line), "{line}in {text}");
    };
    let (honest, manifest) = segment_of(&[FIRST, SECOND]);
    let before = scrape();
    assert!(
        before.contains("kbf_pack_untrusted_commits_total 0\n"),
        "{before}"
    );
    for label in [
        "forbidden",
        "shape",
        "footer",
        "mismatch",
        "missing",
        "conflict",
    ] {
        reason(&before, label, 0);
    }

    let other = NewPack {
        key: "cas/b/build7/other".to_owned(),
        ..manifest.clone()
    };
    commit(&rig, other).await.expect_err("forbidden");
    reason(&scrape(), "forbidden", 1);

    let first = FIRST.len() as u64;
    let gap = listed(&[(FIRST, 0), (SECOND, first + 3)], honest.len() as u64 + 3);
    commit(&rig, gap).await.expect_err("shape");
    reason(&scrape(), "shape", 1);

    commit(&rig, manifest.clone()).await.expect_err("missing");
    reason(&scrape(), "missing", 1);

    let body = format!("{FIRST}{SECOND}");
    let swapped = [entry(SECOND, 0), entry(FIRST, SECOND.len() as u64)];
    let (lying, _) = forged(&swapped, body.as_bytes());
    rig.store.seed(KEY, &lying).await;
    let footer = listed(&[(FIRST, 0), (SECOND, first)], lying.len() as u64);
    commit(&rig, footer).await.expect_err("footer");
    reason(&scrape(), "footer", 1);

    let mut wrong = entry(FIRST, 0);
    wrong.crc32c ^= 1;
    let (crooked, crooked_manifest) = forged(&[wrong], FIRST.as_bytes());
    rig.store.seed(KEY, &crooked).await;
    commit(&rig, crooked_manifest).await.expect_err("mismatch");
    reason(&scrape(), "mismatch", 1);

    rig.store.seed(KEY, &honest).await;
    rig.store.reject_reads(1);
    let failed = commit(&rig, manifest.clone())
        .await
        .expect_err("unavailable");
    assert!(matches!(failed, CommitError::Unavailable(_)), "{failed:?}");
    let text = scrape();
    assert!(
        text.contains("kbf_pack_untrusted_commits_total 0\n"),
        "{text}"
    );
    for (label, n) in [("forbidden", 1), ("shape", 1), ("footer", 1)] {
        reason(&text, label, n);
    }
    for (label, n) in [("mismatch", 1), ("missing", 1), ("conflict", 0)] {
        reason(&text, label, n);
    }

    commit(&rig, manifest.clone()).await.expect("recorded");
    commit(&rig, manifest).await.expect("replayed");
    let (changed, changed_manifest) = segment_of(&[FIRST, "third blob"]);
    rig.store.seed(KEY, &changed).await;
    commit(&rig, changed_manifest).await.expect_err("conflict");

    let text = scrape();
    assert!(
        text.contains("kbf_pack_untrusted_commits_total 1\n"),
        "{text}"
    );
    for (label, n) in [("forbidden", 1), ("shape", 1), ("footer", 1)] {
        reason(&text, label, n);
    }
    for (label, n) in [("mismatch", 1), ("missing", 1), ("conflict", 1)] {
        reason(&text, label, n);
    }
    assert!(text.contains("kbf_pack_commit_rejects_total 6\n"), "{text}");
    assert!(text.contains("kbf_pack_commits_total 1\n"), "{text}");
}
