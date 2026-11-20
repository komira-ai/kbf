//! `FilerPresence` against a fake SeaweedFS filer on loopback (`filer::fake`): the calls it
//! makes are counted and their names read back; no test looks at a clock. The fake's own
//! tests are in `fake.rs`, in this one test binary, so that the fake's code is measured
//! by one binary.

mod fake;

use std::time::Duration;

use kbf_objstore::ObjectKey;
use kbf_objstore::filer::fake::FakeFiler;
use kbf_objstore::filer::{
    Ask, FilerError, FilerPresence, FilerRoot, MAX_NAMES_PER_CALL, Presence, PresenceCheck,
    Unanswered,
};
use kbf_proto::filer_pb::{
    Entry, FileChunk, FuseAttributes, LookupDirectoryEntryResult, RemoteEntry,
};
use tonic::Code;

const ROOT: &str = "/buckets/kbf-cas";

fn ask(key: &str, size: u64) -> Ask {
    Ask {
        key: ObjectKey::new(key).expect("key"),
        size,
    }
}

fn client(filer: &FakeFiler) -> FilerPresence {
    let root = FilerRoot::new(ROOT).expect("root");
    FilerPresence::connect_lazy(&filer.address(), root, Duration::from_secs(30)).expect("client")
}

fn asked(directory: &str, name: &str) -> (String, String) {
    (directory.to_owned(), name.to_owned())
}

/// Catches a key-to-path mapping that asks for the wrong entries, which would answer every
/// pack missing: each key is the entry of its last segment, in the directory of the rest
/// under the root, and a key with no `/` is an entry of the root itself.
#[tokio::test]
async fn a_lookup_names_every_key_under_the_root() {
    let filer = FakeFiler::start().await;
    filer.put_file("/buckets/kbf-cas/cas/l/00ab", 10);
    filer.put_file("/buckets/kbf-cas/cas/b/100000-0/n7.p", 20);
    filer.put_file("/buckets/kbf-cas/top", 30);
    let asks = [
        ask("cas/l/00ab", 10),
        ask("cas/b/100000-0/n7.p", 20),
        ask("top", 30),
    ];

    let answers = client(&filer).check(&asks).await.expect("check");

    assert_eq!(
        answers,
        [
            Presence::Present { size: 10 },
            Presence::Present { size: 20 },
            Presence::Present { size: 30 },
        ]
    );
    assert_eq!(
        filer.asked(),
        [
            asked("/buckets/kbf-cas/cas/l", "00ab"),
            asked("/buckets/kbf-cas/cas/b/100000-0", "n7.p"),
            asked("/buckets/kbf-cas", "top"),
        ]
    );
    assert_eq!(filer.lookups(), [3], "three keys are one call");
}

/// Catches keys dropped past the filer's limit of 4,096 names a call (the fake refuses a
/// larger call, as the filer does), and answers put back in the wrong order: 10,000 keys
/// are three calls, of 4,096, 4,096 and 1,808 names, and every key gets its own answer.
#[tokio::test]
async fn a_batch_over_4096_keys_is_split_and_every_key_answered() {
    let filer = FakeFiler::start().await;
    let keys = 10_000;
    let mut asks = Vec::with_capacity(keys);
    for n in 0..keys {
        // Every third key is held, with a size of its own number.
        if n % 3 == 0 {
            filer.put_file(&format!("{ROOT}/cas/l/{n:05}"), n as u64);
        }
        asks.push(ask(&format!("cas/l/{n:05}"), n as u64));
    }

    let answers = client(&filer).check(&asks).await.expect("check");

    assert_eq!(answers.len(), keys);
    for (n, answer) in answers.iter().enumerate() {
        let want = if n % 3 == 0 {
            Presence::Present { size: n as u64 }
        } else {
            Presence::Absent
        };
        assert_eq!(*answer, want, "key {n}");
    }
    let mut lookups = filer.lookups();
    lookups.sort_unstable();
    assert_eq!(lookups, [10_000 - 2 * MAX_NAMES_PER_CALL, 4096, 4096]);
    assert_eq!(filer.asked().len(), keys, "each key asked once");
}

