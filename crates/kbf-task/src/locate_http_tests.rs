//! `/locate` and the store over HTTP, against fake servers.

use super::*;

// `/locate` over HTTP.

fn plain_listener(
    handler: impl Fn(&crate::fake_http::Seen) -> Answer + Send + Sync + 'static,
) -> (Fake, HttpBlobSource) {
    let fake = Fake::start(handler);
    let source =
        HttpBlobSource::new_with(&fake.base, "tok", &[vec![1, 2, 3]], true).expect("source");
    (fake, source)
}

/// Catches the request being wrong: `GET /locate` with the bearer token, and the body returned.
#[test]
fn locate_asks_the_listener_with_the_bearer() {
    let (fake, source) = plain_listener(|_| Answer::new(200, "the table"));
    assert_eq!(source.locate().expect("locate"), b"the table");
    let seen = fake.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("GET", "/locate")
    );
    assert_eq!(seen[0].header("authorization"), Some("Bearer tok"));
}

/// Catches an answer other than 200 being used, and an over-long body being read whole: 503,
/// 404 and 401 are errors naming the status and not the body, and a body past the cap is refused.
#[test]
fn locate_is_an_error_unless_the_listener_answers_200_within_the_cap() {
    for status in [503u16, 404, 401] {
        let (_fake, source) = plain_listener(move |_| Answer::new(status, "store_unavailable"));
        let error = source.locate().expect_err("refused").to_string();
        assert!(error.contains(&status.to_string()), "{error}");
        assert!(!error.contains("store_unavailable"), "{error}");
    }
    let (_fake, source) = plain_listener(|_| Answer::new(200, vec![b'x'; 11]));
    assert_eq!(source.locate_within(11).expect("at the cap").len(), 11);
    let error = source.locate_within(10).expect_err("over").to_string();
    assert!(error.contains("10"), "{error}");
}

/// Catches a listener that cannot be reached being an error that stops the run: it is an error
/// value, which `open` turns into an empty table.
#[test]
fn locate_of_a_listener_that_is_not_there_is_an_error() {
    let free = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", free.local_addr().expect("addr"));
    drop(free);
    let source = HttpBlobSource::new_with(&base, "tok", &[vec![1, 2, 3]], true).expect("source");
    assert!(source.locate().is_err());
}

/// A listener that serves `blobs` by `/<hex>/<size>` and `locate` at `/locate`.
fn listener_serving(blobs: &[Vec<u8>], locate: Answer) -> (Fake, HttpBlobSource) {
    let by_path: HashMap<String, Vec<u8>> = blobs
        .iter()
        .map(|b| (format!("/{}", label(&digest_of(b))), b.clone()))
        .collect();
    let locate = Arc::new(Mutex::new(Some(locate)));
    plain_listener(move |seen| {
        if seen.path == "/locate" {
            let answer = locate.lock().expect("locate");
            let answer = answer.as_ref().expect("answer");
            return Answer::new(answer.status, answer.body.clone());
        }
        match by_path.get(&seen.path) {
            Some(body) => Answer::new(200, body.clone()),
            None => Answer::new(404, ""),
        }
    })
}

/// A store server holding `objects`, answering ranges as the store does: 206 for a `Range`, 200
/// for none, 403 for a missing grant header.
fn store_serving(objects: HashMap<String, Vec<u8>>) -> Fake {
    Fake::start(move |seen| {
        if seen.header("x-tierstore-grant").is_none() {
            return Answer::new(403, "");
        }
        let Some(object) = seen
            .path
            .strip_prefix("/b/")
            .and_then(|key| objects.get(key))
        else {
            return Answer::new(404, "");
        };
        match seen.header("range") {
            None => Answer::new(200, object.clone()),
            Some(range) => {
                let (from, to) = range
                    .strip_prefix("bytes=")
                    .and_then(|r| r.split_once('-'))
                    .expect("range");
                let (from, to): (usize, usize) =
                    (from.parse().expect("from"), to.parse().expect("to"));
                Answer::new(206, object[from..=to].to_vec())
            }
        }
    })
}

fn open(listener: HttpBlobSource, dir: &Path) -> LocatedSource {
    LocatedSource::open(listener, &[vec![1, 2, 3]], true, dir.to_path_buf())
}

