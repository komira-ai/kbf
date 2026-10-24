//! The multipart uploads in progress.

use std::time::Duration;

use bytes::Bytes;

use super::uploads::{Part, Rejected, Uploads};

const TTL: Duration = Duration::from_secs(60);

/// The most bytes the parts of all the uploads may hold in these tests.
const LIMIT: u64 = 10;

fn part(text: &str) -> Part {
    Part {
        body: Bytes::copy_from_slice(text.as_bytes()),
        tag: format!("tag-{text}"),
    }
}

/// Adds a part with this text, as number `number`, to the upload `id` of `key`.
fn add(uploads: &Uploads, id: &str, key: &str, number: u32, text: &str) -> Result<(), Rejected> {
    uploads.add_part(id, key, number, part(text), LIMIT, TTL)
}

/// Asserts that every upload has an id of its own, also across servers started at different
/// times, and that a new upload has no parts.
#[tokio::test(start_paused = true)]
async fn each_upload_has_an_id_of_its_own() {
    let uploads = Uploads::new(0xAB);
    let first = uploads.create("k", TTL);
    let second = uploads.create("k", TTL);
    assert_ne!(first, second);
    assert!(first.starts_with("ab-"));
    assert!(Uploads::new(0xAC).create("k", TTL).starts_with("ac-"));
    let held = uploads.parts(&first, "k").map(|parts| parts.len());
    assert_eq!(held, Some(0));
}

/// Asserts that parts are kept by number, that a part of a number replaces the one before, and
/// that the parts of an upload are read back with their tags.
#[tokio::test(start_paused = true)]
async fn parts_are_kept_by_number_and_replaced() {
    let uploads = Uploads::new(1);
    let id = uploads.create("k", TTL);
    add(&uploads, &id, "k", 2, "bb").unwrap();
    add(&uploads, &id, "k", 1, "a").unwrap();
    add(&uploads, &id, "k", 2, "ccc").unwrap();
    let parts = uploads.parts(&id, "k").unwrap();
    let numbers: Vec<u32> = parts.keys().copied().collect();
    assert_eq!(numbers, [1, 2]);
    assert_eq!(parts[&2].body, Bytes::from_static(b"ccc"));
    assert_eq!(parts[&2].tag, "tag-ccc");
    assert_eq!(uploads.held(), 4);
}

/// Asserts that a part that would take the parts of all the uploads past the limit is refused
/// and not kept, that a part that replaces another counts only for the difference, and that
/// the limit is shared by the uploads.
#[tokio::test(start_paused = true)]
async fn the_limit_is_on_the_bytes_of_all_the_parts() {
    let uploads = Uploads::new(1);
    let first = uploads.create("k", TTL);
    let second = uploads.create("j", TTL);
    add(&uploads, &first, "k", 1, "1234").unwrap();
    add(&uploads, &second, "j", 1, "123456").unwrap();
    let refused = add(&uploads, &first, "k", 2, "x");
    assert_eq!(refused, Err(Rejected::Full));
    assert_eq!(uploads.held(), 10);
    add(&uploads, &first, "k", 1, "12").unwrap();
    assert_eq!(uploads.held(), 8);
    add(&uploads, &first, "k", 2, "xx").unwrap();
    assert_eq!(uploads.held(), 10);
}

/// Asserts that a part that would take the parts of its upload alone past the limit is refused
/// as too large, whatever the other uploads hold, and not kept; that a part that replaces
/// another counts only for the difference; and that a part that fits is taken.
#[tokio::test(start_paused = true)]
async fn a_part_that_the_limit_never_allows_is_too_large() {
    let uploads = Uploads::new(1);
    let id = uploads.create("k", TTL);
    let other = uploads.create("j", TTL);
    add(&uploads, &other, "j", 1, "123").unwrap();
    let alone = add(&uploads, &id, "k", 1, "12345678901");
    assert_eq!(alone, Err(Rejected::TooLarge));
    assert_eq!(uploads.held(), 3);
    add(&uploads, &id, "k", 1, "123456").unwrap();
    let grown = add(&uploads, &id, "k", 2, "12345");
    assert_eq!(grown, Err(Rejected::TooLarge));
    assert_eq!(uploads.held(), 9);
    add(&uploads, &id, "k", 1, "1234567").unwrap();
    assert_eq!(uploads.held(), 10);
}

/// Asserts that a part for an upload that does not exist, or that is of another key, is refused.
#[tokio::test(start_paused = true)]
async fn a_part_needs_its_upload() {
    let uploads = Uploads::new(1);
    let id = uploads.create("k", TTL);
    let unknown = add(&uploads, "nope", "k", 1, "a");
    assert_eq!(unknown, Err(Rejected::NoSuchUpload));
    let other_key = add(&uploads, &id, "j", 1, "a");
    assert_eq!(other_key, Err(Rejected::NoSuchUpload));
    assert!(uploads.parts(&id, "j").is_none());
    assert!(uploads.parts("nope", "k").is_none());
}

/// Asserts that finishing an upload forgets it, and that an abort forgets it only for the key
/// it belongs to, and says whether there was an upload.
#[tokio::test(start_paused = true)]
async fn an_upload_is_forgotten_when_it_is_finished_or_aborted() {
    let uploads = Uploads::new(1);
    let done = uploads.create("k", TTL);
    uploads.finish(&done);
    assert!(uploads.parts(&done, "k").is_none());
    let open = uploads.create("k", TTL);
    assert!(!uploads.abort(&open, "j"));
    assert!(uploads.parts(&open, "k").is_some());
    assert!(uploads.abort(&open, "k"));
    assert!(!uploads.abort(&open, "k"));
    assert!(uploads.parts(&open, "k").is_none());
}

/// Asserts that an upload that got no part for the time to live is dropped when the next
/// upload is made or the next part is added, and that a part keeps an upload for the time to
/// live again.
#[tokio::test(start_paused = true)]
async fn an_upload_that_gets_no_part_is_dropped_after_the_time_to_live() {
    let uploads = Uploads::new(1);
    let old = uploads.create("k", TTL);
    let kept = uploads.create("j", TTL);
    tokio::time::advance(Duration::from_secs(40)).await;
    add(&uploads, &kept, "j", 1, "a").unwrap();
    tokio::time::advance(Duration::from_secs(40)).await;
    let new = uploads.create("m", TTL);
    assert!(uploads.parts(&old, "k").is_none());
    assert!(uploads.parts(&kept, "j").is_some());
    assert!(uploads.parts(&new, "m").is_some());
    tokio::time::advance(Duration::from_secs(40)).await;
    let late = add(&uploads, &kept, "j", 2, "b");
    assert_eq!(late, Err(Rejected::NoSuchUpload));
    assert_eq!(uploads.held(), 0);
}