/// Catches an off-by-one at the limit: exactly 4,096 keys are one call, 4,097 are two.
#[tokio::test]
async fn exactly_4096_keys_are_one_call_and_one_more_is_two() {
    let filer = FakeFiler::start().await;
    let presence = client(&filer);
    let asks: Vec<Ask> = (0..=MAX_NAMES_PER_CALL)
        .map(|n| ask(&format!("cas/l/{n}"), 1))
        .collect();

    let full = presence
        .check(&asks[..MAX_NAMES_PER_CALL])
        .await
        .expect("check");
    assert_eq!(filer.lookups(), [MAX_NAMES_PER_CALL]);
    let over = presence.check(&asks).await.expect("check");

    assert_eq!(full.len(), MAX_NAMES_PER_CALL);
    assert_eq!(over.len(), MAX_NAMES_PER_CALL + 1);
    assert!(over.iter().all(|answer| *answer == Presence::Absent));
    let mut lookups = filer.lookups();
    lookups.sort_unstable();
    assert_eq!(lookups, [1, MAX_NAMES_PER_CALL, MAX_NAMES_PER_CALL]);
}

/// Catches a truncated object answered present: an entry one byte short of the size asked
/// is short (with its length), one of exactly that size or longer is present.
#[tokio::test]
async fn an_entry_shorter_than_asked_is_short() {
    let filer = FakeFiler::start().await;
    filer.put_file(&format!("{ROOT}/cas/f/a"), 99);
    filer.put_file(&format!("{ROOT}/cas/f/b"), 100);
    filer.put_file(&format!("{ROOT}/cas/f/c"), 101);
    let asks = [
        ask("cas/f/a", 100),
        ask("cas/f/b", 100),
        ask("cas/f/c", 100),
    ];

    let answers = client(&filer).check(&asks).await.expect("check");

    assert_eq!(
        answers,
        [
            Presence::Short { size: 99 },
            Presence::Present { size: 100 },
            Presence::Present { size: 101 },
        ]
    );
}

/// Catches a directory taken for an object: a key whose entry is a directory is absent, as
/// is a key with no entry.
#[tokio::test]
async fn a_directory_or_no_entry_is_absent() {
    let filer = FakeFiler::start().await;
    let directory = Entry {
        is_directory: true,
        attributes: Some(FuseAttributes::default()),
        ..Entry::default()
    };
    filer.put_entry(&format!("{ROOT}/cas/b/1-0"), directory);

    let answers = client(&filer)
        .check(&[ask("cas/b/1-0", 0), ask("cas/b/2-0", 0)])
        .await
        .expect("check");

    assert_eq!(answers, [Presence::Absent, Presence::Absent]);
}

/// Catches an item the filer could not answer taken for an absence (which would forget a
/// pack over a volume lookup that failed) or for a presence: an item with an error is
/// unanswered with the filer's text, even when the filer also says it found the entry; a
/// found item with no entry is unanswered too.
#[tokio::test]
async fn an_item_the_filer_could_not_answer_is_unanswered() {
    let filer = FakeFiler::start().await;
    let failed = LookupDirectoryEntryResult {
        error: "volume 7 has no locations".to_owned(),
        ..LookupDirectoryEntryResult::default()
    };
    filer.answer_raw(&format!("{ROOT}/cas/l/a"), failed);
    let found_and_failed = LookupDirectoryEntryResult {
        found: true,
        entry: Some(Entry::default()),
        error: "entry chunk 0 has no file id".to_owned(),
        ..LookupDirectoryEntryResult::default()
    };
    filer.answer_raw(&format!("{ROOT}/cas/l/b"), found_and_failed);
    let found_without_entry = LookupDirectoryEntryResult {
        found: true,
        ..LookupDirectoryEntryResult::default()
    };
    filer.answer_raw(&format!("{ROOT}/cas/l/c"), found_without_entry);

    let answers = client(&filer)
        .check(&[ask("cas/l/a", 1), ask("cas/l/b", 1), ask("cas/l/c", 1)])
        .await
        .expect("check");

    assert_eq!(
        answers,
        [
            Presence::Unanswered(Unanswered::Filer("volume 7 has no locations".to_owned())),
            Presence::Unanswered(Unanswered::Filer("entry chunk 0 has no file id".to_owned())),
            Presence::Unanswered(Unanswered::NoEntry),
        ]
    );
}

