use bytes::Bytes;
use kbf_front::{Cache, VerifiedBlob};
use kbf_proto::reapi::{Action, Command, Platform, command::EnvironmentVariable, platform};
use kbf_types::{Digest, DigestFunction};
use serde_json::json;

use super::*;

async fn stored(
    cache: &Cache<kbf_front::MemoryMetaLog, kbf_objstore::MemoryStore>,
    b: &[u8],
) -> Digest {
    let blob = VerifiedBlob::hashed(Bytes::copy_from_slice(b));
    let digest = blob.digest();
    cache.store_blobs(vec![blob]).await.expect("store");
    digest
}

fn prop(name: &str, value: &str) -> platform::Property {
    platform::Property {
        name: name.to_owned(),
        value: value.to_owned(),
    }
}

/// Catches: a tail that is the head of the log, one longer than the bound, one whose
/// `bytes` is the log's size rather than the tail's (so `truncated` never shows), and a
/// cut inside a multi-byte character that leaves its stray continuation bytes in front
/// (shown as replacement characters).
#[tokio::test]
async fn a_tail_is_the_end_of_the_log_cut_on_a_character() {
    let cache = Cache::memory();
    let short = stored(&cache, b"one\ntwo\n").await;
    let got = tail(&cache, &short).await.expect("a tail");
    assert_eq!(
        got,
        Tail {
            text: "one\ntwo\n".to_owned(),
            bytes: 8
        }
    );

    // ASCII over the bound: exactly the last TAIL_BYTES, nothing dropped.
    let ascii = stored(&cache, "a".repeat(TAIL_BYTES as usize + 5).as_bytes()).await;
    let got = tail(&cache, &ascii).await.expect("a tail");
    assert_eq!(got.bytes, TAIL_BYTES);
    assert_eq!(got.text.len() as u64, TAIL_BYTES);

    // '€' is three bytes and TAIL_BYTES is one more than a multiple of three: the cut
    // falls inside a character, leaving one stray byte first. It is dropped, not shown
    // as a replacement character.
    let euros = stored(&cache, "€".repeat(TAIL_BYTES as usize).as_bytes()).await;
    let got = tail(&cache, &euros).await.expect("a tail");
    assert!(
        got.text.chars().all(|c| c == '€'),
        "no replacement or stray byte: {:?}",
        &got.text[..8]
    );
    assert_eq!(got.bytes, TAIL_BYTES - 1, "the stray byte is not counted");
    assert_eq!(
        got.text.len() as u64,
        got.bytes,
        "bytes counts what is shown"
    );
}

/// Catches: an empty log read from the CAS (it has no object to read), a log over the
/// read bound read anyway, and a blob that is not held reported as a read error.
#[tokio::test]
async fn a_tail_says_why_it_is_not_there() {
    let cache = Cache::memory();
    let empty = Digest::new(DigestFunction::Sha256, [0; 32], 0);
    assert_eq!(
        tail(&cache, &empty).await,
        Ok(Tail {
            text: String::new(),
            bytes: 0
        })
    );
    let huge = Digest::new(DigestFunction::Sha256, [1; 32], TAIL_READ_BYTES + 1);
    assert_eq!(tail(&cache, &huge).await, Err(Omitted::TooLarge));
    let gone = Digest::new(DigestFunction::Sha256, [2; 32], 10);
    assert_eq!(tail(&cache, &gone).await, Err(Omitted::NotHeld));
}

/// Catches: the omission names swapped, which a page shows as the reason a log is
/// missing.
#[test]
fn omissions_are_named() {
    let names: Vec<_> = [
        Omitted::BlobsOff,
        Omitted::NotHeld,
        Omitted::TooLarge,
        Omitted::Unreadable,
    ]
    .map(Omitted::name)
    .into();
    assert_eq!(names, ["blobs_off", "not_held", "too_large", "unreadable"]);
}

/// Catches: `truncated` computed against the tail's own size (never true), a tail kept
/// when the read failed, and a stale `null` left where the tail goes.
#[test]
fn a_tail_is_written_into_its_blob_object() {
    let mut log = json!({"bytes": 100, "tail": null, "tail_bytes": null, "truncated": null})
        .as_object()
        .cloned()
        .expect("an object");
    put_tail(
        &mut log,
        Ok(Tail {
            text: "abc".to_owned(),
            bytes: 3,
        }),
    );
    assert_eq!(
        serde_json::Value::Object(log.clone()),
        json!({"bytes": 100, "tail": "abc", "tail_bytes": 3, "truncated": true})
    );
    put_tail(
        &mut log,
        Ok(Tail {
            text: "x".repeat(100),
            bytes: 100,
        }),
    );
    assert_eq!(log["truncated"], false, "the whole log is the tail");
    put_tail(&mut log, Err(Omitted::NotHeld));
    assert_eq!(log["tail_omitted"], "not_held");
}

