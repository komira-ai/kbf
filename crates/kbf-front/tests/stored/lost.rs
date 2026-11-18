//! An object the store lost while the pack index still names it: the store is general-purpose,
//! and its objects can vanish (a store swapped for an empty one). Two reads that find the object
//! gone, some time apart, have the index forget its whole pack, so FindMissingBlobs reports its
//! blobs, a client uploads them again, and an action-cache entry that names one is a miss until
//! then. A store that answers "not there" for a while, or fails to answer, loses nothing. The
//! rig's manual clock is the index's and the packs' clock.

use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::{ObjectKey, ObjectStore as _};
use kbf_pack::{CONFIRM_AFTER, CONFIRM_WITHIN, Loss, MIN_AGE};
use kbf_pack_index::PackIndex as _;
use kbf_proto::reapi::{self, ActionResult, OutputFile};
use kbf_types::Digest;
use tonic::Code;

use crate::bytestream::{pieces, read as stream_read, upload_name, write};
use crate::cas::{missing, read};
use crate::rig::{Rig, blob_len, blob_of};

fn proto(digest: &Digest) -> reapi::Digest {
    reapi::Digest {
        hash: digest.hash_hex(),
        size_bytes: digest.size_bytes as i64,
    }
}

/// Stores the blob `bytes` as a client uploads one.
async fn upload(rig: &Rig, digest: &Digest, bytes: &Bytes) {
    write(rig, pieces(digest, bytes, 700))
        .await
        .expect("uploaded");
}

/// Deletes from the store the object the index places `digest` in, as a store swapped for an
/// empty one loses it, and returns its key; the index is not told.
async fn lose(rig: &Rig, digest: &Digest) -> String {
    let at = rig.index.where_is(&[*digest]).remove(0).expect("indexed");
    let key = ObjectKey::new(&at.pack_key).expect("a key");
    rig.store.delete(&key).await.expect("deleted");
    at.pack_key
}

/// The reads of a lost blob after which the index forgot its pack, so far, as the metrics page
/// renders them: the family is a counter, its one sample is the counter's value.
fn forgotten(rig: &Rig) -> u64 {
    const NAME: &str = "kbf_pack_read_lost_forgotten_total";
    let mut text = String::new();
    kbf_front::Packs::render_metrics_of(Some(rig.stored.packs()), &mut text);
    assert!(
        text.contains(&format!("\n# TYPE {NAME} counter\n")),
        "{text}"
    );
    let samples: Vec<&str> = (text.lines())
        .filter_map(|line| line.strip_prefix(NAME)?.strip_prefix(' '))
        .collect();
    assert_eq!(samples.len(), 1, "{text}");
    let rendered: u64 = samples[0].parse().expect("a count");
    assert_eq!(rendered, rig.stored.packs().metrics().lost_forgotten.get());
    rendered
}

/// The result a client gets for `action`, or its status code.
async fn get(rig: &Rig, action: &Digest) -> Result<ActionResult, Code> {
    use kbf_proto::reapi::GetActionResultRequest;
    use kbf_proto::reapi::action_cache_client::ActionCacheClient;
    let asked = GetActionResultRequest {
        action_digest: Some(proto(action)),
        ..GetActionResultRequest::default()
    };
    let got = ActionCacheClient::new(rig.front.clone())
        .get_action_result(asked)
        .await;
    got.map(tonic::Response::into_inner).map_err(|s| s.code())
}

/// A result whose one output file is `output`.
fn names(output: &Digest) -> ActionResult {
    ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(proto(output)),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    }
}

