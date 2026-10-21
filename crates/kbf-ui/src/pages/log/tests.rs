//! What the Log page shows for each answer the log route can give, and that it reads
//! the least it can: one ranged request, never the whole log.

use serde_json::json;

use super::{LOG_SHOWN_BYTES, printable, without_split_char, without_split_start};
use crate::tests::{Fake, ID, RUN_ID, action, action_digest, body, farm, render, stderr_blob};

/// The cap, as a length.
const CAP: usize = LOG_SHOWN_BYTES as usize;

/// The route of the log the fixtures answer.
fn route(stream: &str) -> String {
    format!("/v1/runs/{RUN_ID}/logs/{stream}")
}

/// A read API whose `stderr` of `RUN_ID` answers `206` with `bytes`, the first of a log
/// of `total` (what a `bytes=0-N` range gets).
fn partial(bytes: impl Into<Vec<u8>>, total: usize) -> Fake {
    let bytes = bytes.into();
    let range = format!("bytes 0-{}/{total}", bytes.len().saturating_sub(1));
    Fake::default().with_headers(&route("stderr"), 206, &[("Content-Range", &range)], bytes)
}

/// A read API whose `stderr` of `RUN_ID` answers `206` with `bytes`, the last of a log of
/// `total` (what a `bytes=-N` range gets).
fn tail(bytes: impl Into<Vec<u8>>, total: usize) -> Fake {
    let bytes = bytes.into();
    let range = format!(
        "bytes {}-{}/{total}",
        total - bytes.len(),
        total.saturating_sub(1)
    );
    Fake::default().with_headers(&route("stderr"), 206, &[("Content-Range", &range)], bytes)
}

fn page(fake: &Fake, stream: &str) -> (u16, String) {
    page_at(fake, stream, "")
}

/// The page of `stream` with `query` (with its `?`, or empty).
fn page_at(fake: &Fake, stream: &str, query: &str) -> (u16, String) {
    let a = render(fake, &format!("/ui/runs/{RUN_ID}/logs/{stream}{query}"));
    (a.status(), body(&a))
}

/// The link that shows the other end of the log.
fn other_end(query: &str, words: &str) -> String {
    format!("<a href=\"/ui/runs/{RUN_ID}/logs/stderr{query}\">{words}</a>")
}

/// What the `<pre>` of a page holds, as written.
fn pre(html: &str) -> &str {
    let from = html.find("<pre>").expect("a pre") + "<pre>".len();
    let to = html.find("</pre>").expect("a closed pre");
    &html[from..to]
}

