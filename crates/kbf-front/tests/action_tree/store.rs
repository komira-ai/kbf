//! Where the writer finds blobs, how it writes the ones that are nowhere, and what a store that
//! fails or does not answer makes of a tree: `why=store`, by name.

use kbf_front::action_tree::{HEADS_IN_FLIGHT, LOCATE_CHUNK};

use super::*;

/// The input root holding `files`, each a plain file named by its position.
async fn root_of(rig: &Rig, files: &[Digest]) -> Digest {
    let nodes = files
        .iter()
        .enumerate()
        .map(|(at, digest)| file(&format!("f{at}"), digest, false))
        .collect();
    rig.store(&directory(nodes, vec![], vec![])).await
}

/// Catches: a blob with no location named in a tree before it is in `cas/l/`, written with
/// other bytes, or named with a `source`: it is read from the CAS, written whole to
/// `cas/l/<sha256>` before any manifest is written, and named as that whole object.
#[tokio::test]
async fn an_unlocated_blob_is_written_to_cas_l_before_any_manifest_names_it() {
    let rig = Rig::new().await;
    let bytes = Bytes::from_static(b"only in a storage node's write tier");
    let blob = rig.farm.cas(bytes.clone()).await;
    let root = root_of(&rig, &[blob]).await;
    let written = rig.write(&root).await.expect("a tree");
    let key = whole_key(&blob.hash_hex());
    assert_eq!(rig.farm.object(&key), Some(bytes));
    assert_eq!(
        rig.farm.puts().first(),
        Some(&key),
        "the blob is written first"
    );
    let entries = rig.farm.flatten(&written).expect("a tree");
    let f0 = entries.iter().find(|e| &*e.name == "f0").expect("f0");
    assert!(matches!(
        f0.kind,
        tierstore::EntryKind::File { source: None, .. }
    ));
}

/// Catches: bytes that do not hash to their digest written to the blob's content-bound key, or
/// a store sent the whole of them before the check: nothing is stored, the store received less
/// than the blob before the body ended in an error, no manifest is written, and the answer is
/// `why=store`.
#[tokio::test]
async fn a_blob_whose_bytes_do_not_hash_is_not_written() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("the blob's own bytes").await;
    rig.farm
        .state()
        .wrong
        .insert(blob, Bytes::from_static(b"some other bytes!!!!"));
    let root = root_of(&rig, &[blob]).await;
    let refused = rig.write(&root).await.expect_err("no tree");
    assert_eq!(refused, NoTree::Mismatch { digest: blob });
    assert_eq!(refused.why(), "store");
    let key = whole_key(&blob.hash_hex());
    assert_eq!(rig.farm.object(&key), None);
    let received = rig.farm.state().received.get(&key).copied();
    assert!(
        received.is_some_and(|got| got < blob.size_bytes),
        "{received:?}"
    );
    assert_eq!(rig.farm.puts(), [key], "no manifest is written");
}

/// Catches: a blob that cannot be read, or whose read fails part way, taken for written: it is
/// `why=store` naming the blob, and nothing is stored under its key.
#[tokio::test]
async fn a_blob_that_cannot_be_read_is_not_written() {
    for broken in [false, true] {
        let rig = Rig::new().await;
        let blob = rig.farm.cas("a blob whose read fails").await;
        {
            let mut state = rig.farm.state();
            if broken {
                state.faults.read_broken = Some(blob);
            } else {
                state.faults.read = Some(blob);
            }
        }
        let root = root_of(&rig, &[blob]).await;
        let refused = rig.write(&root).await.expect_err("no tree");
        assert!(
            matches!(&refused, NoTree::Read { digest, .. } if *digest == blob),
            "{refused:?}"
        );
        assert_eq!(refused.why(), "store");
        assert_eq!(rig.farm.object(&whole_key(&blob.hash_hex())), None);
    }
}

/// Catches: a create-only write that finds its key taken treated as a failure: the key is
/// content-bound, so a blob written by someone between the presence check and the write, and
/// manifests written by an earlier tree, are success.
#[tokio::test]
async fn a_key_already_taken_is_success() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("written by another frontend meanwhile").await;
    let key = whole_key(&blob.hash_hex());
    rig.farm.state().late.insert(key.clone());
    let root = root_of(&rig, &[blob]).await;
    let first = rig.write(&root).await.expect("a tree");
    assert!(rig.farm.puts().contains(&key));
    // Every manifest of the second write's tree is already stored.
    rig.writer.forget(&root);
    rig.farm.clear_records();
    let second = rig.write(&root).await.expect("a tree");
    assert_eq!(second, first);
    assert!(
        !rig.farm.puts().is_empty(),
        "the manifests were written again"
    );
}