/// Catches one key the filer cannot name failing the call of every other key (the filer
/// refuses a whole call over one bad name): a key with an empty segment or a NUL is not
/// sent and is answered `NotAPath`, and the others are asked and answered in place.
#[tokio::test]
async fn a_key_that_is_no_filer_path_is_not_sent() {
    let filer = FakeFiler::start().await;
    filer.put_file(&format!("{ROOT}/cas/l/ok"), 5);
    let not_paths = ["cas//x", "cas/x/", "/cas/x", "cas/a\0b"];
    let mut asks: Vec<Ask> = not_paths.iter().map(|key| ask(key, 1)).collect();
    asks.insert(2, ask("cas/l/ok", 5));

    let answers = client(&filer).check(&asks).await.expect("check");

    let not_a_path = Presence::Unanswered(Unanswered::NotAPath);
    assert_eq!(
        answers,
        [
            not_a_path.clone(),
            not_a_path.clone(),
            Presence::Present { size: 5 },
            not_a_path.clone(),
            not_a_path,
        ]
    );
    assert_eq!(filer.asked(), [asked("/buckets/kbf-cas/cas/l", "ok")]);
}

/// Catches a call the filer refuses for being empty: no asks, or only keys that are no
/// filer path, make no call at all.
#[tokio::test]
async fn nothing_to_ask_makes_no_call() {
    let filer = FakeFiler::start().await;
    let presence = client(&filer);

    let none = presence.check(&[]).await.expect("check");
    let unmapped = presence.check(&[ask("a//b", 1)]).await.expect("check");

    assert!(none.is_empty());
    assert_eq!(unmapped, [Presence::Unanswered(Unanswered::NotAPath)]);
    assert!(filer.lookups().is_empty(), "no call was made");
}

/// Catches a filer fault turned into absences (a storm of uploads and rebuilds): a refused
/// call fails the whole check, naming the filer, the names the call carried and the status.
#[tokio::test]
async fn a_refused_call_fails_the_check() {
    let filer = FakeFiler::start().await;
    filer.fail_lookups(Code::Unavailable, "filer store is down");
    let asks: Vec<Ask> = (0..5000).map(|n| ask(&format!("cas/l/{n}"), 1)).collect();

    let error = client(&filer).check(&asks).await.expect_err("refused");

    let FilerError::Call {
        filer: at,
        names,
        status,
    } = &error
    else {
        panic!("not a call error: {error:?}");
    };
    assert_eq!(*at, filer.address());
    assert!(
        *names == MAX_NAMES_PER_CALL || *names == 5000 - MAX_NAMES_PER_CALL,
        "{names} names"
    );
    assert_eq!(status.code(), Code::Unavailable);
    assert_eq!(status.message(), "filer store is down");
}

/// Catches answers matched to the wrong keys when the filer leaves one out: an answer with
/// fewer results than names fails the check.
#[tokio::test]
async fn an_answer_with_a_result_missing_fails_the_check() {
    let filer = FakeFiler::start().await;
    filer.drop_last_result();

    let error = client(&filer)
        .check(&[ask("cas/l/a", 1), ask("cas/l/b", 1)])
        .await
        .expect_err("short answer");

    assert_eq!(
        error.to_string(),
        format!(
            "the filer at {} answered 1 results to a lookup of 2 names",
            filer.address()
        )
    );
}

/// Catches an unreachable filer answered as "nothing there": the call fails.
#[tokio::test]
async fn an_unreachable_filer_fails_the_check() {
    let dead = kbf_testkit::port::dead_addr().to_string();
    let root = FilerRoot::new(ROOT).expect("root");
    let presence =
        FilerPresence::connect_lazy(&dead, root, Duration::from_secs(30)).expect("client");

    let error = presence
        .check(&[ask("cas/l/a", 1)])
        .await
        .expect_err("no filer");

    assert!(
        matches!(&error, FilerError::Call { names: 1, status, .. } if status.code() == Code::Unavailable),
        "{error:?}"
    );
}