/// Catches: a command that shows environment values (secrets), the Command's platform
/// used over the Action's, the Action's timeout lost or written in the wrong unit, a
/// deprecated output list dropped, and a negative timeout shown as a huge one.
#[test]
fn a_command_shows_names_not_values() {
    #[allow(deprecated)]
    let command = Command {
        arguments: vec!["cc".to_owned(), "-c".to_owned(), "a.c".to_owned()],
        environment_variables: vec![EnvironmentVariable {
            name: "TOKEN".to_owned(),
            value: "hunter2".to_owned(),
        }],
        working_directory: "src".to_owned(),
        output_paths: vec!["out/a.o".to_owned()],
        output_files: vec!["old/a.o".to_owned()],
        output_directories: vec!["old_dir".to_owned()],
        platform: Some(Platform {
            properties: vec![prop("OSFamily", "command")],
        }),
        ..Command::default()
    };
    let action = Action {
        timeout: Some(prost_types::Duration {
            seconds: 2,
            nanos: 500_000,
        }),
        platform: Some(Platform {
            properties: vec![prop("OSFamily", "linux"), prop("arch", "x86_64")],
        }),
        ..Action::default()
    };
    let shown = render(&action, command.clone());
    assert_eq!(
        shown,
        json!({
            "arguments": ["cc", "-c", "a.c"],
            "environment_names": ["TOKEN"],
            "working_directory": "src",
            "platform": {"OSFamily": "linux", "arch": "x86_64"},
            "timeout_micros": 2_000_500,
            "output_paths": ["out/a.o", "old/a.o", "old_dir"],
            "truncated": false,
        })
    );
    assert!(!shown.to_string().contains("hunter2"), "a value leaked");

    // No platform on the Action: the Command's. An empty one counts as none.
    for platform in [None, Some(Platform::default())] {
        let bare = Action {
            platform,
            ..Action::default()
        };
        assert_eq!(
            render(&bare, command.clone())["platform"],
            json!({"OSFamily": "command"})
        );
    }
    assert_eq!(
        render(&Action::default(), Command::default())["platform"],
        json!({})
    );

    let negative = Action {
        timeout: Some(prost_types::Duration {
            seconds: -1,
            nanos: 0,
        }),
        ..Action::default()
    };
    assert_eq!(timeout_micros(&negative), serde_json::Value::Null);
    assert_eq!(timeout_micros(&Action::default()), serde_json::Value::Null);
    let overflow = Action {
        timeout: Some(prost_types::Duration {
            seconds: i64::MAX,
            nanos: 0,
        }),
        ..Action::default()
    };
    assert_eq!(timeout_micros(&overflow), serde_json::Value::Null);
}

/// Catches: a command line of any length copied whole into a page (a Command blob may be
/// a megabyte and a page lists a hundred), strings dropped from the front or in another
/// order than output paths, names, then arguments, and a cut that does not say so.
#[test]
fn a_very_long_command_is_cut_from_the_end_and_says_so() {
    let word = "w".repeat(1024);
    let command = Command {
        arguments: vec![word.clone(); 100],
        environment_variables: vec![EnvironmentVariable {
            name: word.clone(),
            value: String::new(),
        }],
        output_paths: vec![word.clone(); 10],
        working_directory: "wd".to_owned(),
        ..Command::default()
    };
    let shown = render(&Action::default(), command);
    assert_eq!(shown["truncated"], true);
    let count = |key: &str| shown[key].as_array().expect("a list").len();
    assert_eq!(count("output_paths"), 0, "paths go first");
    assert_eq!(count("environment_names"), 0, "then names");
    assert_eq!(
        count("arguments"),
        63,
        "then arguments: 63 KiB of them and 2 bytes of directory fit in 64 KiB"
    );
    assert_eq!(shown["working_directory"], "wd", "never cut");
}

/// Catches: a log whose object the store no longer holds (the index still names it)
/// shown as an empty tail or taken for a blob that was never held; it is `unreadable`.
#[tokio::test]
async fn a_blob_the_store_lost_is_unreadable() {
    use kbf_objstore::{ObjectStore, PageSize};
    let cache = Cache::memory();
    let held = stored(&cache, b"a log the store will lose").await;
    let page = cache
        .objects()
        .list(cache.prefix(), None, PageSize::MAX)
        .await
        .expect("a listing");
    for object in &page.objects {
        cache.objects().delete(&object.key).await.expect("deleted");
    }
    let lost = tail(&cache, &held).await;
    assert_eq!(
        lost,
        Err(Omitted::Unreadable),
        "{} objects deleted",
        page.objects.len()
    );
}

fn read(action: Digest, command: Option<Digest>) -> Read {
    Read {
        action,
        command,
        stdout: None,
        stderr: None,
    }
}

/// Catches: a blob over the bound read anyway, a missing one that answers, and a command
/// built from an Action or Command that does not decode (garbage in the CAS must not
/// become an empty command shown as the real one).
#[tokio::test]
async fn a_command_is_read_only_from_small_blobs_that_decode() {
    let cache = Cache::memory();
    let big = Digest::new(DigestFunction::Sha256, [3; 32], COMMAND_READ_BYTES + 1);
    assert!(blob(&cache, &big).await.is_none(), "over the bound");
    let gone = Digest::new(DigestFunction::Sha256, [4; 32], 5);
    assert!(blob(&cache, &gone).await.is_none(), "not held");
    let held = stored(&cache, b"x").await;
    assert_eq!(blob(&cache, &held).await.as_deref(), Some(&b"x"[..]));

    // A byte string that is no message: tag 0xff... is an invalid wire type.
    let garbage = stored(&cache, &[0xff, 0xff, 0xff]).await;
    let command = stored(&cache, &Command::default().encode_to_vec()).await;
    let action = stored(&cache, &Action::default().encode_to_vec()).await;
    assert!(
        command_of(&cache, read(garbage, Some(command)))
            .await
            .is_none()
    );
    assert!(
        command_of(&cache, read(action, Some(garbage)))
            .await
            .is_none()
    );
    assert!(command_of(&cache, read(action, None)).await.is_none());
    assert!(
        command_of(&cache, read(action, Some(command)))
            .await
            .is_some()
    );
}

async fn command_of(
    cache: &Cache<kbf_front::MemoryMetaLog, kbf_objstore::MemoryStore>,
    read: Read,
) -> Option<serde_json::Value> {
    command(cache, &read).await
}
