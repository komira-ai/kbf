//! Route matching, and what a read answers for each way it can fail.

use std::future::{Future, ready};

use futures::executor::block_on;
use serde_json::Value;

use super::{Page, ReadError, Reader, error_message, matches};
use crate::tests::Fake;
use crate::{Fetched, Source, SourceError};

/// Catches: a pattern segment matching more or less than one non-empty segment
/// (catches: each condition of the `{name}` guard dropped, a prefix match), or a
/// dot segment, in any spelling, matching one (catches: the dot check dropped); a
/// path with a backslash, whitespace or a control character matching (catches: the
/// character check dropped).
#[test]
fn a_pattern_matches_segment_by_segment() {
    let cases = [
        ("/v1/invocations/{id}", "/v1/invocations/b1", true),
        ("/v1/invocations/{id}", "/v1/invocations/", false),
        ("/v1/invocations/{id}", "/v1/invocations/b1/actions", false),
        ("/v1/invocations/{id}/actions", "/v1/invocations/b1", false),
        ("/v1/invocations/{id}", "/v1/invocations/{id}", true),
        ("/v1/invocations/{id", "/v1/invocations/b1", false),
        ("/v1/invocations/id}", "/v1/invocations/b1", false),
        ("/v1/blobs/{digest}", "/v2/blobs/x", false),
        // Dot segments in every spelling a URL client resolves (WHATWG URL 4.4).
        ("/v1/invocations/{id}", "/v1/invocations/.", false),
        ("/v1/invocations/{id}", "/v1/invocations/..", false),
        ("/v1/invocations/{id}", "/v1/invocations/%2e", false),
        ("/v1/invocations/{id}", "/v1/invocations/%2E%2e", false),
        ("/v1/invocations/{id}", "/v1/invocations/.%2E", false),
        ("/v1/invocations/{id}", "/v1/invocations/%2e.", false),
        (
            "/v1/invocations/{id}/actions",
            "/v1/invocations/../actions",
            false,
        ),
        // A backslash (a WHATWG URL client reads it as `/` in a path), whitespace or a
        // control character: refused, as `Href::root_relative` refuses them.
        ("/v1/blobs/{digest}", "/v1/blobs/..\\x", false),
        ("/v1/blobs/{digest}", "/v1/blobs/a\\b", false),
        ("/v1/blobs/{digest}", "/v1/blobs/a b", false),
        ("/v1/blobs/{digest}", "/v1/blobs/a\tb", false),
        ("/v1/blobs/{digest}", "/v1/blobs/a\u{7f}b", false),
        // Not dot segments: matched as any other id.
        ("/v1/invocations/{id}", "/v1/invocations/...", true),
        ("/v1/invocations/{id}", "/v1/invocations/.a", true),
        ("/v1/invocations/{id}", "/v1/invocations/%252e", true),
    ];
    for (pattern, path, want) in cases {
        assert_eq!(matches(pattern, path), want, "{pattern} {path}");
    }
    let page = Page {
        name: "p",
        reads: &["/v1/a/{x}", "/v1/b"],
    };
    assert!(page.declares("/v1/b?x=1"));
    assert!(page.declares("/v1/a/y"));
    assert!(!page.declares("/v1/c"));
}

const PAGE: Page = Page {
    name: "p",
    reads: &["/v1/doc"],
};

