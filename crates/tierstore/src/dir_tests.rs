//! The directory backend: the names of the files, an object's calls, and the errors of a
//! directory that cannot be read or written. The multipart calls are held to the contract of the
//! trait, with the in-memory backend, in `tests/tierstore/multipart.rs`.

use std::fs::Permissions;
use std::io::{Error as IoError, ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use bytes::Bytes;
use futures::{StreamExt, future, stream};
use kbf_testkit::dir::TestDir;
use tokio::sync::oneshot;

use crate::{Backend, ByteRange, DirBackend, Error, PartBody};

use super::{NAME_LIMIT, classify, create_new, decode, encode, failed, part_name};

fn backend() -> (TestDir, DirBackend) {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-dir");
    let backend = DirBackend::open(dir.path()).unwrap();
    (dir, backend)
}

fn body(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

/// A body of these pieces.
fn pieces(texts: &[&'static str]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = texts.iter().map(|text| Ok(body(text))).collect();
    stream::iter(items).boxed()
}

/// A body of the piece `first` and then the piece `second`. When the body is asked for `second`
/// it tells `started`, and it gives `second` only after `release` is sent or dropped.
fn gated(
    first: &'static str,
    second: &'static str,
    started: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = vec![Ok(body(first))];
    let rest = stream::once(async move {
        let _ = started.send(());
        let _ = release.await;
        Ok(body(second))
    });
    stream::iter(items).chain(rest).boxed()
}

/// The names of the files in a directory, in order.
fn names(dir: &Path) -> Vec<String> {
    let entries = std::fs::read_dir(dir).unwrap();
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
}

/// Asserts that a key is written as a name with no `/`, no `.` first and no byte but letters,
/// digits and `_-.~%`, that `/` is `~` and the other bytes are `%` and two hex digits, and that
/// the name decodes back to the key.
#[test]
fn a_key_is_a_file_name_that_decodes_back() {
    let keys = [
        "cas/d/n1/b7/00012",
        ".hidden",
        "a.b",
        "..",
        "é/ü",
        "a~b%c d",
        "x-y_z",
    ];
    for key in keys {
        let name = encode(key);
        assert!(!name.contains('/'), "{name}");
        assert!(!name.starts_with('.'), "{name}");
        assert_eq!(decode(&name).as_deref(), Some(key), "{key}");
    }
    assert_eq!(encode("a/b.c"), "a~b.c");
    assert_eq!(encode(".x"), "%2Ex");
    assert_eq!(encode("a~ b%"), "a%7E%20b%25");
    assert_eq!(encode("é"), "%C3%A9");
}

/// Asserts that a name that `encode` does not make is not a key: a `%` without two hex digits
/// after it, and escapes that are not UTF-8.
#[test]
fn a_file_name_that_is_not_an_encoded_key_does_not_decode() {
    assert_eq!(decode("a~b").as_deref(), Some("a/b"));
    for name in ["%", "%4", "%zz", "%FF", "a%+f"] {
        assert_eq!(decode(name), None, "{name}");
    }
}

/// Asserts that the error of a failed call names the file, that a full disk is `Full` and any
/// other failure `Other`, and that a failure of the kind that tells a key is absent or taken is
/// that error for the key.
#[test]
fn the_error_of_a_failed_call_names_the_file_and_says_whether_the_disk_is_full() {
    let path = Path::new("/disk/file");
    let full = failed(path, &IoError::from(ErrorKind::StorageFull));
    assert!(matches!(full, Error::Full(_)), "{full}");
    assert!(full.to_string().contains("/disk/file"), "{full}");
    let denied = failed(path, &IoError::from(ErrorKind::PermissionDenied));
    assert!(matches!(denied, Error::Other(_)), "{denied}");
    assert!(denied.to_string().contains("/disk/file"), "{denied}");
    let absent = IoError::from(ErrorKind::NotFound);
    let found = classify("k", path, &absent, ErrorKind::NotFound, Error::NotFound);
    assert_eq!(found, Error::NotFound("k".to_owned()));
    let other = classify(
        "k",
        path,
        &absent,
        ErrorKind::AlreadyExists,
        Error::AlreadyExists,
    );
    assert!(matches!(other, Error::Other(_)));
}

/// Asserts that a directory is opened where it exists and made, with the one for uploads, where
/// it does not, and that a name that is empty and one that is under a file are refused.
#[test]
fn a_directory_is_made_and_a_name_that_cannot_be_one_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-open");
    let nested = dir.path().join("a/b");
    assert!(DirBackend::open(&nested).is_ok());
    assert!(nested.join(".uploads").is_dir());
    assert!(DirBackend::open(&nested).is_ok());
    let empty = DirBackend::open("").err().unwrap();
    assert!(empty.to_string().contains("no name"), "{empty}");
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    let blocked = DirBackend::open(file.join("inside")).err().unwrap();
    assert!(blocked.to_string().contains("inside"), "{blocked}");
}

/// Asserts that a key of 255 bytes is stored and one of 256, and the empty key, are refused with
/// an error for every call that names the key, and that nothing is made for them.
#[tokio::test]
async fn a_key_that_cannot_be_a_file_name_is_refused() {
    let (dir, backend) = backend();
    let longest = "a".repeat(NAME_LIMIT);
    backend.put(&longest, body("v")).await.unwrap();
    for key in ["a".repeat(NAME_LIMIT + 1), String::new()] {
        let refused = backend.put(&key, body("v")).await.unwrap_err();
        assert!(matches!(refused, Error::Other(_)), "{refused}");
        assert!(backend.get(&key, None).await.is_err());
        assert!(backend.head(&key).await.is_err());
        assert!(backend.delete(&key).await.is_err());
        assert!(backend.create_upload(&key).await.is_err());
    }
    assert_eq!(names(dir.path()), [".uploads".to_owned(), longest]);
}

/// Asserts that an object is stored, read whole and by range (byte-exact at both ends, cut at the
/// end, an error past it), described with the content tag the in-memory backend gives the same
/// bytes, and listed with its size; that a put never replaces it; and that a delete removes it
/// and succeeds for a key that is not there.
#[tokio::test]
async fn an_object_is_stored_read_described_listed_and_deleted() {
    let (_dir, backend) = backend();
    backend.put("cas/k", body("0123456789")).await.unwrap();
    let again = backend.put("cas/k", body("other")).await;
    assert_eq!(again, Err(Error::AlreadyExists("cas/k".to_owned())));
    let cases = [
        (0, 1, "0"),
        (9, 1, "9"),
        (4, 3, "456"),
        (8, 100, "89"),
        (3, 0, ""),
        (10, 5, ""),
    ];
    for (offset, len, want) in cases {
        let range = Some(ByteRange::new(offset, len));
        let got = backend.get("cas/k", range).await.unwrap();
        assert_eq!(got, want.as_bytes(), "range {offset}+{len}");
    }
    assert_eq!(backend.get("cas/k", None).await.unwrap(), "0123456789");
    let past = backend.get("cas/k", Some(ByteRange::new(11, 1))).await;
    assert!(matches!(past, Err(Error::Other(_))));
    let memory = crate::MemoryBackend::new();
    memory.put("m", body("0123456789")).await.unwrap();
    let head = backend.head("cas/k").await.unwrap();
    assert_eq!(head.size, 10);
    assert!(head.same_content(&memory.head("m").await.unwrap()));
    assert!(head.modified > 1_577_836_800_000);
    let listed = backend.list("cas/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].key.as_str(), listed[0].size), ("cas/k", 10));
    backend.delete("cas/k").await.unwrap();
    backend.delete("cas/k").await.unwrap();
    let gone = Error::NotFound("cas/k".to_owned());
    assert_eq!(backend.get("cas/k", None).await.unwrap_err(), gone);
    assert_eq!(backend.head("cas/k").await.unwrap_err(), gone);
    assert!(backend.list("").await.unwrap().is_empty());
}

/// Asserts that a listing holds the keys with the prefix in key order, with the odd bytes of
/// keys decoded, and leaves out what is not an object: a file whose name is not an encoded key,
/// a hidden file, and a directory.
#[tokio::test]
async fn a_listing_holds_the_keys_with_the_prefix_and_skips_what_is_not_an_object() {
    let (dir, backend) = backend();
    let keys = ["cas/d/n 1/é.pack", ".hidden", "a.b", "~", "cas/a"];
    for key in keys {
        backend.put(key, body("v")).await.unwrap();
    }
    std::fs::write(dir.path().join("%zz"), "not an object").unwrap();
    std::fs::write(dir.path().join(".tmp-1-1"), "a temporary file").unwrap();
    std::fs::create_dir(dir.path().join("folder")).unwrap();
    let all = backend.list("").await.unwrap();
    let all: Vec<&str> = all.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(all, [".hidden", "a.b", "cas/a", "cas/d/n 1/é.pack", "~"]);
    let cas = backend.list("cas/").await.unwrap();
    assert_eq!(cas.len(), 2);
    assert!(backend.list("zzz").await.unwrap().is_empty());
    let page = backend.list_page("cas/", Some("cas/a"), 5).await.unwrap();
    assert_eq!(page.len(), 1);
}

/// Asserts that a put with the content tag of the object replaces it and gives the new tag, that
/// another tag is refused and changes nothing, that an absent key is `NotFound`, and that no
/// temporary file is left behind.
#[tokio::test]
async fn a_put_if_match_replaces_only_the_object_with_that_tag() {
    let (dir, backend) = backend();
    backend.put("k", body("one")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;
    let wrong = backend.put_if_match("k", body("x"), "not-the-tag").await;
    assert_eq!(wrong, Err(Error::PreconditionFailed("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), "one");
    let new_tag = backend.put_if_match("k", body("two"), &tag).await.unwrap();
    assert_ne!(new_tag, tag);
    assert_eq!(backend.head("k").await.unwrap().tag, new_tag);
    assert_eq!(backend.get("k", None).await.unwrap(), "two");
    let absent = backend.put_if_match("other", body("x"), &tag).await;
    assert_eq!(absent, Err(Error::NotFound("other".to_owned())));
    assert_eq!(names(dir.path()), [".uploads".to_owned(), "k".to_owned()]);
}

/// Asserts that a file too short to hold the content tag is an error, not an object.
#[tokio::test]
async fn a_file_too_short_for_its_content_tag_is_an_error() {
    let (dir, backend) = backend();
    std::fs::write(dir.path().join("short"), "tiny").unwrap();
    let got = backend.get("short", None).await.unwrap_err();
    assert!(matches!(got, Error::Other(_)), "{got}");
    assert!(matches!(backend.head("short").await, Err(Error::Other(_))));
}

/// Asserts that a directory that cannot be written fails the calls that change it with an error
/// that names the file, and leaves nothing behind: a put, a put that replaces and a delete.
#[tokio::test]
async fn a_directory_that_cannot_be_written_fails_the_changes_and_leaves_nothing() {
    let (dir, backend) = backend();
    backend.put("k", body("one")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;
    chmod(dir.path(), 0o500);
    let put = backend.put("j", body("two")).await;
    let replace = backend.put_if_match("k", body("two"), &tag).await;
    let delete = backend.delete("k").await;
    let listed = names(dir.path());
    chmod(dir.path(), 0o700);
    for error in [put.unwrap_err(), replace.unwrap_err(), delete.unwrap_err()] {
        assert!(matches!(error, Error::Other(_)), "{error}");
    }
    assert_eq!(listed, [".uploads".to_owned(), "k".to_owned()]);
    assert_eq!(backend.get("k", None).await.unwrap(), "one");
}

/// Asserts that a directory that cannot be read fails a listing with an error, and that an
/// object in it can still be read by its name, and that a file that cannot be read is an error
/// and not an absent key.
#[tokio::test]
async fn a_directory_that_cannot_be_read_fails_the_listing() {
    let (dir, backend) = backend();
    backend.put("k", body("one")).await.unwrap();
    chmod(dir.path(), 0o300);
    let listing = backend.list("").await;
    let got = backend.get("k", None).await;
    chmod(dir.path(), 0o700);
    assert!(matches!(listing, Err(Error::Other(_))));
    assert_eq!(got.unwrap(), "one");
    chmod(&dir.path().join("k"), 0o000);
    let unreadable = backend.get("k", None).await;
    chmod(&dir.path().join("k"), 0o600);
    assert!(matches!(unreadable, Err(Error::Other(_))));
}

/// Asserts that the calls on an upload report the files they cannot write: a directory of
/// uploads that cannot be written fails the start and the abort, the directory of an upload that
/// cannot be written fails a part, and one that cannot be read fails a part and a completion,
/// as the directory of the objects that cannot be written fails a completion.
#[tokio::test]
async fn an_upload_that_cannot_be_written_or_read_fails_with_an_error() {
    let (dir, backend) = backend();
    let uploads = dir.path().join(".uploads");
    let id = backend.create_upload("k").await.unwrap();
    let doomed = backend.create_upload("d").await.unwrap();
    let upload = uploads.join(&id);
    let tag = backend
        .upload_part("k", &id, 1, pieces(&["abc"]))
        .await
        .unwrap();
    let parts = [(1, tag)];

    chmod(&uploads, 0o500);
    let start = backend.create_upload("j").await;
    let abort = backend.abort_upload("d", &doomed).await;
    chmod(&uploads, 0o700);
    assert!(matches!(start, Err(Error::Other(_))));
    assert!(matches!(abort, Err(Error::Other(_))));

    chmod(&upload, 0o500);
    let part = backend.upload_part("k", &id, 2, pieces(&["abc"])).await;
    chmod(&upload, 0o300);
    let again = backend.upload_part("k", &id, 3, pieces(&["abc"])).await;
    let complete = backend.complete_upload("k", &id, &parts).await;
    chmod(&upload, 0o700);
    assert!(matches!(part, Err(Error::Other(_))));
    assert!(matches!(again, Err(Error::Other(_))));
    assert!(matches!(complete, Err(Error::Other(_))));

    chmod(dir.path(), 0o500);
    let joined = backend.complete_upload("k", &id, &parts).await;
    chmod(dir.path(), 0o700);
    assert!(matches!(joined, Err(Error::Other(_))));
    assert_eq!(names(&upload).len(), 3);
    backend.complete_upload("k", &id, &parts).await.unwrap();
    assert_eq!(backend.get("k", None).await.unwrap(), "abc");
}

/// Asserts that the temporary files of two backends over one directory have names of their own,
/// and that a new file is never made over a name that is taken.
#[tokio::test]
async fn backends_over_one_directory_make_their_temporary_files_apart() {
    let (dir, first) = backend();
    let second = DirBackend::open(dir.path()).unwrap();
    assert_ne!(first.temp(dir.path()), second.temp(dir.path()));
    let taken = dir.path().join("taken");
    std::fs::write(&taken, "x").unwrap();
    let refused = create_new(&taken).await.unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::AlreadyExists);
    assert!(create_new(&dir.path().join("fresh")).await.is_ok());
}

/// Asserts that two uploads of one part number that overlap leave one file for the number, and
/// that the content tag of one of them, and not of the other, completes the upload. The overlap
/// is forced by gates: each body gives its first piece and then tells that it waits for its
/// second, and the test sends both releases only after both have told.
#[tokio::test]
async fn two_overlapping_uploads_of_one_part_number_leave_one_part() {
    let (dir, backend) = backend();
    let id = backend.create_upload("k").await.unwrap();
    let (started_a, a_started) = oneshot::channel();
    let (release_a, a_released) = oneshot::channel();
    let (started_b, b_started) = oneshot::channel();
    let (release_b, b_released) = oneshot::channel();
    let body_a = gated("aa", "bb", started_a, a_released);
    let body_b = gated("cc", "dd", started_b, b_released);
    let releasing = async {
        let _ = a_started.await;
        let _ = b_started.await;
        let _ = release_a.send(());
        let _ = release_b.send(());
    };
    let (a, b, _) = futures::join!(
        backend.upload_part("k", &id, 1, body_a),
        backend.upload_part("k", &id, 1, body_b),
        releasing,
    );
    let upload = dir.path().join(".uploads").join(&id);
    assert_eq!(names(&upload).len(), 2);
    let first = backend.complete_upload("k", &id, &[(1, a.unwrap())]).await;
    let second = backend.complete_upload("k", &id, &[(1, b.unwrap())]).await;
    assert_ne!(first.is_ok(), second.is_ok());
}

/// Asserts that a part whose call is dropped before its body ends leaves no temporary file in the
/// directory of the upload. The call is polled until its body, which never ends, is asked for the
/// piece after the first and tells a gate; the temporary file is then there, and it is gone once
/// the call is dropped.
#[tokio::test]
async fn a_part_dropped_half_way_leaves_no_temporary_file() {
    let (dir, backend) = backend();
    let id = backend.create_upload("k").await.unwrap();
    let (started, first_taken) = oneshot::channel();
    let items: Vec<Result<Bytes, Error>> = vec![Ok(body("ab"))];
    let endless = stream::once(async move {
        let _ = started.send(());
        std::future::pending::<Result<Bytes, Error>>().await
    });
    let stalled: PartBody<'static> = stream::iter(items).chain(endless).boxed();
    let waiting = backend.upload_part("k", &id, 1, stalled);
    let upload = dir.path().join(".uploads").join(&id);
    let parked = future::select(waiting, first_taken).await;
    assert!(matches!(parked, future::Either::Right((Ok(()), _))));
    assert_eq!(names(&upload).len(), 2);
    drop(parked);
    assert_eq!(names(&upload), ["key".to_owned()]);
}

/// Asserts that a part whose upload is removed while the part arrives is an error, and that no
/// file is left behind.
#[tokio::test]
async fn a_part_whose_upload_is_removed_while_it_arrives_is_an_error() {
    let (dir, backend) = backend();
    let id = backend.create_upload("k").await.unwrap();
    let upload = dir.path().join(".uploads").join(&id);
    let removing = stream::once(async move {
        std::fs::remove_dir_all(&upload).unwrap();
        Ok(body("x"))
    });
    let removing: PartBody<'static> = removing.boxed();
    let stored = backend.upload_part("k", &id, 1, removing).await;
    assert!(matches!(stored, Err(Error::Other(_))));
    assert!(names(&dir.path().join(".uploads")).is_empty());
}

/// Asserts that a completion that names a part that cannot be opened, and one that names a part
/// that cannot be read, is an error, and that no temporary file is left behind.
#[tokio::test]
async fn a_part_that_cannot_be_opened_or_read_fails_the_completion() {
    let (dir, backend) = backend();
    let id = backend.create_upload("k").await.unwrap();
    let tag = backend
        .upload_part("k", &id, 1, pieces(&["abc"]))
        .await
        .unwrap();
    let parts = [(1, tag.clone())];
    let upload = dir.path().join(".uploads").join(&id);
    let file = upload.join(part_name(1, &tag));
    chmod(&file, 0o000);
    let unopened = backend.complete_upload("k", &id, &parts).await;
    chmod(&file, 0o600);
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    let unread = backend.complete_upload("k", &id, &parts).await;
    assert!(matches!(unopened, Err(Error::Other(_))));
    assert!(matches!(unread, Err(Error::Other(_))));
    assert_eq!(names(dir.path()), [".uploads".to_owned()]);
}