/// Asserts the loop a lost pack takes, from a state that says present to a blob held again,
/// against a store that answers 404 for it: while the index names the lost pack FindMissingBlobs
/// says the blob is held; the first read of it is NOT_FOUND and forgets nothing; a read
/// [`CONFIRM_AFTER`] later is NOT_FOUND too and the pack is forgotten (its row and the pack's row
/// gone); FindMissingBlobs then reports the blob missing, so the client uploads it again, and the
/// upload is stored and read back. A blob of another pack keeps its row throughout.
#[tokio::test]
async fn a_pack_absent_twice_some_time_apart_makes_its_blob_missing_until_uploaded_again() {
    let rig = Rig::new().await;
    let (lost, lost_bytes) = blob_of(b"a blob whose pack the store lost");
    let (kept, kept_bytes) = blob_of(b"a blob whose pack the store keeps");
    upload(&rig, &lost, &lost_bytes).await;
    upload(&rig, &kept, &kept_bytes).await;
    let key = lose(&rig, &lost).await;
    rig.wait(MIN_AGE);

    let believed = missing(&rig, &[lost, kept]).await.expect("asked");
    let first = read(&rig, &[lost]).await.expect("read");
    let noted = missing(&rig, &[lost, kept]).await.expect("asked");
    rig.wait(CONFIRM_AFTER);
    let second = read(&rig, &[lost]).await.expect("read");
    let after = missing(&rig, &[lost, kept]).await.expect("asked");

    assert!(believed.is_empty(), "the index still names the lost pack");
    assert_eq!(first[0].0, Code::NotFound as i32);
    assert!(noted.is_empty(), "one absence forgets nothing: {noted:?}");
    assert_eq!(second[0].0, Code::NotFound as i32);
    assert_eq!(after, [lost]);
    assert!(rig.index.pack(&key).expect("asked").is_none());
    assert_eq!(forgotten(&rig), 1);

    upload(&rig, &lost, &lost_bytes).await;
    let held = missing(&rig, &[lost, kept]).await.expect("asked");
    let again = read(&rig, &[lost, kept]).await.expect("read");
    assert!(held.is_empty());
    assert_eq!(again[0], (Code::Ok as i32, lost_bytes.to_vec()));
    assert_eq!(again[1], (Code::Ok as i32, kept_bytes.to_vec()));
}

/// Asserts the case of a store whose volume servers restart while another replica holds every
/// object: its reads answer 404 and its lengths say there is no object, for a few minutes, and
/// then it serves again. No pack is forgotten, no length is asked within the wait, and the blob
/// is served once the store is back. A pack written less than [`MIN_AGE`] before is never
/// forgotten, even when two absences [`CONFIRM_AFTER`] apart are confirmed by its length.
#[tokio::test]
async fn a_store_answering_absent_for_a_while_loses_no_pack_and_a_young_one_never() {
    let rig = Rig::new().await;
    let (held, bytes) = blob_of(b"a blob the store holds on its other replica");
    upload(&rig, &held, &bytes).await;
    let key = rig
        .index
        .where_is(&[held])
        .remove(0)
        .expect("indexed")
        .pack_key;
    rig.store.hide_len_of(&key);

    rig.store.lose_reads(1);
    let young = read(&rig, &[held]).await.expect("read");
    rig.wait(CONFIRM_AFTER);
    rig.store.lose_reads(1);
    let young_again = read(&rig, &[held]).await.expect("read");
    let kept_young = missing(&rig, &[held]).await.expect("asked");
    rig.wait(MIN_AGE + CONFIRM_WITHIN);
    rig.store.lose_reads(2);
    let restarting = read(&rig, &[held]).await.expect("read");
    rig.wait(CONFIRM_AFTER - Duration::from_secs(1));
    let still = read(&rig, &[held]).await.expect("read");
    let back = read(&rig, &[held]).await.expect("read");

    assert_eq!(young[0].0, Code::NotFound as i32);
    assert_eq!(young_again[0].0, Code::NotFound as i32);
    assert!(
        kept_young.is_empty(),
        "a young pack is kept: {kept_young:?}"
    );
    assert_eq!(restarting[0].0, Code::NotFound as i32);
    assert_eq!(still[0].0, Code::NotFound as i32);
    assert_eq!(back[0], (Code::Ok as i32, bytes.to_vec()));
    assert!(missing(&rig, &[held]).await.expect("asked").is_empty());
    assert!(rig.index.pack(&key).expect("asked").is_some());
    assert_eq!(forgotten(&rig), 0);
}

/// Asserts that a store that fails to answer forgets nothing, even for a large blob whose object
/// is gone: reads answered 503, at the first look and after the wait, are UNAVAILABLE and no
/// absence; and the blob stays held for FindMissingBlobs.
#[tokio::test]
async fn reads_the_store_fails_forget_nothing() {
    let rig = Rig::new().await;
    let (large, bytes) = blob_len(3000, 17);
    upload(&rig, &large, &bytes).await;
    lose(&rig, &large).await;
    rig.wait(MIN_AGE);
    let name = upload_name(&large);

    rig.store.reject_reads(1);
    let first = stream_read(&rig, &name, 0, 0).await;
    rig.wait(CONFIRM_AFTER);
    rig.store.reject_reads(1);
    let second = stream_read(&rig, &name, 0, 0).await;

    assert_eq!(first.expect_err("failed").code(), Code::Unavailable);
    assert_eq!(second.expect_err("failed").code(), Code::Unavailable);
    assert!(missing(&rig, &[large]).await.expect("asked").is_empty());
    assert_eq!(forgotten(&rig), 0);
}