/// Catches `open` asking more than once, or not at all, or reading the listener for a located
/// blob, or the store being the answer's: one `/locate`, the located blob from the store the
/// answer names (with the grant its line names), the other from the listener.
#[test]
fn open_asks_locate_once_and_reads_where_it_points() {
    let blobs = bodies(2);
    let (pack, offsets) = pack_of(&blobs[..1]);
    let store = store_serving(HashMap::from([("cas/f/w/p0".to_owned(), pack)]));
    let digests: Vec<Digest> = blobs.iter().map(|b| digest_of(b)).collect();
    let body = [
        header_line(&store.base),
        pack_line_g(&digests[0], "cas/f/w/p0", offsets[0], 1),
        absent_line(&digests[1]),
    ]
    .join("\n");
    let (fake, listener) = listener_serving(&blobs, Answer::new(200, body));
    let dir = scratch("open-once");
    let source = open(listener, &dir);
    assert!(source.get(&digests[0]).expect("first") == blobs[0]);
    assert!(source.get(&digests[1]).expect("second") == blobs[1]);
    let paths = fake.paths();
    assert_eq!(paths.iter().filter(|p| *p == "/locate").count(), 1);
    assert_eq!(paths.len(), 2);
    assert_eq!(paths[1], format!("/{}", label(&digests[1])));
    let seen = store.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].header("x-tierstore-grant"), Some("grant-2"));
    assert_eq!(seen[0].path, "/b/cas/f/w/p0");
    assert_eq!(source.counts().range, 1);
    // The listener is never given a store grant.
    assert!(
        fake.requests()
            .iter()
            .all(|r| r.header("x-tierstore-grant").is_none())
    );
}