/// Catches: a refused write taken for written: a blob's and an input manifest's each make no
/// tree, `why=store`, naming the key; and a directory is remembered only once every directory
/// manifest of its tree was stored.
#[tokio::test]
async fn a_refused_write_names_no_tree() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("a blob the store will not take").await;
    let packed = rig.farm.packed("packed", "cas/f/w/p0", 0).await;
    let sub = rig
        .store(&directory(vec![file("p", &packed, false)], vec![], vec![]))
        .await;
    let root = rig
        .store(&directory(
            vec![file("b", &blob, false)],
            vec![subdir("sub", &sub)],
            vec![],
        ))
        .await;
    let blob_key = whole_key(&blob.hash_hex());
    rig.farm.state().faults.put = Some(blob_key.clone());
    let refused = rig.write(&root).await.expect_err("no tree");
    assert!(
        matches!(&refused, NoTree::Put { key, .. } if *key == blob_key),
        "{refused:?}"
    );
    assert_eq!(refused.why(), "store");
    assert_eq!(rig.farm.puts(), [blob_key], "no manifest is written");
    let sub_manifest = format!(
        r#"{{"v":1,"children":[{{"name":"p","kind":"file","sha256":"{}","size":6,"exec":false,"source":{{"key":"cas/f/w/p0","offset":0}}}}]}}"#,
        packed.hash_hex()
    );
    let sub_hex = TreeDigest::of(sub_manifest.as_bytes()).to_string();
    let work_manifest = format!(
        r#"{{"v":1,"children":[{{"name":"b","kind":"file","sha256":"{}","size":{},"exec":false}},{{"name":"sub","kind":"dir","sha256":"{sub_hex}","size":{}}}]}}"#,
        blob.hash_hex(),
        blob.size_bytes,
        sub_manifest.len()
    );
    let work_key = whole_key(&TreeDigest::of(work_manifest.as_bytes()).to_string());
    rig.farm.state().faults.put = Some(work_key.clone());
    let refused = rig.write(&root).await.expect_err("no tree");
    assert!(
        matches!(&refused, NoTree::Put { key, .. } if *key == work_key),
        "{refused:?}"
    );
    assert_eq!(rig.farm.puts_of(&sub_hex), 1);
    rig.farm.state().faults.put = None;
    rig.write(&root).await.expect("a tree");
    assert_eq!(
        rig.farm.puts_of(&sub_hex),
        2,
        "the subdirectory was not remembered, so it is built and written again"
    );
}

/// Catches: an object of another length at a blob's `cas/l/` key taken for the blob (a tree
/// would read wrong bytes) or written over: it is `why=store`, naming the length found.
#[tokio::test]
async fn an_object_of_another_length_at_the_key_is_store() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("seven b").await;
    rig.farm
        .state()
        .sized
        .insert(whole_key(&blob.hash_hex()), blob.size_bytes + 1);
    let root = root_of(&rig, &[blob]).await;
    let refused = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        refused,
        NoTree::WrongLength {
            digest: blob,
            length: blob.size_bytes + 1
        }
    );
    assert_eq!(refused.why(), "store");
    assert!(rig.farm.puts().is_empty());
}

/// Catches: a pack span of another length than the blob named as its source, and a blob the
/// index gave no answer for taken as located: both are looked for in `cas/l/`, and written
/// there when they are not.
#[tokio::test]
async fn a_span_of_another_length_or_no_answer_is_not_a_location() {
    let rig = Rig::new().await;
    let odd = rig.farm.cas("a span that is too short").await;
    rig.farm.state().spans.insert(
        odd,
        Span {
            key: "cas/f/w/p0".to_owned(),
            offset: 0,
            length: odd.size_bytes - 1,
        },
    );
    let a = rig.farm.packed("first packed blob", "cas/f/w/p0", 0).await;
    let b = rig
        .farm
        .packed("second packed blob", "cas/f/w/p0", 64)
        .await;
    rig.farm.state().faults.index_short = true;
    let root = root_of(&rig, &[odd, a, b]).await;
    let action = rig.action(&root).await;
    rig.write(&root).await.expect("a tree");
    let mut asked: Vec<Digest> = vec![odd, a, b, action, rig.command];
    asked.sort();
    let unanswered = *asked.last().expect("a digest");
    let state = rig.farm.state();
    assert!(state.heads.contains(&odd));
    assert!(state.heads.contains(&unanswered));
    assert!(state.objects.contains_key(&whole_key(&odd.hash_hex())));
    assert!(
        state
            .objects
            .contains_key(&whole_key(&unanswered.hash_hex()))
    );
}

