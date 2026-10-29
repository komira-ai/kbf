//! `Packs::commit_pack`: a pack that a daemon stored in the object store is recorded in the pack
//! index, after the frontend has checked the pack against the store.
//!
//! Every test says the behaviour it asserts.

use bytes::Bytes;
use kbf_front::{CommitError, MAX_COMMIT_BLOBS};
use kbf_pack::PackerConfig;
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex as _};
use kbf_types::Digest;

use crate::support::{Rig, blob, index_form};

const KEY: &str = "cas/d/node1/boot1/0000000000000000";

/// The bytes of a pack of `blobs` back to back and a few bytes of footer, and the commit that
/// names it, as the daemon `node1` stored it under `key`.
fn pack_of(key: &str, blobs: &[&(Digest, Bytes)]) -> (Vec<u8>, NewPack) {
    let mut body = Vec::new();
    let mut listed = Vec::new();
    for (digest, bytes) in blobs {
        let offset = body.len() as u64;
        listed.push(PackBlob {
            digest: index_form(digest),
            offset,
        });
        body.extend_from_slice(bytes);
    }
    body.extend_from_slice(b"footer");
    let commit = NewPack {
        key: key.to_owned(),
        writer: "node1".to_owned(),
        size: body.len() as u64,
        blobs: listed,
    };
    (body, commit)
}

/// A rig whose store holds the pack of `blobs` under `key`, and the commit that names it.
async fn stored(key: &str, blobs: &[&(Digest, Bytes)]) -> (Rig, NewPack) {
    let rig = Rig::new().await;
    let (body, commit) = pack_of(key, blobs);
    rig.store.seed(key, &body).await;
    (rig, commit)
}

/// The ranges read from the store by committing, with `sample`, the pack of `blobs` stored under
/// `key`.
async fn ranges_read(
    key: &str,
    blobs: &[&(Digest, Bytes)],
    sample: usize,
) -> Vec<(String, u64, u64)> {
    let (rig, commit) = stored(key, blobs).await;
    let committed = rig.packs.commit_pack("node1", commit, sample).await;
    committed.expect("committed");
    rig.store.ranges()
}

/// Asserts that a stored pack is recorded: its blobs are located at their offsets in the pack,
/// the index records the writer as the daemon, the commit is counted, and the same commit sent
/// again is answered as the first was, with nothing created and nothing counted twice.
#[tokio::test]
async fn a_stored_pack_is_recorded_and_a_retry_changes_nothing() {
    let (one, two) = (blob("first blob"), blob("second blob"));
    let (rig, commit) = stored(KEY, &[&one, &two]).await;

    let first = rig.packs.commit_pack("node1", commit.clone(), 2).await;
    let first = first.expect("committed");

    assert_eq!((first.created, first.indexed), (true, 2));
    let places = rig.index.where_is(&[one.0, two.0]);
    let offsets: Vec<u64> = places.iter().flatten().map(|p| p.offset).collect();
    assert_eq!(offsets, [0, one.1.len() as u64]);
    assert!(places.iter().flatten().all(|place| place.pack_key == KEY));
    let info = rig.index.pack(KEY).expect("asked").expect("a row");
    assert_eq!(info.writer, "d/node1");
    assert_eq!(rig.packs.metrics().commits.get(), 1);

    let again = rig.packs.commit_pack("node1", commit, 2).await;
    let again = again.expect("retried");
    assert_eq!((again.created, again.indexed), (false, 2));
    assert_eq!(rig.packs.metrics().commits.get(), 1);
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 0);
}

/// Asserts the sample: the last byte of the pack and the byte after it are asked for, then
/// `sample` blobs, each once; all of them when the sample is as large as the list, and none when
/// it is 0. The same commit is checked at the same blobs by every frontend, which is what a test
/// of the sample can rest on.
#[tokio::test]
async fn the_sample_is_a_fixed_set_of_distinct_blobs() {
    let blobs: Vec<_> = (0..5).map(|n| blob(&format!("blob number {n}"))).collect();
    let all: Vec<&(Digest, Bytes)> = blobs.iter().collect();
    let size = pack_of(KEY, &all).1.size;
    let length = blobs[0].1.len() as u64;
    let starts: Vec<u64> = (0..5).map(|n| n * length).collect();

    let two = ranges_read(KEY, &all, 2).await;
    assert_eq!(two.len(), 3, "the last byte and two blobs: {two:?}");
    assert_eq!(two[0], (KEY.to_owned(), size - 1, 2));
    assert_eq!(two, ranges_read(KEY, &all, 2).await);

    let none = ranges_read(KEY, &all, 0).await;
    assert_eq!(none, [(KEY.to_owned(), size - 1, 2)]);

    for sample in [5, 99] {
        let every = ranges_read(KEY, &all, sample).await;
        let mut offsets: Vec<u64> = every.iter().skip(1).map(|(_, at, _)| *at).collect();
        offsets.sort_unstable();
        assert_eq!(offsets, starts, "sample {sample}");
        assert!(every.iter().skip(1).all(|(_, _, len)| *len == length));
    }
}