/// Catches: a failed read answered as a document, or with another status than its
/// kind's (catches: a non-200 status treated as success; `status` mapping changed).
#[test]
fn each_failure_has_its_kind_and_status() {
    let fake = Fake::default().down("/v1/doc");
    let e = block_on(Reader::new(&fake, &PAGE).json::<Value>("/v1/doc")).expect_err("down");
    assert!(matches!(&e, ReadError::Unreachable { .. }), "{e}");
    assert_eq!((e.status(), e.is_page_bug()), (502, false));
    assert_eq!(e.to_string(), "/v1/doc: connection refused");

    let fake = Fake::default().with("/v1/doc", 200, "not json");
    let e = block_on(Reader::new(&fake, &PAGE).json::<Value>("/v1/doc")).expect_err("malformed");
    assert!(matches!(&e, ReadError::Malformed { .. }), "{e}");
    assert_eq!(e.status(), 502);

    let fake = Fake::default().with("/v1/doc", 403, r#"{"error": "denied"}"#);
    let e = block_on(Reader::new(&fake, &PAGE).text("/v1/doc")).expect_err("refused");
    assert_eq!(e.to_string(), "/v1/doc answered 403: denied");
    assert_eq!(e.status(), 403);

    let fake = Fake::default().with("/v1/doc", 200, b"a\xffb".to_vec());
    let text = block_on(Reader::new(&fake, &PAGE).text("/v1/doc")).expect("text");
    assert_eq!(text, "a\u{fffd}b");
}

/// Catches: an API error body shown as raw JSON when it has an `error`, or the
/// `records_since` of a missing record dropped.
#[test]
fn an_error_body_is_shown_by_its_error_string() {
    assert_eq!(error_message(br#"{"error": "denied"}"#), "denied");
    assert_eq!(
        error_message(br#"{"error": "no_record", "records_since": "2026-10-11T09:00:00Z"}"#),
        "no_record: no record of it is held; records are held since 2026-10-11T09:00:00Z"
    );
    assert_eq!(error_message(br#"{"other": 1}"#), r#"{"other": 1}"#);
    assert_eq!(error_message(b"plain"), "plain");
}

/// An error of `status` on `url` with the message of `body`.
fn status_error(url: &str, status: u16, body: &str) -> ReadError {
    ReadError::Status {
        url: url.to_owned(),
        status,
        message: error_message(body.as_bytes()),
        headers: Vec::new(),
    }
}

/// Catches: "history is off" said of a record the history does not hold, of a route that
/// is off by policy, of a route that is not a history route, or of a status that is not
/// `404`; and not said of the history routes the farm lacks (they answer as an unknown
/// route: `404` with no body). Catches: the status check dropped; the empty-message check
/// dropped; a route of the list dropped; the `/`-boundary check dropped (`/v1/runsx`).
#[test]
fn history_is_off_only_when_a_history_route_is_missing() {
    let missing = [
        "/v1/invocations",
        "/v1/invocations?page_size=10&state=open",
        "/v1/tests?page_size=50",
        "/v1/shapes/ab?instance=main&cfg=c",
        "/v1/shapes",
        "/v1/grid?keys=a",
        "/v1/runs/0a0b/logs/stderr",
        "/v1/runs",
    ];
    for url in missing {
        assert!(status_error(url, 404, "").is_history_off(), "{url}");
    }
    let said = status_error(
        "/v1/tests",
        404,
        r#"{"error": "history is off: use a flag"}"#,
    );
    assert!(said.is_history_off());
    let not = [
        // A record the history does not hold has its own code.
        status_error("/v1/shapes/ab", 404, r#"{"error": "no_shape"}"#),
        status_error("/v1/runs/0a0b", 404, r#"{"error": "no_run", "id": "0a0b"}"#),
        status_error("/v1/runs/0a0b/logs/stderr", 404, r#"{"error": "no_log"}"#),
        // The routes off by policy are not the history's.
        status_error(
            "/v1/invocations",
            404,
            r#"{"error": "pages are off", "off": "invocations"}"#,
        ),
        // One build: a route every farm has.
        status_error("/v1/invocations/b1", 404, ""),
        status_error("/v1/nodes", 404, ""),
        status_error("/v1/runsx", 404, ""),
        status_error("/v1/testsx", 404, ""),
        status_error("/v1/invocations", 503, ""),
        status_error("/v1/invocations", 200, ""),
        ReadError::Unreachable {
            url: "/v1/invocations".to_owned(),
            why: "down".to_owned(),
        },
        ReadError::Malformed {
            url: "/v1/invocations".to_owned(),
            why: "bad".to_owned(),
        },
    ];
    for e in not {
        assert!(!e.is_history_off(), "{e}");
    }
}

/// Catches: a ranged read that is not read as the server answers it. A `206` is its bytes
/// with the size from `Content-Range` (any case of the name), a `416` has no bytes and the
/// size from its `Content-Range`, a `200` is the whole and its length is the size, a
/// `Content-Range` that does not parse leaves the size unknown, any other status is the
/// error, and an undeclared URL is refused unsent. Catches: the `416` body kept; the `206`
/// size taken from the body; the header name matched case-sensitively; `/` split from the
/// left; a refusal read as bytes.
#[test]
fn a_ranged_read_is_read_as_the_server_answers_it() {
    let ask = |fake: &Fake| block_on(Reader::new(fake, &PAGE).range("/v1/doc", 0, 9));
    let part =
        Fake::default().with_headers("/v1/doc", 206, &[("content-range", "bytes 0-2/100")], "abc");
    let got = ask(&part).expect("a slice");
    assert_eq!((got.bytes.as_slice(), got.total), (&b"abc"[..], Some(100)));
    assert_eq!(part.ranges(), [("/v1/doc".to_owned(), 0, 9)]);

    let shouting =
        Fake::default().with_headers("/v1/doc", 206, &[("CONTENT-RANGE", "bytes 0-2/7")], "abc");
    assert_eq!(ask(&shouting).expect("a slice").total, Some(7));

    let past = Fake::default().with_headers(
        "/v1/doc",
        416,
        &[("Content-Range", "bytes */0")],
        "not sent",
    );
    let got = ask(&past).expect("a slice");
    assert_eq!((got.bytes.is_empty(), got.total), (true, Some(0)));

    let whole = Fake::default().with("/v1/doc", 200, "abcd");
    let got = ask(&whole).expect("a slice");
    assert_eq!((got.bytes.as_slice(), got.total), (&b"abcd"[..], Some(4)));

    let odd =
        Fake::default().with_headers("/v1/doc", 206, &[("Content-Range", "bytes 0-2/x")], "abc");
    assert_eq!(ask(&odd).expect("a slice").total, None);
    let bare = Fake::default().with("/v1/doc", 206, "abc");
    assert_eq!(ask(&bare).expect("a slice").total, None);

    let busy = Fake::default().with("/v1/doc", 503, r#"{"error": "busy"}"#);
    let e = ask(&busy).expect_err("refused");
    assert_eq!(
        (e.status(), e.to_string().as_str()),
        (503, "/v1/doc answered 503: busy")
    );

    let fake = Fake::default();
    let e = block_on(Reader::new(&fake, &PAGE).range("/v1/other", 0, 9)).expect_err("refused");
    assert!(e.is_page_bug(), "{e}");
    assert!(fake.seen().is_empty() && fake.ranges().is_empty());

    let down = Fake::default().down("/v1/doc");
    assert_eq!(ask(&down).expect_err("down").status(), 502);
}

/// A source that sends no header: it keeps the default `get_range`.
struct Plain(Result<Fetched, String>);

impl Source for Plain {
    fn get(&self, _: &str) -> impl Future<Output = Result<Fetched, SourceError>> + Send {
        ready(self.0.clone().map_err(SourceError))
    }
}

/// Catches: the default `get_range` showing more than was asked for, or losing the size.
/// A `200` is answered as the server would answer the range: `206` with the bytes and a
/// `Content-Range` naming the whole (the range clamped to the end), `416` with the size
/// for a start past the end, an empty `200` as it is, any other status untouched. Catches:
/// the body not cut; `last` taken as exclusive; the `416` branch dropped; the header
/// dropped; the status check dropped.
#[test]
fn the_default_ranged_read_cuts_the_whole_answer_to_the_range() {
    let fetched = |status: u16, body: &str| Fetched {
        status,
        body: body.as_bytes().to_vec(),
        headers: vec![("x-other".to_owned(), "kept?".to_owned())],
    };
    let range = |plain: &Plain, first: u64, last: u64| {
        block_on(plain.get_range("/v1/doc", first, last)).expect("an answer")
    };
    let content_range = |f: &Fetched| {
        f.headers
            .iter()
            .find(|(k, _)| k == "content-range")
            .map(|(_, v)| v.clone())
    };

    let plain = Plain(Ok(fetched(200, "abcdefghij")));
    let got = range(&plain, 2, 4);
    assert_eq!((got.status, got.body.as_slice()), (206, &b"cde"[..]));
    assert_eq!(content_range(&got).as_deref(), Some("bytes 2-4/10"));
    let got = range(&plain, 0, 99);
    assert_eq!((got.status, got.body.len()), (206, 10));
    assert_eq!(content_range(&got).as_deref(), Some("bytes 0-9/10"));
    let got = range(&plain, 9, 9);
    assert_eq!(got.body, b"j");
    let got = range(&plain, 10, 20);
    assert_eq!((got.status, got.body.is_empty()), (416, true));
    assert_eq!(content_range(&got).as_deref(), Some("bytes */10"));

    let empty = range(&Plain(Ok(fetched(200, ""))), 0, 9);
    assert_eq!((empty.status, empty.body.is_empty()), (200, true));
    let refused = range(&Plain(Ok(fetched(404, "no"))), 0, 9);
    assert_eq!((refused.status, refused.body.as_slice()), (404, &b"no"[..]));
    assert_eq!(content_range(&refused), None);

    let down = Plain(Err("connection refused".to_owned()));
    let e = block_on(down.get_range("/v1/doc", 0, 1)).expect_err("down");
    assert_eq!(e.0, "connection refused");
}

/// Catches: the default `get_suffix` showing more than was asked for, the wrong end, or
/// losing the size. A `200` is answered as the server answers `bytes=-n`: `206` with the
/// last `n` bytes and a `Content-Range` naming the whole (`n` clamped to the size), `416`
/// with the size for an `n` of zero, an empty `200` as it is, any other status untouched.
/// Catches: the front kept instead of the end; `n` not clamped (a panic on a short log);
/// the first byte of the range off by one; the `n == 0` branch dropped; the header
/// dropped; the status check dropped.
#[test]
fn the_default_suffix_read_cuts_the_whole_answer_to_its_end() {
    let fetched = |status: u16, body: &str| Fetched {
        status,
        body: body.as_bytes().to_vec(),
        headers: vec![("x-other".to_owned(), "kept?".to_owned())],
    };
    let suffix =
        |plain: &Plain, n: u64| block_on(plain.get_suffix("/v1/doc", n)).expect("an answer");
    let content_range = |f: &Fetched| {
        f.headers
            .iter()
            .find(|(k, _)| k == "content-range")
            .map(|(_, v)| v.clone())
    };

    let plain = Plain(Ok(fetched(200, "abcdefghij")));
    let got = suffix(&plain, 3);
    assert_eq!((got.status, got.body.as_slice()), (206, &b"hij"[..]));
    assert_eq!(content_range(&got).as_deref(), Some("bytes 7-9/10"));
    let got = suffix(&plain, 99);
    assert_eq!((got.status, got.body.len()), (206, 10));
    assert_eq!(content_range(&got).as_deref(), Some("bytes 0-9/10"));
    let got = suffix(&plain, 1);
    assert_eq!(got.body, b"j");
    assert_eq!(content_range(&got).as_deref(), Some("bytes 9-9/10"));
    let got = suffix(&plain, 0);
    assert_eq!((got.status, got.body.is_empty()), (416, true));
    assert_eq!(content_range(&got).as_deref(), Some("bytes */10"));

    let empty = suffix(&Plain(Ok(fetched(200, ""))), 5);
    assert_eq!((empty.status, empty.body.is_empty()), (200, true));
    let refused = suffix(&Plain(Ok(fetched(404, "no"))), 5);
    assert_eq!((refused.status, refused.body.as_slice()), (404, &b"no"[..]));
    assert_eq!(content_range(&refused), None);

    let down = Plain(Err("connection refused".to_owned()));
    let e = block_on(down.get_suffix("/v1/doc", 1)).expect_err("down");
    assert_eq!(e.0, "connection refused");
}

/// Catches: a suffix read that is not read as the server answers it, and one that
/// reaches the source as a span or a plain read. A `206` is its bytes with the size from
/// `Content-Range`, a `416` has no bytes and the size, a `200` is the whole and its length
/// the size, any other status the error; an undeclared URL is refused unsent. Catches:
/// `suffix` asking for a span (`Ask::Span`); the `416` body kept; a refusal read as bytes.
#[test]
fn a_suffix_read_is_asked_as_a_suffix_and_read_as_the_server_answers_it() {
    let ask = |fake: &Fake| block_on(Reader::new(fake, &PAGE).suffix("/v1/doc", 9));
    let part = Fake::default().with_headers(
        "/v1/doc",
        206,
        &[("content-range", "bytes 91-99/100")],
        "abc",
    );
    let got = ask(&part).expect("a slice");
    assert_eq!((got.bytes.as_slice(), got.total), (&b"abc"[..], Some(100)));
    assert_eq!(part.suffixes(), [("/v1/doc".to_owned(), 9)]);
    assert!(part.ranges().is_empty());

    let none = Fake::default().with_headers(
        "/v1/doc",
        416,
        &[("Content-Range", "bytes */0")],
        "not sent",
    );
    let got = ask(&none).expect("a slice");
    assert_eq!((got.bytes.is_empty(), got.total), (true, Some(0)));

    let whole = Fake::default().with("/v1/doc", 200, "abcd");
    let got = ask(&whole).expect("a slice");
    assert_eq!((got.bytes.as_slice(), got.total), (&b"abcd"[..], Some(4)));

    let busy = Fake::default().with("/v1/doc", 503, r#"{"error": "busy"}"#);
    assert_eq!(ask(&busy).expect_err("refused").status(), 503);

    let fake = Fake::default();
    let e = block_on(Reader::new(&fake, &PAGE).suffix("/v1/other", 9)).expect_err("refused");
    assert!(e.is_page_bug(), "{e}");
    assert!(fake.seen().is_empty() && fake.suffixes().is_empty());
}