/// Catches: a pack index or a presence check that fails taken as "nowhere" (every blob would
/// be written again) or as located: either is `why=store`, and nothing is written.
#[tokio::test]
async fn a_failed_index_or_presence_check_is_store() {
    for index in [true, false] {
        let rig = Rig::new().await;
        let blob = rig.farm.cas("unpacked").await;
        {
            let mut state = rig.farm.state();
            if index {
                state.faults.index = Fault::Fail;
            } else {
                state.faults.head = Fault::Fail;
            }
        }
        let root = root_of(&rig, &[blob]).await;
        let refused = rig.write(&root).await.expect_err("no tree");
        assert!(matches!(refused, NoTree::Lookup { .. }), "{refused:?}");
        assert_eq!(refused.why(), "store");
        assert!(rig.farm.puts().is_empty());
    }
}

/// Catches: a writer that waits for ever on an index or a presence check that never answers:
/// at the lookups' deadline (on tokio's paused clock) it is `why=store`, and nothing is
/// written.
#[tokio::test(start_paused = true)]
async fn lookups_that_never_answer_end_at_the_deadline() {
    for index in [true, false] {
        let rig = Rig::new().await;
        let blob = rig.farm.cas("unpacked").await;
        {
            let mut state = rig.farm.state();
            if index {
                state.faults.index = Fault::Hang;
            } else {
                state.faults.head = Fault::Hang;
            }
        }
        let root = root_of(&rig, &[blob]).await;
        let refused = rig.write(&root).await.expect_err("no tree");
        assert_eq!(refused, NoTree::Deadline);
        assert_eq!(refused.why(), "store");
        assert!(rig.farm.puts().is_empty());
    }
}

/// A tree of `packed` blobs in the pack index and `large` blobs held whole in `cas/l/`, all in
/// one directory: every blob is located, by asks of at most [`LOCATE_CHUNK`] digests and one
/// presence check per large blob, and nothing is read or written but manifests.
async fn located_in_chunks(packed: usize, large: usize) {
    let rig = Rig::new().await;
    let mut files = Vec::with_capacity(packed + large);
    for at in 0..packed {
        files.push(
            rig.farm
                .packed(format!("packed {at}"), "cas/f/w/p0", 0)
                .await,
        );
    }
    let mut wholes = BTreeSet::new();
    for at in 0..large {
        let digest = rig.farm.whole(format!("large {at}")).await;
        files.push(digest);
        wholes.insert(digest);
    }
    let root = root_of(&rig, &files).await;
    rig.write(&root).await.expect("a tree");
    let state = rig.farm.state();
    let asked: usize = state.asks.iter().map(Vec::len).sum();
    assert_eq!(
        asked,
        packed + large + 2,
        "every blob, the Action and the Command"
    );
    assert!(state.asks.iter().all(|ask| ask.len() <= LOCATE_CHUNK));
    assert_eq!(state.asks.len(), (asked).div_ceil(LOCATE_CHUNK));
    let heads: BTreeSet<Digest> = state.heads.iter().copied().collect();
    assert_eq!(
        heads, wholes,
        "a presence check for each large blob, and only them"
    );
    assert!(state.reads.is_empty(), "nothing is read");
    assert_eq!(
        state.puts.len(),
        3,
        "the input root's manifest, `.kbf` and the root"
    );
}

/// Catches: the listener's per-request bounds (65,536 packed digests, 256 presence checks)
/// applied to a tree, which would refuse or truncate a large toolchain closure: at those
/// bounds and one past each, every blob is located.
#[tokio::test]
async fn blobs_are_located_in_chunks_past_the_listeners_bounds() {
    const { assert!(HEADS_IN_FLIGHT < 256) };
    located_in_chunks(65_536, 256).await;
    located_in_chunks(65_537, 257).await;
}