/// Asserts that a sampled blob whose bytes are not its digest refuses the commit as a mismatch,
/// recording nothing and counting the refusal; and that the same commit with a sample of 0,
/// which checks no blob, is recorded: the bytes it did not look at are caught at the first read.
#[tokio::test]
async fn a_sampled_blob_that_is_not_its_digest_refuses_the_commit() {
    let (one, two) = (blob("first blob"), blob("second blob"));
    let (mut body, commit) = pack_of(KEY, &[&one, &two]);
    body[0] ^= 1;
    let rig = Rig::new().await;
    rig.store.seed(KEY, &body).await;

    let refused = rig.packs.commit_pack("node1", commit.clone(), 2).await;

    let refused = refused.expect_err("refused");
    assert!(matches!(refused, CommitError::Mismatch(_)), "{refused:?}");
    let said = refused.message();
    assert!(said.contains("holds other bytes"), "{said}");
    assert!(rig.index.has_none(&[one.0, two.0]));
    assert!(rig.index.pack(KEY).expect("asked").is_none());
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 1);
    assert_eq!(rig.packs.metrics().commits.get(), 0);

    let trusted = rig.packs.commit_pack("node1", commit, 0).await;
    assert!(trusted.expect("recorded unchecked").created);
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 1);
}

/// Asserts that a pack the store does not hold as the commit says is refused as missing, whether
/// nothing is stored under its name, the object is shorter than the commit says (its last byte is
/// not there) or it is longer (a byte follows the last): nothing is recorded and each refusal is
/// counted.
#[tokio::test]
async fn a_pack_the_store_does_not_hold_is_refused() {
    let one = blob("first blob");
    let (body, commit) = pack_of(KEY, &[&one]);
    let rig = Rig::new().await;

    let nothing = rig.packs.commit_pack("node1", commit.clone(), 2).await;
    let nothing = nothing.expect_err("refused");
    assert!(matches!(nothing, CommitError::Missing(_)), "{nothing:?}");

    rig.store.seed(KEY, &body[..body.len() - 2]).await;
    let short = rig.packs.commit_pack("node1", commit.clone(), 2).await;
    let short = short.expect_err("refused");
    assert!(matches!(short, CommitError::Missing(_)), "{short:?}");
    assert!(rig.index.has_none(&[one.0]));
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 2);

    let other = "cas/d/node1/boot1/0000000000000001";
    let mut long = body.clone();
    long.push(b'!');
    rig.store.seed(other, &long).await;
    let named = NewPack {
        key: other.to_owned(),
        ..commit
    };
    let longer = rig.packs.commit_pack("node1", named, 2).await;
    let longer = longer.expect_err("refused");
    assert!(matches!(longer, CommitError::Missing(_)), "{longer:?}");
    assert!(longer.message().contains("longer"), "{longer:?}");
    assert!(rig.index.has_none(&[one.0]));
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 3);
}

/// Asserts that a pack named outside `cas/d/<writer>/<boot>/<sequence>` is refused as invalid,
/// whichever way it is wrong, before the store is asked anything: another kind of writer, another
/// node's prefix, too few or too many parts, an empty or a dotted part, a character a key cannot
/// hold.
#[tokio::test]
async fn a_pack_named_outside_the_writers_prefix_is_refused() {
    let one = blob("first blob");
    let (body, commit) = pack_of(KEY, &[&one]);
    let rig = Rig::new().await;
    rig.store.seed(KEY, &body).await;
    let wrong = [
        "cas/f/node1/boot1/0000000000000000",
        "cas/d/node2/boot1/0000000000000000",
        "cas/d/node1/boot1",
        "cas/d/node1/boot1/under/0000000000000000",
        "cas/d/node1//0000000000000000",
        "cas/d/node1/../0000000000000000",
        "cas/d/node1/bo ot/0000000000000000",
        "",
    ];

    for key in wrong {
        let named = NewPack {
            key: key.to_owned(),
            ..commit.clone()
        };
        let refused = rig.packs.commit_pack("node1", named, 2).await;
        let refused = refused.expect_err("refused");
        assert!(
            matches!(refused, CommitError::Invalid(_)),
            "{key}: {refused:?}"
        );
    }

    assert!(rig.store.ranges().is_empty(), "the store was not asked");
    let rejected = rig.packs.metrics().commit_rejects.get();
    assert_eq!(rejected, wrong.len() as u64);
}