/// Asserts that a blob whose row is in a lost pack, and whose bytes are also in a second pack
/// that has no row for it (the first location wins), is reported missing once the lost pack is
/// forgotten: nothing reads the second pack's footer to find it there. The client's upload of it
/// is stored and recorded, and the blob is read back.
#[tokio::test]
async fn a_blob_also_in_a_second_pack_is_uploaded_again_once_its_pack_is_forgotten() {
    let rig = Rig::new().await;
    let (twice, bytes) = blob_of(b"a blob two packs hold, one of them with its row");
    upload(&rig, &twice, &bytes).await;
    rig.store
        .seed("cas/f/other/boot1/0000000000000001", &bytes)
        .await;
    let second = kbf_pack_index::NewPack {
        key: "cas/f/other/boot1/0000000000000001".to_owned(),
        writer: "other".to_owned(),
        size: bytes.len() as u64,
        blobs: vec![kbf_pack_index::PackBlob {
            digest: crate::rig::index_form(&twice),
            offset: 0,
        }],
    };
    let recorded = rig.index.record_pack(&second).expect("recorded");
    let key = lose(&rig, &twice).await;
    rig.wait(MIN_AGE);

    read(&rig, &[twice]).await.expect("read");
    rig.wait(CONFIRM_AFTER);
    read(&rig, &[twice]).await.expect("read");
    let reported = missing(&rig, &[twice]).await.expect("asked");
    upload(&rig, &twice, &bytes).await;
    let back = read(&rig, &[twice]).await.expect("read");

    assert_eq!(recorded.indexed, 0, "the first location won");
    assert_eq!(reported, [twice]);
    assert!(rig.index.pack(&key).expect("asked").is_none());
    assert_eq!(back[0], (Code::Ok as i32, bytes.to_vec()));
    let row = rig.index.where_is(&[twice]).remove(0).expect("indexed");
    assert_ne!(row.pack_key, key);
}

/// Asserts that an action-cache hit whose output's pack the store lost is served while the index
/// names the pack, that the client's reads of the output are NOT_FOUND, and that once two reads
/// [`CONFIRM_AFTER`] apart have found it absent the entry is a miss (NOT_FOUND, the client runs
/// the action again) and its output is missing; once the output is uploaded again, the entry
/// names only held blobs and is a hit.
#[tokio::test]
async fn an_action_result_whose_output_is_lost_is_a_miss_once_its_pack_is_forgotten() {
    let rig = Rig::new().await;
    let (action, _) = blob_of(b"an action");
    let (output, bytes) = blob_len(100, 7);
    upload(&rig, &output, &bytes).await;
    rig.stored
        .put_action_result(&action, &names(&output))
        .await
        .expect("written");
    lose(&rig, &output).await;
    rig.wait(MIN_AGE);

    let believed = get(&rig, &action).await;
    let fetched = stream_read(&rig, &upload_name(&output), 0, 0).await;
    let still = get(&rig, &action).await;
    rig.wait(CONFIRM_AFTER);
    let confirmed = stream_read(&rig, &upload_name(&output), 0, 0).await;
    let after = get(&rig, &action).await;
    let reported = missing(&rig, &[output]).await.expect("asked");

    assert_eq!(believed, Ok(names(&output)));
    assert_eq!(fetched.expect_err("lost").code(), Code::NotFound);
    assert_eq!(still, Ok(names(&output)));
    assert_eq!(confirmed.expect_err("lost").code(), Code::NotFound);
    assert_eq!(after, Err(Code::NotFound));
    assert_eq!(reported, [output]);

    upload(&rig, &output, &bytes).await;
    assert_eq!(get(&rig, &action).await, Ok(names(&output)));
}