/// Catches an unusable `/locate` stopping the run: a 503, a 404, a body that is no table and a
/// table with no grants all leave every blob to the listener, and the report names why.
#[test]
fn a_locate_that_fails_leaves_every_blob_to_the_listener() {
    let blobs = bodies(2);
    let digests: Vec<Digest> = blobs.iter().map(|b| digest_of(b)).collect();
    let (pack, offsets) = pack_of(&blobs[..1]);
    let store = store_serving(HashMap::from([("cas/f/w/p0".to_owned(), pack)]));
    let head = format!(r#"{{"store":"{}","bucket":"b","grants":[]}}"#, store.base);
    let no_grants = format!(
        "{head}\n{}",
        pack_line(&digests[0], "cas/f/w/p0", offsets[0])
    );
    for answer in [
        Answer::new(503, "store_unavailable\n"),
        Answer::new(404, ""),
        Answer::new(200, "this is not a table\n"),
        Answer::new(200, no_grants),
    ] {
        let status = answer.status;
        let (fake, listener) = listener_serving(&blobs, answer);
        let dir = scratch("locate-fails");
        let source = open(listener, &dir);
        for (d, blob) in digests.iter().zip(&blobs) {
            assert!(source.get(d).expect("get") == *blob);
        }
        assert_eq!(source.counts().absent, 2);
        assert_eq!(fake.paths().len(), 3);
        let report = source.report().expect("report");
        if status != 200 {
            assert!(report.contains(&status.to_string()), "{report}");
        }
    }
    assert!(store.requests().is_empty());
}

/// Catches a store the task should not use being used: an `http` store without the opt-in, a
/// store that is not a URL, and a bucket that is not a plain name all leave the listener to serve
/// every blob and the store unasked.
#[test]
fn a_store_the_task_will_not_use_is_not_used() {
    let blobs = bodies(1);
    let digests: Vec<Digest> = blobs.iter().map(|b| digest_of(b)).collect();
    let (pack, offsets) = pack_of(&blobs);
    let store = store_serving(HashMap::from([("cas/f/w/p0".to_owned(), pack)]));
    let entry = pack_line(&digests[0], "cas/f/w/p0", offsets[0]);
    let cases = [
        (
            format!(
                r#"{{"store":"{}","bucket":"b","grants":["g"]}}"#,
                store.base
            ),
            false,
        ),
        (
            r#"{"store":"ftp://x","bucket":"b","grants":["g"]}"#.to_owned(),
            true,
        ),
        (
            format!(
                r#"{{"store":"{}","bucket":"Bad Name","grants":["g"]}}"#,
                store.base
            ),
            true,
        ),
    ];
    for (head, allow_plain) in cases {
        let (_fake, listener) =
            listener_serving(&blobs, Answer::new(200, format!("{head}\n{entry}")));
        let dir = scratch("store-unused");
        let source =
            LocatedSource::open(listener, &[vec![1, 2, 3]], allow_plain, dir.to_path_buf());
        assert!(source.get(&digests[0]).expect("get") == blobs[0]);
        assert_eq!(source.counts().absent, 1, "{head}");
    }
    assert!(store.requests().is_empty());
}

// The store over HTTP.

fn store_at(fake: &Fake, bucket: &str) -> HttpObjectStore {
    HttpObjectStore::new(&fake.base, bucket, &[vec![1, 2, 3]], true).expect("store")
}

/// Catches the wrong request to the store: path-style `/<bucket>/<key>`, a `Range` of the right
/// bytes, the grant in `x-tierstore-grant` and no `Authorization`; a 206 is the bytes asked for.
#[test]
fn the_store_is_asked_for_a_range_with_the_grant_header() {
    let object: Vec<u8> = (0..100u8).collect();
    let body = object.clone();
    let fake = Fake::start(move |_| Answer::new(206, body[10..20].to_vec()));
    let store = store_at(&fake, "kbf-bucket");
    let mut out = Vec::new();
    store
        .read(
            "cas/f/w/p0",
            Want::Range {
                offset: 10,
                length: 10,
            },
            "the-grant",
            &mut out,
        )
        .expect("range");
    assert_eq!(out, object[10..20]);
    let seen = &fake.requests()[0];
    assert_eq!(
        (seen.method.as_str(), seen.path.as_str()),
        ("GET", "/kbf-bucket/cas/f/w/p0")
    );
    assert_eq!(seen.header("range"), Some("bytes=10-19"));
    assert_eq!(seen.header("x-tierstore-grant"), Some("the-grant"));
    assert_eq!(seen.header("authorization"), None);
}

/// Catches a whole object read with a `Range`, or a 200 taken for a range: a whole read has no
/// `Range` header and wants 200; a range read wants 206.
#[test]
fn a_whole_read_wants_200_and_a_range_read_wants_206() {
    let fake = Fake::start(|_| Answer::new(200, "whole"));
    let store = store_at(&fake, "b");
    let mut out = Vec::new();
    store
        .read("cas/l/abc", Want::Whole { size: 5 }, "g", &mut out)
        .expect("whole");
    assert_eq!(out, b"whole");
    assert_eq!(fake.requests()[0].header("range"), None);
    let error = store
        .read(
            "cas/f/p",
            Want::Range {
                offset: 0,
                length: 5,
            },
            "g",
            &mut Vec::new(),
        )
        .expect_err("200 for a range")
        .to_string();
    assert!(error.contains("200"), "{error}");
    let fake = Fake::start(|_| Answer::new(206, "part"));
    let error = store_at(&fake, "b")
        .read("cas/l/abc", Want::Whole { size: 4 }, "g", &mut Vec::new())
        .expect_err("206 for whole")
        .to_string();
    assert!(error.contains("206"), "{error}");
}

/// Catches a refusal being read as bytes: 403 and 404 are errors naming the status, and an
/// unreachable store is an error, and a grant that cannot be a header value is one.
#[test]
fn a_refusal_or_an_unreachable_store_is_an_error() {
    for status in [403u16, 404, 500] {
        let fake = Fake::start(move |_| Answer::new(status, "AccessDenied"));
        let error = store_at(&fake, "b")
            .read("cas/l/abc", Want::Whole { size: 4 }, "g", &mut Vec::new())
            .expect_err("refused")
            .to_string();
        assert!(error.contains(&status.to_string()), "{error}");
        assert!(!error.contains("AccessDenied"), "{error}");
    }
    let free = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", free.local_addr().expect("addr"));
    drop(free);
    let store = HttpObjectStore::new(&base, "b", &[vec![1, 2, 3]], true).expect("store");
    assert!(
        store
            .read("cas/l/abc", Want::Whole { size: 4 }, "g", &mut Vec::new())
            .is_err()
    );
    let fake = Fake::start(|_| Answer::new(200, "x"));
    assert!(
        store_at(&fake, "b")
            .read(
                "cas/l/abc",
                Want::Whole { size: 1 },
                "bad\nvalue",
                &mut Vec::new()
            )
            .is_err()
    );
}

/// Catches a store that cannot write: when the writer refuses, the read is an error.
#[test]
fn a_writer_that_refuses_ends_the_store_read() {
    let fake = Fake::start(|_| Answer::new(200, "data"));
    let error = store_at(&fake, "b")
        .read("cas/l/abc", Want::Whole { size: 4 }, "g", &mut Refuses)
        .expect_err("refused")
        .to_string();
    assert!(error.contains("full"), "{error}");
}

/// Catches a store made from a bad name, a bad scheme or with no roots: no roots, a bucket that
/// is empty or has a slash, a capital or a space, and an endpoint that is not `https` (or `http`
/// with the opt-in) are errors, and a trailing slash on the endpoint is dropped.
#[test]
fn a_store_needs_roots_a_plain_bucket_name_and_a_scheme_it_may_use() {
    let roots = [vec![1, 2, 3]];
    assert!(HttpObjectStore::new("http://h:1", "b", &[], true).is_err());
    let long = "a".repeat(64);
    for bucket in [
        "",
        "a/b",
        "Upper",
        "a b",
        "a?b",
        long.as_str(),
        ".",
        "..",
        ".a",
        "a.",
    ] {
        assert!(
            HttpObjectStore::new("http://h:1", bucket, &roots, true).is_err(),
            "{bucket:?}"
        );
    }
    assert!(HttpObjectStore::new("http://h:1/", "my-bucket.1", &roots, true).is_ok());
    assert!(HttpObjectStore::new("HTTPS://h:1", "b", &roots, false).is_ok());
    assert!(HttpObjectStore::new("http://h:1", "b", &roots, false).is_err());
    assert!(HttpObjectStore::new("ftp://h:1", "b", &roots, true).is_err());
    for endpoint in [
        "https://h:1?x=1",
        "https://h:1#frag",
        "https://user:pw@h:1",
        "https://h:1 ",
        "https://h:1\t",
        "https://h:1\n",
        "https://h:1\\x",
        "https://h\u{e9}:1",
    ] {
        assert!(
            HttpObjectStore::new(endpoint, "b", &roots, true).is_err(),
            "{endpoint:?}"
        );
    }
    let fake = Fake::start(|_| Answer::new(200, "x"));
    let store = HttpObjectStore::new(&format!("{}/", fake.base), "b", &roots, true).expect("store");
    store
        .read("cas/l/abc", Want::Whole { size: 1 }, "g", &mut Vec::new())
        .expect("read");
    assert_eq!(fake.paths(), vec!["/b/cas/l/abc".to_owned()]);
}

/// Catches a writer that fails after a good read being taken for a bad blob: when the bytes
/// cannot be written out the call fails with a source error and the listener is not asked, for a
/// span and for a large object alike.
#[test]
fn a_writer_that_refuses_a_checked_blob_fails_without_a_fallback() {
    let big = vec![5u8; BIG];
    let small = b"a small blob".to_vec();
    let (pack, offsets) = pack_of(std::slice::from_ref(&small));
    let store = Arc::new(FakeStore::default());
    store.put("cas/f/w/p0", pack);
    let (big_digest, small_digest) = (digest_of(&big), digest_of(&small));
    store.put(&format!("cas/l/{}", big_digest.hash), big);
    let mut mem = Memory::default();
    mem.put(vec![5u8; BIG]);
    mem.put(small.clone());
    let mem = Arc::new(mem);
    let lines = vec![
        pack_line(&small_digest, "cas/f/w/p0", offsets[0]),
        large_line(&big_digest),
    ];
    let dir = scratch("located-refused-writer");
    let source = located(&mem, &store, &lines, &dir);
    for digest in [&small_digest, &big_digest] {
        let error = source.fetch_to(digest, &mut Refuses).expect_err("refused");
        assert!(matches!(error, FetchError::Source { .. }), "{error:?}");
        assert_eq!(mem.reads(digest), 0);
    }
    assert_eq!(source.counts().fallback, 0);
}

/// Catches an answer cut short being used: the listener ends the body early and the error is a
/// `/locate` error.
#[test]
fn a_locate_answer_cut_short_is_an_error() {
    let (_fake, source) = plain_listener(|_| Answer::cut(200, "the start of a table"));
    let error = source.locate().expect_err("cut").to_string();
    assert!(error.starts_with("/locate: "), "{error}");
}

/// Catches a redirect from the store being followed: the grant would go to whatever host the
/// `Location` names. The store answers 302 to a second server; the read is an error and the
/// second server sees no request, and through `open` the blob is read from the listener.
#[test]
fn a_redirect_from_the_store_is_an_error_and_is_not_followed() {
    let elsewhere = Fake::start(|_| Answer::new(206, "stolen"));
    let target = format!("{}/b/cas/f/w/p0", elsewhere.base);
    let store = Fake::start(move |_| Answer::redirect(302, &target));
    let error = store_at(&store, "b")
        .read(
            "cas/f/w/p0",
            Want::Range {
                offset: 0,
                length: 6,
            },
            "the-grant",
            &mut Vec::new(),
        )
        .expect_err("redirect")
        .to_string();
    assert!(error.contains("302"), "{error}");
    assert!(elsewhere.requests().is_empty());

    let blobs = bodies(1);
    let (_, offsets) = pack_of(&blobs);
    let d = digest_of(&blobs[0]);
    let body = [
        header_line(&store.base),
        pack_line(&d, "cas/f/w/p0", offsets[0]),
    ]
    .join("\n");
    let (_fake, listener) = listener_serving(&blobs, Answer::new(200, body));
    let dir = scratch("store-redirect");
    let source = open(listener, &dir);
    assert!(source.get(&d).expect("get") == blobs[0]);
    assert_eq!(source.counts().fallback, 1);
    assert!(elsewhere.requests().is_empty());
}