/// Asserts that a writer that is not the caller is refused as forbidden, though the pack is
/// stored and well named: a daemon commits only its own packs. Nothing is recorded and the
/// store is not asked.
#[tokio::test]
async fn a_writer_that_is_not_the_caller_is_refused() {
    let one = blob("first blob");
    let (rig, commit) = stored(KEY, &[&one]).await;

    let refused = rig.packs.commit_pack("node2", commit, 2).await;

    let refused = refused.expect_err("refused");
    assert!(matches!(refused, CommitError::Forbidden(_)), "{refused:?}");
    assert!(refused.message().contains("node2"), "{refused:?}");
    assert!(rig.index.has_none(&[one.0]));
    assert!(rig.store.ranges().is_empty());
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 1);
}

/// Asserts the bounds on what a commit lists: the most blobs is what the packer cuts a pack at,
/// and one more is invalid; a pack of no bytes, a blob that reaches past the end of the pack, a
/// blob whose end does not fit a number, and blobs whose sizes together exceed the pack (or do not
/// fit a number) are invalid. The store is not asked for any of them.
#[tokio::test]
async fn a_commit_that_cannot_be_a_pack_is_refused() {
    assert_eq!(MAX_COMMIT_BLOBS, PackerConfig::default().max_blobs);
    let rig = Rig::new().await;
    let at = |offset: u64, size: u64| PackBlob {
        digest: BlobDigest {
            hash: [1; 32],
            size,
        },
        offset,
    };
    let commit = |size: u64, blobs: Vec<PackBlob>| NewPack {
        key: KEY.to_owned(),
        writer: "node1".to_owned(),
        size,
        blobs,
    };
    let cases = [
        (
            commit(100, vec![at(0, 0); MAX_COMMIT_BLOBS + 1]),
            "16385 blobs",
        ),
        (commit(0, Vec::new()), "is empty"),
        (commit(10, vec![at(8, 5)]), "past the end"),
        (commit(10, vec![at(u64::MAX, 5)]), "past the end"),
        (commit(10, vec![at(0, 6), at(4, 6)]), "add up to more"),
        (commit(u64::MAX, vec![at(0, 1 << 63); 2]), "add up to more"),
    ];

    for (given, said) in cases {
        let refused = rig.packs.commit_pack("node1", given, 2).await;
        let refused = refused.expect_err("refused");
        assert!(matches!(refused, CommitError::Invalid(_)), "{refused:?}");
        assert!(refused.message().contains(said), "{said}: {refused:?}");
    }

    assert!(rig.store.ranges().is_empty());
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 6);
}

/// Asserts what the index adds: a list that names a digest twice is refused as invalid and a pack
/// recorded before with other contents is refused as a conflict, each recording nothing new and
/// counted.
#[tokio::test]
async fn the_index_refuses_a_list_it_cannot_hold_and_a_pack_it_has_with_other_contents() {
    let (one, two) = (blob("first blob"), blob("second blob"));
    let (rig, commit) = stored(KEY, &[&one, &two]).await;
    let twice = NewPack {
        blobs: vec![commit.blobs[0], commit.blobs[0]],
        ..commit.clone()
    };

    let refused = rig.packs.commit_pack("node1", twice, 2).await;
    let refused = refused.expect_err("refused");
    assert!(matches!(refused, CommitError::Invalid(_)), "{refused:?}");
    assert!(rig.index.has_none(&[one.0, two.0]));

    let recorded = rig.packs.commit_pack("node1", commit.clone(), 2).await;
    recorded.expect("recorded");
    let other = NewPack {
        blobs: vec![commit.blobs[0]],
        ..commit
    };
    let conflict = rig.packs.commit_pack("node1", other, 2).await;
    let conflict = conflict.expect_err("refused");
    assert!(matches!(conflict, CommitError::Conflict(_)), "{conflict:?}");
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 2);
    assert_eq!(rig.packs.metrics().commits.get(), 1);
}