fn chunk(offset: i64, size: u64) -> FileChunk {
    FileChunk {
        offset,
        size,
        ..FileChunk::default()
    }
}

/// Catches a length read from the wrong field, which would answer a whole pack short (or
/// a short one whole): the length is SeaweedFS's `FileSize`, the largest of the recorded
/// size, the end of the furthest chunk and the size of a newer remote copy; an older remote
/// copy, a negative offset or size, and an entry with no attributes count for nothing.
#[tokio::test]
async fn the_length_is_the_one_seaweedfs_answers_a_head_with() {
    let filer = FakeFiler::start().await;
    let attributes = |file_size, mtime| {
        Some(FuseAttributes {
            file_size,
            mtime,
            ..FuseAttributes::default()
        })
    };
    let remote = |remote_size, remote_mtime| {
        Some(RemoteEntry {
            remote_size,
            remote_mtime,
            ..RemoteEntry::default()
        })
    };
    let entries = [
        // The chunks reach further than the recorded size.
        Entry {
            attributes: attributes(10, 0),
            chunks: vec![chunk(0, 8), chunk(8, 12), chunk(4, 1)],
            ..Entry::default()
        },
        // A remote copy newer than the entry is longer.
        Entry {
            attributes: attributes(10, 5),
            remote_entry: remote(40, 6),
            ..Entry::default()
        },
        // A remote copy as old as the entry does not count.
        Entry {
            attributes: attributes(10, 5),
            remote_entry: remote(40, 5),
            ..Entry::default()
        },
        // A negative remote size and chunk offset count for nothing.
        Entry {
            attributes: attributes(3, 0),
            remote_entry: remote(-1, 1),
            chunks: vec![chunk(-5, 2)],
            ..Entry::default()
        },
        // No attributes: SeaweedFS reckons 0, whatever the chunks say.
        Entry {
            chunks: vec![chunk(0, 50)],
            ..Entry::default()
        },
    ];
    let mut asks = Vec::new();
    for (n, entry) in entries.into_iter().enumerate() {
        filer.put_entry(&format!("{ROOT}/e/{n}"), entry);
        asks.push(ask(&format!("e/{n}"), 1));
    }

    let answers = client(&filer).check(&asks).await.expect("check");

    assert_eq!(
        answers,
        [
            Presence::Present { size: 20 },
            Presence::Present { size: 40 },
            Presence::Present { size: 10 },
            Presence::Present { size: 3 },
            Presence::Short { size: 0 },
        ]
    );
}

/// Catches an address the client would dial wrongly (no port, a URL, port 0, text that is
/// no authority) accepted at start, where it would fail every later call instead.
#[tokio::test]
async fn the_address_must_be_host_and_port() {
    let root = FilerRoot::new(ROOT).expect("root");
    for bad in [
        "filer-host",
        ":18888",
        "http://filer-host:18888",
        "filer-host:0",
        "filer-host:port",
        "filer host:18888",
    ] {
        let made = FilerPresence::connect_lazy(bad, root.clone(), Duration::from_secs(1));
        let error = made.expect_err(bad);
        assert_eq!(
            error.to_string(),
            format!("the filer address {bad:?} is not <host>:<port>")
        );
    }
    for good in ["filer-host:18888", "127.0.0.1:1", "[::1]:18888"] {
        let made = FilerPresence::connect_lazy(good, root.clone(), Duration::from_secs(1));
        assert!(made.is_ok(), "{good}");
    }
}

/// Catches a root that would put every key in the wrong directory: it must be an absolute
/// path with no empty, `.` or `..` segment and no NUL, and is kept as given.
#[test]
fn the_root_must_be_an_absolute_directory() {
    for bad in [
        "",
        "/",
        "buckets/kbf-cas",
        "/buckets/kbf-cas/",
        "/buckets//kbf-cas",
        "/buckets/./kbf-cas",
        "/buckets/../kbf-cas",
        "/buckets/kbf\0cas",
    ] {
        let error = FilerRoot::new(bad).expect_err(bad);
        assert_eq!(
            error.to_string(),
            format!("the filer root {bad:?} is not a directory path such as /buckets/<bucket>")
        );
    }
    let root = FilerRoot::new("/buckets/kbf-cas").expect("root");
    assert_eq!(root.as_str(), "/buckets/kbf-cas");
}