/// Asserts that a large blob (an object of its own under `cas/l/`) whose object the store lost
/// is forgotten on the read [`CONFIRM_AFTER`] after the first that found it gone, both
/// UNAVAILABLE; that the next read is NOT_FOUND and FindMissingBlobs reports it; and that its
/// upload, which writes the same key again, is recorded anew and read back whole.
#[tokio::test]
async fn a_lost_large_blob_is_forgotten_and_its_upload_under_the_same_key_is_recorded() {
    let rig = Rig::new().await;
    let (large, bytes) = blob_len(3000, 9);
    upload(&rig, &large, &bytes).await;
    let key = lose(&rig, &large).await;
    rig.wait(MIN_AGE);
    let name = upload_name(&large);

    let first = stream_read(&rig, &name, 0, 0).await;
    let noted = missing(&rig, &[large]).await.expect("asked");
    rig.wait(CONFIRM_AFTER);
    let second = stream_read(&rig, &name, 0, 0).await;
    let third = stream_read(&rig, &name, 0, 0).await;
    let reported = missing(&rig, &[large]).await.expect("asked");

    assert_eq!(first.expect_err("lost").code(), Code::Unavailable);
    assert!(noted.is_empty(), "{noted:?}");
    assert_eq!(second.expect_err("lost").code(), Code::Unavailable);
    assert_eq!(third.expect_err("forgotten").code(), Code::NotFound);
    assert_eq!(reported, [large]);
    assert!(rig.index.pack(&key).expect("asked").is_none());
    assert_eq!(forgotten(&rig), 1);

    upload(&rig, &large, &bytes).await;
    let read_back = stream_read(&rig, &name, 0, 0).await.expect("held");
    assert_eq!(read_back, bytes.to_vec());
    assert!(rig.index.pack(&key).expect("asked").is_some());
}

/// Asserts that a lost large blob whose object's length the store cannot give at the second look
/// is not forgotten: the reads are UNAVAILABLE and the index keeps its row, so nothing a store
/// that merely failed to answer says is taken for a loss.
#[tokio::test]
async fn a_loss_the_store_cannot_confirm_forgets_nothing() {
    let rig = Rig::new().await;
    let (large, bytes) = blob_len(3000, 11);
    upload(&rig, &large, &bytes).await;
    let key = lose(&rig, &large).await;
    rig.store.fail_len_of(&key);
    rig.wait(MIN_AGE);
    let name = upload_name(&large);

    let first = stream_read(&rig, &name, 0, 0).await;
    rig.wait(CONFIRM_AFTER);
    let second = stream_read(&rig, &name, 0, 0).await;

    assert_eq!(first.expect_err("lost").code(), Code::Unavailable);
    assert_eq!(second.expect_err("lost").code(), Code::Unavailable);
    assert!(missing(&rig, &[large]).await.expect("asked").is_empty());
    assert_eq!(forgotten(&rig), 0);
}

/// Asserts that a large blob whose read the store answers "not there" while it holds the object
/// whole (a read that raced the write) is not forgotten: the read is UNAVAILABLE, the row is kept
/// and the next read is served, which clears the absence noted: a later "not there" is a first
/// absence again, and forgets nothing.
#[tokio::test]
async fn a_large_read_that_raced_its_object_forgets_nothing() {
    let rig = Rig::new().await;
    let (large, bytes) = blob_len(3000, 13);
    upload(&rig, &large, &bytes).await;
    rig.wait(MIN_AGE);
    let name = upload_name(&large);
    let key = rig
        .index
        .where_is(&[large])
        .remove(0)
        .expect("indexed")
        .pack_key;
    rig.store.hide_len_of(&key);

    rig.store.lose_reads(1);
    let first = stream_read(&rig, &name, 0, 0).await;
    let second = stream_read(&rig, &name, 0, 0).await;
    rig.wait(CONFIRM_AFTER);
    rig.store.lose_reads(1);
    let third = stream_read(&rig, &name, 0, 0).await;

    assert_eq!(first.expect_err("raced").code(), Code::Unavailable);
    assert_eq!(second.expect("held"), bytes.to_vec());
    assert_eq!(third.expect_err("raced").code(), Code::Unavailable);
    assert!(missing(&rig, &[large]).await.expect("asked").is_empty());
    assert_eq!(forgotten(&rig), 0);
}