/// Catches (hostile fixture): a log that runs on the page or draws it. Markup in a log
/// shown as markup; a control character, an escape sequence, a bidirectional override or
/// a C1 control written raw; bytes that are not UTF-8 dropped, or the page refused. A log
/// is also read with exactly one `Range`: the last MiB by default, the first with
/// `?from=start`. Mutants: `printable` returning its text unchanged; the C0 arm dropped;
/// the bidi arm dropped; the DEL arm dropped; the range not sent (the `suffix` call
/// replaced by a plain read); the default the head.
#[test]
fn a_hostile_log_is_shown_as_inert_text_and_read_with_one_range() {
    let log =
        b"<script>alert(1)</script>\x1b[31mred\x00\x07\x7f \xe2\x80\xaeevil\xc2\x85 end\xff\xfe\n";
    let fake = tail(log.to_vec(), log.len());
    let (status, html) = page(&fake, "stderr");
    assert_eq!(status, 200, "{html}");
    assert_eq!(
        fake.suffixes(),
        [(route("stderr"), LOG_SHOWN_BYTES)],
        "one suffix read of the last MiB"
    );
    assert!(fake.ranges().is_empty(), "no span read");
    assert_eq!(fake.seen(), [route("stderr")], "and nothing else");
    let head = partial(log.to_vec(), log.len());
    let (status, _) = page_at(&head, "stderr", "?from=start");
    assert_eq!(status, 200);
    assert_eq!(
        head.ranges(),
        [(route("stderr"), 0, LOG_SHOWN_BYTES - 1)],
        "one ranged read of the first MiB"
    );
    assert!(head.suffixes().is_empty(), "no suffix read");
    assert!(!html.contains("<script"), "{html}");
    let shown = pre(&html);
    assert!(
        shown.starts_with("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{shown}"
    );
    for raw in ['\u{1b}', '\0', '\u{7}', '\u{7f}', '\u{202e}', '\u{85}'] {
        assert!(!html.contains(raw), "{raw:?} is written raw");
    }
    assert!(
        shown.contains("\u{241b}[31mred\u{2400}\u{2407}\u{2421} "),
        "{shown}"
    );
    assert!(shown.contains("\u{fffd}evil\u{fffd} end"), "{shown}");
    assert!(
        shown.ends_with("end\u{fffd}\u{fffd}\n"),
        "each invalid byte is one replacement: {shown}"
    );
    assert!(
        html.contains("Unredacted"),
        "the page says it hides nothing"
    );
}

/// Catches: a cut log shown as the whole one, a whole log shown as cut, no way to the
/// rest, or a download link that is not the route. The cut is `total > shown`: a log of
/// exactly the cap is whole. The notice names the end that is shown (the last MiB by
/// default, the first with `?from=start`) and links the other end, which is the page
/// itself or its `?from=start`; a whole log links neither. Mutants: `more` always false;
/// always true; `>` written `>=`; the link dropped; the words for the ends swapped; the
/// other end's link pointing at the page's own end.
#[test]
fn a_log_longer_than_the_range_says_which_end_it_shows_and_links_the_other() {
    let link = format!("<a href=\"{}\">Download the whole log</a>", route("stderr"));
    let fake = tail(vec![b'a'; CAP], 3_000_000);
    let (status, html) = page(&fake, "stderr");
    assert_eq!(status, 200);
    assert!(
        html.contains("Cut: only the last 1.0 MiB of the log is shown. "),
        "{}",
        &html[..html.len().min(3000)]
    );
    assert!(html.contains(&other_end("?from=start", "Show the start of the log")));
    assert!(!html.contains("Show the end of the log"), "{html}");
    assert!(html.contains("<dd>2.9 MiB</dd>"), "the size is the whole's");
    assert!(html.contains(&link));
    assert_eq!(pre(&html).len(), CAP);

    let fake = partial(vec![b'a'; CAP], 3_000_000);
    let (status, html) = page_at(&fake, "stderr", "?from=start");
    assert_eq!(status, 200);
    assert!(
        html.contains("Cut: only the first 1.0 MiB of the log is shown. "),
        "{}",
        &html[..html.len().min(3000)]
    );
    assert!(html.contains(&other_end("", "Show the end of the log")));
    assert!(!html.contains("Show the start of the log"), "{html}");
    assert!(html.contains(&link));

    for (fake, query) in [
        (tail(vec![b'a'; CAP], CAP), ""),
        (partial(vec![b'a'; CAP], CAP), "?from=start"),
    ] {
        let (_, html) = page_at(&fake, "stderr", query);
        assert!(!html.contains("Cut:"), "a log of exactly the cap is whole");
        assert!(!html.contains("Show the "), "{html}");
        assert!(html.contains(&link));
    }

    let small = tail("short\n", 6);
    let (_, html) = page(&small, "stderr");
    assert!(!html.contains("Cut:"));
    assert_eq!(pre(&html), "short\n");
    assert!(html.contains("<dd>6 B</dd>"), "{html}");
}

/// Catches: the end of the log taken from anything but `from=start` exactly: `from=end`,
/// an unknown value, an empty one and a hostile one are the default tail, the first of
/// two values counts, and a value is never written to the page. Mutants: the match
/// arm for `start` always taken; the default the head; the value echoed.
#[test]
fn only_from_start_asks_for_the_head() {
    let hostile = "?from=%3Cscript%3Ealert(1)%3C%2Fscript%3E";
    for query in [
        "",
        "?from=end",
        "?from=",
        "?from=START",
        "?other=start",
        "?from=end&from=start",
        hostile,
    ] {
        let fake = tail("x", 1);
        let (status, html) = page_at(&fake, "stderr", query);
        assert_eq!(status, 200, "{query}");
        assert_eq!(fake.suffixes().len(), 1, "{query} is the tail");
        assert!(fake.ranges().is_empty(), "{query}");
        assert!(!html.contains("<script>alert"), "{query}: {html}");
    }
    for query in ["?from=start", "?from=start&from=end", "?x=1&from=start"] {
        let fake = partial("x", 1);
        let (status, _) = page_at(&fake, "stderr", query);
        assert_eq!(status, 200, "{query}");
        assert_eq!(fake.ranges().len(), 1, "{query} is the head");
        assert!(fake.suffixes().is_empty(), "{query}");
    }
}

/// Catches: a cut inside a code point shown as a replacement character the log does not
/// hold, or a real invalid byte dropped with it: at the END of a head window and at the
/// START of a tail window (the euro sign, three bytes, cut after one and after two of
/// them). Mutants: the cut not trimmed; the trim applied to a log that is whole; the tail
/// trimmed at its end, or the head at its start.
#[test]
fn a_cut_inside_a_code_point_drops_the_part_and_a_whole_log_does_not() {
    let mut cut = vec![b'a'; CAP - 2];
    cut.extend_from_slice(&[0xe2, 0x82]);
    let (_, html) = page_at(&partial(cut, CAP + 100), "stderr", "?from=start");
    let shown = pre(&html);
    assert!(!shown.contains('\u{fffd}'), "{}", shown.len());
    assert_eq!(shown.len(), CAP - 2);

    // A tail window that starts with the last one or two bytes of a code point.
    for split in [&[0x82, 0xac][..], &[0xac][..]] {
        let mut cut = split.to_vec();
        cut.extend_from_slice(&vec![b'b'; CAP - split.len()]);
        let (_, html) = page(&tail(cut, CAP + 100), "stderr");
        let shown = pre(&html);
        assert!(!shown.contains('\u{fffd}'), "{split:x?}: {}", shown.len());
        assert_eq!(shown.len(), CAP - split.len(), "{split:x?}");
    }
    // A tail window that ends with an incomplete sequence is the log's own: shown.
    let mut end = vec![b'b'; CAP - 1];
    end.push(0xe2);
    let (_, html) = page(&tail(end, CAP + 100), "stderr");
    assert!(pre(&html).ends_with('\u{fffd}'), "the log's own bad byte");
    // A head window that starts with a continuation byte is the log's own: shown.
    let (_, html) = page_at(
        &partial(b"\xac b".to_vec(), CAP + 100),
        "stderr",
        "?from=start",
    );
    assert!(pre(&html).starts_with('\u{fffd}'), "the log's own bad byte");

    let (_, html) = page_at(&partial(b"ab\xe2\x82".to_vec(), 4), "stderr", "?from=start");
    assert_eq!(pre(&html), "ab\u{fffd}", "a whole log's bad end is shown");
    let (_, html) = page(&tail(b"\x82\xacab".to_vec(), 4), "stderr");
    assert_eq!(
        pre(&html),
        "\u{fffd}\u{fffd}ab",
        "a whole log's bad start is shown"
    );
}

/// Catches: a source that ignored the range (a `200` with the whole log) shown whole: the
/// page reads no more than it asked for even then, and keeps the end that was asked for:
/// the last MiB by default (the 10 bytes before it are not shown), the first with
/// `?from=start`. Mutants: the body not truncated; a tail cut from its front; a head cut
/// from its back.
#[test]
fn a_range_the_source_ignored_is_cut_to_the_cap_from_the_end_asked_for() {
    let mut whole = vec![b'x'; 10];
    whole.extend_from_slice(&vec![b'a'; CAP]);
    let fake = Fake::default().with(&route("stderr"), 200, whole.clone());
    let (status, html) = page(&fake, "stderr");
    assert_eq!(status, 200);
    assert_eq!(pre(&html).len(), CAP);
    assert!(!pre(&html).contains('x'), "the tail is the end of the log");
    assert!(html.contains("Cut: only the last 1.0 MiB"), "cut says so");

    let (status, html) = page_at(&fake, "stderr", "?from=start");
    assert_eq!(status, 200);
    assert_eq!(pre(&html).len(), CAP);
    assert!(
        pre(&html).starts_with("xxxxxxxxxxaa"),
        "the head is the start"
    );
    assert!(html.contains("Cut: only the first 1.0 MiB"), "cut says so");
}

/// Catches: an empty log shown as an empty box or as an error, and a size nobody said
/// shown as zero. A `416` (a start past the end of an empty log), a `200` with no body, and
/// a `206` with no `Content-Range`: the last has no size and is cut only when it filled the
/// range. Mutants: the `416` body kept; `unknown` shown as `0 B`; `>=` written `>`.
#[test]
fn an_empty_log_and_an_unknown_size_are_said_plainly() {
    let empty = Fake::default().with_headers(
        &route("stderr"),
        416,
        &[("Content-Range", "bytes */0")],
        "garbage a server would not send",
    );
    for fake in [empty, Fake::default().with(&route("stderr"), 200, "")] {
        let (status, html) = page(&fake, "stderr");
        assert_eq!(status, 200);
        assert!(html.contains("The log is empty."), "{html}");
        assert!(!html.contains("<pre>"), "{html}");
        assert!(html.contains("<dd>0 B</dd>"), "{html}");
    }

    let blind = Fake::default().with(&route("stderr"), 206, "abc");
    let (_, html) = page(&blind, "stderr");
    assert!(html.contains("<dd>unknown</dd>"), "{html}");
    assert!(!html.contains("Cut:"), "{html}");
    let blind = Fake::default().with(&route("stderr"), 206, vec![b'a'; CAP]);
    let (_, html) = page(&blind, "stderr");
    assert!(html.contains("Cut:"), "a full range may have more");
}

/// Catches: the API's refusals shown as a log, or as another status: no copy kept of the
/// log (`404`, `no_log`), a busy read budget (`503` with its `Retry-After`), and a farm
/// with no history (an unknown route: the page is `200` and names the flag).
#[test]
fn the_apis_refusals_are_the_page() {
    let none = Fake::default().with(
        &route("stderr"),
        404,
        format!(r#"{{"error": "no_log", "id": "{RUN_ID}", "log": "stderr"}}"#),
    );
    let (status, html) = page(&none, "stderr");
    assert_eq!(status, 404, "a missing log stays a 404");
    assert!(html.contains("no_log"), "{html}");
    assert!(!html.contains("History is off"), "{html}");

    let busy = Fake::default().with_headers(
        &route("stdout"),
        503,
        &[("Retry-After", "1")],
        r#"{"error": "busy", "busy": true}"#,
    );
    let a = render(&busy, &format!("/ui/runs/{RUN_ID}/logs/stdout"));
    assert_eq!(a.status(), 503);
    assert!(a.headers().contains(&("retry-after", "1")));

    let off = Fake::default().with(&route("result"), 404, "");
    let (status, html) = page(&off, "result");
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("History is off on this farm"), "{html}");
    assert!(html.contains("--history-objects"), "{html}");
}

/// Catches: a path that is not a log of a run read as one, so the page asks the API for
/// something else: another stream, no run id, a longer path.
#[test]
fn only_the_logs_of_a_run_are_pages() {
    for path in [
        format!("/ui/runs/{RUN_ID}/logs/other"),
        format!("/ui/runs/{RUN_ID}/logs/"),
        format!("/ui/runs/{RUN_ID}/logs"),
        format!("/ui/runs/{RUN_ID}/logs/stderr/more"),
        "/ui/runs//logs/stderr".to_owned(),
        format!("/ui/runs/{RUN_ID}"),
        "/ui/runs".to_owned(),
    ] {
        let fake = Fake::default();
        let a = render(&fake, &path);
        assert_eq!(a.status(), 404, "{path}");
        assert!(fake.seen().is_empty(), "{path}: {:?}", fake.seen());
    }
}

/// Catches: a run id written raw into the page or into the URL read: it is escaped in the
/// page and one encoded path segment in the route. Mutants: `segment` dropped from the
/// route; the heading written without escaping.
#[test]
fn a_hostile_run_id_is_escaped_and_encoded() {
    let fake = Fake::default().with_headers(
        "/v1/runs/%3Cb%3E%2Fx/logs/stdout",
        206,
        &[("Content-Range", "bytes 0-1/2")],
        "ok",
    );
    let a = render(&fake, "/ui/runs/%3Cb%3E%2Fx/logs/stdout");
    let html = body(&a);
    assert_eq!(a.status(), 200, "{html}");
    assert!(html.contains("&lt;b&gt;/x"), "{html}");
    assert!(!html.contains("<b>"), "{html}");
    assert_eq!(fake.seen(), ["/v1/runs/%3Cb%3E%2Fx/logs/stdout"]);
}

/// Catches: the Action page with no way to a run's logs: a link for each of the three logs
/// of each attempt that has a run id; none shown for a farm that sends no run id. Mutants:
/// the section dropped; the links built for a stream that is not one.
#[test]
fn the_action_page_links_the_logs_of_each_attempt_that_has_a_run() {
    let url = format!("/v1/invocations/{ID}/actions/{}", action_digest());
    let mut with_run = action(&stderr_blob());
    with_run["attempts"][0]["run_id"] = json!(RUN_ID);
    let fake = farm().json(&url, &with_run);
    let a = render(
        &fake,
        &format!("/ui/builds/{ID}/actions/{}", action_digest()),
    );
    let html = body(&a);
    assert_eq!(a.status(), 200);
    assert!(html.contains("<h2>Run logs</h2>"), "{html}");
    for stream in ["stdout", "stderr", "result"] {
        let link = format!("<a href=\"/ui/runs/{RUN_ID}/logs/{stream}\">{stream}</a>");
        assert!(html.contains(&link), "{link}");
    }
    assert!(html.contains(&format!("run {RUN_ID}")), "{html}");

    let a = render(
        &farm(),
        &format!("/ui/builds/{ID}/actions/{}", action_digest()),
    );
    let html = body(&a);
    assert!(!html.contains("Run logs"), "{html}");
    assert!(!html.contains("/ui/runs/"), "{html}");
}

/// Catches: a character mapped wrongly or at the wrong edge: each C0 control but newline,
/// tab and carriage return to its picture, DEL to its picture, C1 controls and the
/// bidirectional controls to the replacement character, and the characters just outside
/// those ranges left alone. Mutants: any range edge moved by one.
#[test]
fn printable_maps_exactly_the_characters_something_acts_on() {
    let kept = [
        '\n',
        '\t',
        '\r',
        ' ',
        'a',
        '~',
        '\u{a0}',
        '\u{2010}',
        '\u{2029}',
        '\u{202f}',
        '\u{2065}',
        '\u{206a}',
        '\u{20ac}',
        '\u{1f600}',
    ];
    for c in kept {
        assert_eq!(printable(&c.to_string()), c.to_string(), "{c:?}");
    }
    for code in (0u32..0x20).filter(|c| ![9, 10, 13].contains(c)) {
        let c = char::from_u32(code).expect("a char");
        let want = char::from_u32(0x2400 + code).expect("a picture");
        assert_eq!(printable(&c.to_string()), want.to_string(), "{code:#x}");
    }
    assert_eq!(printable("\u{7f}"), "\u{2421}");
    let replaced = (0x80u32..=0x9f)
        .chain([0x200e, 0x200f])
        .chain(0x202a..=0x202e)
        .chain(0x2066..=0x2069);
    for code in replaced {
        let c = char::from_u32(code).expect("a char");
        assert_eq!(printable(&c.to_string()), "\u{fffd}", "{code:#x}");
    }
}

/// Catches: an incomplete sequence at the end kept, a complete or invalid one trimmed. Only
/// a trailing prefix of a code point (one to three bytes) is dropped, and only once.
#[test]
fn only_a_trailing_incomplete_sequence_is_dropped() {
    let cases: [(&[u8], &[u8]); 9] = [
        (b"", b""),
        (b"abc", b"abc"),
        (b"a\xe2", b"a"),
        (b"a\xe2\x82", b"a"),
        (b"a\xe2\x82\xac", b"a\xe2\x82\xac"),
        (b"a\xff", b"a\xff"),
        (b"\xf0\x9f\x98", b""),
        (b"\xe2\x82\xe2", b"\xe2\x82"),
        (b"\xe2\x82\xac\xe2\x82", b"\xe2\x82\xac"),
    ];
    for (given, want) in cases {
        assert_eq!(without_split_char(given), want, "{given:?}");
    }
}

/// Catches: a leading continuation byte kept (a replacement character the log does not
/// hold), a lead byte or an ASCII byte dropped, or more than the three bytes that can
/// belong to one code point dropped. Mutants: `take(3)` written `take(4)` or `take(2)`;
/// the range edges moved by one; the run not stopped at the first other byte.
#[test]
fn only_a_leading_run_of_up_to_three_continuation_bytes_is_dropped() {
    let cases: [(&[u8], &[u8]); 11] = [
        (b"", b""),
        (b"abc", b"abc"),
        (b"\xacabc", b"abc"),
        (b"\x82\xacabc", b"abc"),
        (b"\x80\x80\xbfabc", b"abc"),
        (b"\x80\x80\x80\x80abc", b"\x80abc"),
        (b"\x7fabc", b"\x7fabc"),
        (b"\xc0abc", b"\xc0abc"),
        (b"\xe2\x82\xac", b"\xe2\x82\xac"),
        (b"\x82a\xac", b"a\xac"),
        (b"\xac\xac", b""),
    ];
    for (given, want) in cases {
        assert_eq!(without_split_start(given), want, "{given:?}");
    }
}