/// Catches answers put back on the wrong asks when keys that are no filer path are skipped
/// in a check that is split: 5,000 asks with every seventh one `a//<n>` are calls of 4,096
/// and the rest of the good keys, and every ask gets its own answer.
#[tokio::test]
async fn skipped_keys_and_a_split_keep_every_answer_in_place() {
    let filer = FakeFiler::start().await;
    let mut asks = Vec::new();
    for n in 0..5000u64 {
        if n % 7 == 0 {
            asks.push(ask(&format!("a//{n}"), 1));
        } else {
            filer.put_file(&format!("{ROOT}/cas/l/{n}"), n);
            asks.push(ask(&format!("cas/l/{n}"), n));
        }
    }

    let answers = client(&filer).check(&asks).await.expect("check");

    for (n, answer) in (0..5000u64).zip(&answers) {
        let want = match n % 7 {
            0 => Presence::Unanswered(Unanswered::NotAPath),
            _ => Presence::Present { size: n },
        };
        assert_eq!(*answer, want, "ask {n}");
    }
    let good = 5000 - 5000usize.div_ceil(7);
    let mut lookups = filer.lookups();
    lookups.sort_unstable();
    assert_eq!(lookups, [good - MAX_NAMES_PER_CALL, MAX_NAMES_PER_CALL]);
}

/// Catches a partial answer taken for a whole one: when one of the calls of a split check
/// fails and the other is served, the check fails.
#[tokio::test]
async fn one_failed_call_of_several_fails_the_check() {
    let filer = FakeFiler::start().await;
    filer.fail_lookups_of(
        5000 - MAX_NAMES_PER_CALL,
        Code::Internal,
        "store read failed",
    );
    let asks: Vec<Ask> = (0..5000).map(|n| ask(&format!("cas/l/{n}"), 1)).collect();

    let error = client(&filer).check(&asks).await.expect_err("refused");

    let FilerError::Call { names, status, .. } = &error else {
        panic!("not a call error: {error:?}");
    };
    assert_eq!(*names, 5000 - MAX_NAMES_PER_CALL);
    assert_eq!(status.code(), Code::Internal);
    let mut lookups = filer.lookups();
    lookups.sort_unstable();
    assert_eq!(lookups, [5000 - MAX_NAMES_PER_CALL, MAX_NAMES_PER_CALL]);
}

/// Catches a client with no limit on a call: a filer that accepts the connection and never
/// answers fails the check once the client's timeout has passed, rather than holding it
/// for ever. Nothing is asserted about how long that took.
#[tokio::test]
async fn a_filer_that_never_answers_fails_the_check() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address").to_string();
    // Accept and hold every connection, reading nothing and writing nothing.
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let root = FilerRoot::new(ROOT).expect("root");
    let presence =
        FilerPresence::connect_lazy(&address, root, Duration::from_millis(50)).expect("client");

    let error = presence
        .check(&[ask("cas/l/a", 1)])
        .await
        .expect_err("silent filer");

    assert!(
        matches!(&error, FilerError::Call { names: 1, .. }),
        "{error:?}"
    );
}

/// Catches tonic's default limit of 4 MiB on an answer, under which a call of 4,096 entries
/// with long chunk lists would fail on a real filer: an answer of more than 8 MiB is read.
#[tokio::test]
async fn an_answer_over_4_mib_is_read() {
    let filer = FakeFiler::start().await;
    let big = Entry {
        attributes: Some(FuseAttributes {
            file_size: 7,
            ..FuseAttributes::default()
        }),
        chunks: vec![FileChunk {
            e_tag: "e".repeat(8 << 20),
            size: 7,
            ..FileChunk::default()
        }],
        ..Entry::default()
    };
    filer.put_entry(&format!("{ROOT}/cas/l/big"), big);

    let answers = client(&filer)
        .check(&[ask("cas/l/big", 7)])
        .await
        .expect("check");

    assert_eq!(answers, [Presence::Present { size: 7 }]);
}