/// Asserts that a confirmed loss of a large blob whose forget the index fails keeps its row (the
/// read is UNAVAILABLE and nothing is counted), and that the next read, the index answering
/// again, forgets it.
#[tokio::test]
async fn a_forget_the_index_fails_is_made_by_the_next_read() {
    let rig = Rig::new().await;
    let (large, bytes) = blob_len(3000, 15);
    upload(&rig, &large, &bytes).await;
    lose(&rig, &large).await;
    rig.wait(MIN_AGE);
    let name = upload_name(&large);
    stream_read(&rig, &name, 0, 0).await.expect_err("lost");
    rig.wait(CONFIRM_AFTER);
    rig.index.fail_forget(1);

    let refused = stream_read(&rig, &name, 0, 0).await;
    let kept = missing(&rig, &[large]).await.expect("asked");
    let second = stream_read(&rig, &name, 0, 0).await;

    assert_eq!(refused.expect_err("lost").code(), Code::Unavailable);
    assert!(kept.is_empty());
    assert_eq!(second.expect_err("lost").code(), Code::Unavailable);
    assert_eq!(missing(&rig, &[large]).await.expect("asked"), [large]);
    assert_eq!(forgotten(&rig), 1);
}

/// Asserts the frontend's own check of a pack through its CAS (what an action tree's check asks):
/// a pack the store holds whole is kept; one it lost is noted on the first check and forgotten
/// on one [`CONFIRM_AFTER`] later; and a check of it after that changes nothing.
#[tokio::test]
async fn the_backend_forgets_a_pack_the_store_lost_on_a_check_some_time_after_the_first() {
    use kbf_front::job_read::BlobSource as _;
    let rig = Rig::new().await;
    let (lost, lost_bytes) = blob_of(b"a blob whose pack the store lost");
    let (kept, kept_bytes) = blob_of(b"a blob whose pack is held");
    upload(&rig, &lost, &lost_bytes).await;
    upload(&rig, &kept, &kept_bytes).await;
    let held = rig.index.where_is(&[kept]).remove(0).expect("indexed");
    let key = lose(&rig, &lost).await;
    rig.wait(MIN_AGE);

    let kept_check = rig
        .backend
        .forget_lost(&held.pack_key, held.offset + kept.size_bytes);
    let first_check = rig.backend.forget_lost(&key, 1);
    assert!(!kept_check.await.expect("asked"));
    assert!(!first_check.await.expect("asked"));
    assert!(
        missing(&rig, &[lost, kept])
            .await
            .expect("asked")
            .is_empty()
    );
    rig.wait(CONFIRM_AFTER);
    let second_check = rig.backend.forget_lost(&key, 1);

    assert!(second_check.await.expect("asked"));
    assert_eq!(missing(&rig, &[lost, kept]).await.expect("asked"), [lost]);
    assert!(!rig.backend.forget_lost(&key, 1).await.expect("asked"));
    assert!(rig.index.pack(&key).expect("asked").is_none());
}

/// Asserts what the packs answer when a job reports the pack `key` absent ([`Packs::absent`]):
/// the first report is noted, the one [`CONFIRM_AFTER`] later asks the length and forgets the
/// pack, counted; and a report of a key the store holds is held at the second look.
#[tokio::test]
async fn a_job_report_of_an_absent_pack_is_confirmed_as_a_read_is() {
    let rig = Rig::new().await;
    let (lost, lost_bytes) = blob_of(b"a blob whose pack the store lost");
    let (kept, kept_bytes) = blob_of(b"a blob whose pack is held");
    upload(&rig, &lost, &lost_bytes).await;
    upload(&rig, &kept, &kept_bytes).await;
    let held = rig
        .index
        .where_is(&[kept])
        .remove(0)
        .expect("indexed")
        .pack_key;
    let key = lose(&rig, &lost).await;
    rig.wait(MIN_AGE);
    let packs = rig.stored.packs();

    let first = (packs.absent(&key, 0).await, packs.absent(&held, 0).await);
    rig.wait(CONFIRM_AFTER);
    let second = (packs.absent(&key, 0).await, packs.absent(&held, 0).await);

    assert_eq!(first, (Loss::Noted, Loss::Noted));
    assert_eq!(second, (Loss::Forgotten { rows: 1 }, Loss::Held));
    assert_eq!(missing(&rig, &[lost, kept]).await.expect("asked"), [lost]);
    assert_eq!(forgotten(&rig), 1);
}