/// Asserts that a store or an index that fails is not the commit's fault: the commit is
/// unavailable, nothing is recorded, the refusal counter does not move, and the same commit sent
/// again once they answer is recorded.
#[tokio::test]
async fn a_store_or_an_index_that_fails_makes_the_commit_unavailable() {
    let one = blob("first blob");
    let (rig, commit) = stored(KEY, &[&one]).await;

    rig.store.reject_reads(1);
    let store = rig.packs.commit_pack("node1", commit.clone(), 2).await;
    let store = store.expect_err("the store fails");
    assert!(matches!(store, CommitError::Unavailable(_)), "{store:?}");
    assert!(store.message().contains("reading"), "{store:?}");

    rig.index.fail_record(1);
    let index = rig.packs.commit_pack("node1", commit.clone(), 2).await;
    let index = index.expect_err("the index fails");
    assert!(matches!(index, CommitError::Unavailable(_)), "{index:?}");
    assert!(rig.index.has_none(&[one.0]));
    assert_eq!(rig.packs.metrics().commit_rejects.get(), 0);

    let recorded = rig.packs.commit_pack("node1", commit, 2).await;
    assert!(recorded.expect("recorded").created);
}

/// Asserts how an empty blob is checked: it is read from no range, and its digest must be that of
/// no bytes. One listed with the digest of the empty blob is recorded; one listed with another
/// hash is a mismatch.
#[tokio::test]
async fn an_empty_blob_is_checked_without_a_read() {
    let one = blob("first blob");
    let (body, mut commit) = pack_of(KEY, &[&one]);
    let rig = Rig::new().await;
    rig.store.seed(KEY, &body).await;
    let empty = blob("");
    commit.blobs.push(PackBlob {
        digest: index_form(&empty.0),
        offset: 0,
    });
    let first = rig.packs.commit_pack("node1", commit.clone(), 2).await;
    assert!(first.expect("recorded").created);
    let reads = rig.store.ranges().len();
    assert_eq!(reads, 2, "the last byte and the first blob");

    let other = "cas/d/node1/boot1/0000000000000001";
    rig.store.seed(other, &body).await;
    commit.key = other.to_owned();
    commit.blobs[1].digest.hash = [9; 32];
    let lying = rig.packs.commit_pack("node1", commit, 2).await;
    let lying = lying.expect_err("refused");
    assert!(matches!(lying, CommitError::Mismatch(_)), "{lying:?}");
}

/// Asserts that a sampled blob is read in ranges of at most 8 MiB: a blob of 8 MiB and 5 bytes is
/// read as a range of 8 MiB and a range of 5 bytes and is recorded, and the same blob with its
/// last byte changed is refused as a mismatch.
#[tokio::test]
async fn a_large_blob_is_checked_in_ranges_of_at_most_eight_mebibytes() {
    let chunk: u64 = 8 * 1024 * 1024;
    let large = blob(&"a".repeat(chunk as usize + 5));
    let (mut body, commit) = pack_of(KEY, &[&large]);
    let rig = Rig::new().await;
    rig.store.seed(KEY, &body).await;

    let first = rig.packs.commit_pack("node1", commit.clone(), 1).await;

    assert!(first.expect("recorded").created);
    let reads = rig.store.ranges();
    let whole = (KEY.to_owned(), 0, chunk);
    let rest = (KEY.to_owned(), chunk, 5);
    assert_eq!(reads[1..], [whole, rest]);

    let other = "cas/d/node1/boot1/0000000000000001";
    body[chunk as usize + 4] = b'b';
    rig.store.seed(other, &body).await;
    let named = NewPack {
        key: other.to_owned(),
        ..commit
    };
    let changed = rig.packs.commit_pack("node1", named, 1).await;
    let changed = changed.expect_err("refused");
    assert!(matches!(changed, CommitError::Mismatch(_)), "{changed:?}");
}

/// Asserts that the counters are on the scrape: the commits that were recorded and the ones that
/// were refused, each under its family.
#[tokio::test]
async fn the_commits_are_counted_on_the_scrape() {
    let one = blob("first blob");
    let (rig, commit) = stored(KEY, &[&one]).await;
    let recorded = rig.packs.commit_pack("node1", commit.clone(), 2).await;
    recorded.expect("recorded");
    let refused = rig.packs.commit_pack("node2", commit, 2).await;
    refused.expect_err("refused");

    let mut text = String::new();
    rig.packs.render_metrics(&mut text);

    assert!(text.contains("kbf_pack_commits_total 1\n"), "{text}");
    assert!(text.contains("kbf_pack_commit_rejects_total 1\n"), "{text}");
}
