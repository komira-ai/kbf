//! The failed-action views: the Build page's list of failed actions and the Action page's
//! failure class, command and log tails, all from `/failures`.

use serde_json::{Value, json};

use super::failures::{LogTail, download, stream_words};
use crate::html::escape;
use crate::tests::{
    ARGUMENT, Fake, ID, STDERR, action, action_digest, body, digest, failure, farm, invocation,
    render, row, stderr_blob, stderr_tail,
};

fn page(fake: &Fake, path: &str) -> (u16, String) {
    let a = render(fake, path);
    (a.status(), body(&a))
}

fn failures_url() -> String {
    format!("/v1/invocations/{ID}/failures")
}

fn action_path(digest: &str) -> String {
    format!("/ui/builds/{ID}/actions/{digest}")
}

/// Catches: the failed actions not on the Build page, not before the rest, not linked to
/// their Action page, shown without their outcome in words, their failure class or the
/// size of their stderr; a failure class shown from another field (the fixture gives
/// class and reason each its own text), markup in a target, a reason or an outcome
/// message written as markup; the failures read before the actions, once, at the first
/// page.
#[test]
fn build_lists_its_failed_actions_before_its_actions() {
    let fake = farm();
    let (status, html) = page(&fake, &format!("/ui/builds/{ID}"));
    assert_eq!(status, 200, "{html}");
    assert_eq!(
        fake.seen(),
        [
            format!("/v1/invocations/{ID}"),
            failures_url(),
            format!("/v1/invocations/{ID}/actions"),
        ]
    );
    let (failed, rows) = (
        html.find("<h2>Failed actions</h2>").expect("a section"),
        html.find("<h2>Actions</h2>").expect("a section"),
    );
    assert!(failed < rows, "{html}");
    let section = &html[failed..rows];
    let link = format!(
        "<a href=\"{}\">//app:&lt;b&gt;lib&lt;/b&gt;</a>",
        action_path(&action_digest())
    );
    assert!(section.contains(&link), "{section}");
    assert!(
        section.contains("<span class=\"pill fail\">failed (code 2): &lt;img src=x"),
        "{section}"
    );
    assert!(
        section.contains(
            "<td>infra: &lt;b&gt;worker lost&lt;/b&gt;</td><td>1</td><td>main</td>\
             <td>last 25 B of 4.0 KiB</td>"
        ),
        "{section}"
    );
    assert!(!html.contains("<b>"), "{html}");
    assert!(!html.contains("<img"), "{html}");
    // The next page of failures; the other page's rows are not here.
    assert!(
        section.contains(
            "<a href=\"/ui/builds/build-1?failures_token=f%2B1\">More failed actions</a>"
        ),
        "{section}"
    );
    assert!(!section.contains("retried"), "{section}");
}

/// Catches: the second page of failures asked without its token; its rows' links not
/// naming the page that lists them (so the Action page would search); the Actions pager
/// dropping the failures page, or the failures pager dropping the actions page; a
/// "more" link at the last page.
#[test]
fn the_two_pagers_of_the_build_page_keep_each_others_place() {
    let fake = farm();
    let (status, html) = page(&fake, &format!("/ui/builds/{ID}?failures_token=f%2B1"));
    assert_eq!(status, 200, "{html}");
    assert!(
        fake.seen()
            .contains(&format!("{}?page_token=f%2B1", failures_url())),
        "{:?}",
        fake.seen()
    );
    let retried = digest('7', 20);
    assert!(
        html.contains(&format!(
            "<a href=\"{}?failures_token=f%2B1\">//app:retried</a>",
            action_path(&retried)
        )),
        "{html}"
    );
    assert!(!html.contains("More failed actions"), "{html}");
    assert!(
        html.contains(
            "<a href=\"/ui/builds/build-1?failures_token=f%2B1&amp;page_token=t%2B1\">Next page</a>"
        ),
        "{html}"
    );

    let (_, html) = page(&fake, &format!("/ui/builds/{ID}?page_token=t%2B1"));
    assert!(
        html.contains(
            "<a href=\"/ui/builds/build-1?failures_token=f%2B1&amp;page_token=t%2B1\">\
             More failed actions</a>"
        ),
        "{html}"
    );
}

/// Catches: an empty list worded as success when the build counts failures (a record
/// cut at its bound), a page past the list worded as a clean build, a failed read of the
/// list failing the whole page (it must show the actions and say why the list is
/// missing).
#[test]
fn the_failed_actions_section_says_why_it_is_empty_or_missing() {
    let base = format!("/v1/invocations/{ID}");
    let with = |failed: u64, failures: Option<Value>| {
        let mut inv = invocation(ID);
        inv["counts"]["failed"] = json!(failed);
        let fake = Fake::default().json(&base, &inv).json(
            &format!("{base}/actions"),
            &json!({"actions": [row(&digest('b', 10), "//app:x", &json!({"ok": {"exit_code": 0}}))]}),
        );
        match failures {
            Some(doc) => fake.json(&failures_url(), &doc),
            None => fake.with(
                &failures_url(),
                503,
                r#"{"error": "build history cannot be read right now"}"#,
            ),
        }
    };
    let none = json!({"invocation": invocation(ID), "failures": []});
    let (_, html) = page(&with(0, Some(none.clone())), &format!("/ui/builds/{ID}"));
    assert!(html.contains("No action failed."), "{html}");
    let (_, html) = page(&with(2, Some(none.clone())), &format!("/ui/builds/{ID}"));
    assert!(html.contains("though the build counts failures"), "{html}");
    assert!(!html.contains("No action failed."), "{html}");
    let (_, html) = page(
        &with(2, Some(none)),
        &format!("/ui/builds/{ID}?failures_token=t"),
    );
    // The token's page is not in the fake: a 404 shows as the problem, not as a list.
    assert!(html.contains("answered 404"), "{html}");
    let fake = with(
        2,
        Some(json!({"invocation": invocation(ID), "failures": []})),
    );
    let fake = fake.json(
        &format!("{}?page_token=t", failures_url()),
        &json!({"invocation": invocation(ID), "failures": []}),
    );
    let (_, html) = page(&fake, &format!("/ui/builds/{ID}?failures_token=t"));
    assert!(html.contains("No failed actions on this page."), "{html}");

    let (status, html) = page(&with(2, None), &format!("/ui/builds/{ID}"));
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("build history cannot be read right now"),
        "{html}"
    );
    assert!(
        html.contains(">//app:x</a>"),
        "the actions are still listed: {html}"
    );
}

/// Catches: the Action page of a failed action without its failure class or command
/// (arguments, environment names, working directory, platform, timeout, output paths);
/// any of them written as markup (the fixture's arguments, names, directory and
/// platform are markup); an environment value shown (the API sends names only, and the
/// page says so); the tail not shown, shown unescaped, or without the link that
/// downloads the whole log (the same route without `inline=text`).
#[test]
fn a_failed_action_shows_its_failure_command_and_log_tail() {
    let fake = farm();
    let (status, html) = page(&fake, &action_path(&action_digest()));
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("<dt>Failure class</dt><dd>infra: &lt;b&gt;worker lost&lt;/b&gt;</dd>"),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            "<ol class=\"strings\"><li><code>clang</code></li><li><code>{}</code></li>\
             <li><code>-o</code></li><li><code>out/lib.o</code></li></ol>",
            escape(ARGUMENT)
        )),
        "{html}"
    );
    assert!(
        html.contains(
            "<ul class=\"strings\"><li><code>PATH</code></li>\
             <li><code>&lt;ENV&gt;</code></li></ul>"
        ),
        "{html}"
    );
    assert!(
        html.contains("Environment names (values are never shown)"),
        "{html}"
    );
    for (name, value) in [
        ("Working directory", "/work/&lt;dir&gt;"),
        ("Platform", "&lt;k&gt;=v&amp;, OSFamily=linux"),
        ("Timeout", "90.00 s"),
    ] {
        let pair = format!("<dt>{name}</dt><dd>{value}</dd>");
        assert!(html.contains(&pair), "{pair}: {html}");
    }
    assert!(
        html.contains("<ul class=\"strings\"><li><code>out/lib.o</code></li></ul>"),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            "last 25 B of 4.0 KiB. <a href=\"/v1/blobs/{}?instance=main&amp;name=stderr\">\
             Download the whole log</a>",
            digest('5', 4096)
        )),
        "{html}"
    );
    assert!(
        html.contains(&format!("<pre>{}</pre>", escape(STDERR))),
        "{html}"
    );
    assert!(
        !html.contains("inline=text"),
        "the download is not the text view: {html}"
    );
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("<b>"), "{html}");
    // No blob was read: the tail came with the failure.
    assert!(
        !fake.seen().iter().any(|u| u.starts_with("/v1/blobs")),
        "{:?}",
        fake.seen()
    );
}

/// Catches: a failed action found by searching when its page is named (the page asks
/// for that one page), one not named not found on a later page, a hint at a page that
/// does not list the action shown as a failure it has, a search with no end (the bound
/// is `FAILURES_PAGES_MAX` pages, and the page says the list goes on).
#[test]
fn the_failures_page_of_an_action_is_the_one_named_or_a_bounded_search() {
    let retried = digest('7', 20);
    let base = format!("/v1/invocations/{ID}");
    let doc = |digest: &str| {
        let mut a = row(
            digest,
            "//app:retried",
            &json!({"failed": {"code": 2, "message": "x"}}),
        );
        a["outputs"] = json!([]);
        a["outputs_truncated"] = json!(false);
        a
    };

    // Named: one read of failures, at that page.
    let fake = farm();
    let (status, html) = page(
        &fake,
        &format!("{}?failures_token=f%2B1", action_path(&retried)),
    );
    assert_eq!(status, 200, "{html}");
    assert_eq!(
        fake.seen(),
        [
            format!("{base}/actions/{retried}"),
            format!("{}?page_token=f%2B1", failures_url()),
        ]
    );
    assert!(html.contains("<dt>Failure class</dt>"), "{html}");

    // Not named: the first page, then the next, where it is.
    let fake = farm();
    let (_, html) = page(&fake, &action_path(&retried));
    assert_eq!(
        fake.seen(),
        [
            format!("{base}/actions/{retried}"),
            failures_url(),
            format!("{}?page_token=f%2B1", failures_url()),
        ]
    );
    assert!(html.contains("<dt>Failure class</dt>"), "{html}");

    // Named, but not on that page, which is the last.
    let fake = farm();
    let (status, html) = page(
        &fake,
        &format!("{}?failures_token=f%2B1", action_path(&action_digest())),
    );
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("lists no such action in the 1 page(s) read."),
        "{html}"
    );
    assert!(!html.contains("<dt>Failure class</dt>"), "{html}");
    assert!(!html.contains("the list goes on"), "{html}");

    // A list that never ends: five pages are read, and the page says it goes on.
    let target = digest('9', 3);
    let mut fake = Fake::default().json(&format!("{base}/actions/{target}"), &doc(&target));
    for n in 0..8 {
        let url = if n == 0 {
            failures_url()
        } else {
            format!("{}?page_token=p{n}", failures_url())
        };
        let page = json!({"failures": [], "next_page_token": format!("p{}", n + 1)});
        fake = fake.json(&url, &page);
    }
    let (status, html) = page(&fake, &action_path(&target));
    assert_eq!(status, 200, "{html}");
    assert_eq!(fake.seen().len(), 1 + super::action::FAILURES_PAGES_MAX);
    assert!(
        html.contains("lists no such action in the 5 page(s) read; the list goes on"),
        "{html}"
    );

    // A named page is the only one read, however long the list goes on after it.
    fake.forget_seen();
    let (status, html) = page(
        &fake,
        &format!("{}?failures_token=p1", action_path(&target)),
    );
    assert_eq!(status, 200, "{html}");
    assert_eq!(fake.seen().len(), 2, "{:?}", fake.seen());
    assert!(
        html.contains("lists no such action in the 1 page(s) read; the list goes on"),
        "{html}"
    );
}

/// Catches: a failed action whose failure class, command or tail is not available shown
/// as if it had none: a failure with no class, no command (blob not held or route off),
/// an unread tail (the page falls back to the blob the action links), a command cut at
/// the API's bound, empty lists, no platform and no timeout, each in words; a tail that
/// is the whole log shown as a part.
#[test]
fn what_the_failure_lacks_is_said() {
    let base = format!("/v1/invocations/{ID}");
    let mut f = failure(
        &action_digest(),
        "//app:lib",
        &stderr_tail(&Value::Null, true),
    );
    f["failure"] = Value::Null;
    f["command"] = Value::Null;
    f["command_available"] = json!(false);
    let fake = Fake::default()
        .json(
            &format!("{base}/actions/{}", action_digest()),
            &action(&stderr_blob()),
        )
        .json(&failures_url(), &json!({"failures": [f]}));
    let blob = stderr_blob()["href"].as_str().expect("a link").to_owned();
    let fake = fake.with(&blob, 200, STDERR);
    let (status, html) = page(&fake, &action_path(&action_digest()));
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("none: the last attempt did not end in a failure class"),
        "{html}"
    );
    assert!(
        html.contains("Not available: the Action or Command blob"),
        "{html}"
    );
    // The tail was not read: the blob the action links is.
    assert!(
        html.contains(&format!("<pre>{}</pre>", escape(STDERR))),
        "{html}"
    );
    assert!(!html.contains("Download the whole log"), "{html}");

    // On the Build page the same failure's stderr cell says it was not read; a failure
    // with no class and no stderr says "none" in both cells.
    let mut bare = failure(&digest('7', 20), "//app:bare", &Value::Null);
    bare["failure"] = Value::Null;
    let fake = Fake::default()
        .json(&base, &invocation(ID))
        .json(
            &failures_url(),
            &json!({"failures": [failure(
            &action_digest(), "//app:lib", &stderr_tail(&Value::Null, true)), bare]}),
        )
        .json(&format!("{base}/actions"), &json!({"actions": []}));
    let (_, html) = page(&fake, &format!("/ui/builds/{ID}"));
    assert!(html.contains("<td>not read (4.0 KiB)</td>"), "{html}");
    assert!(
        html.contains("<td>none</td><td>1</td><td>main</td><td>none</td>"),
        "{html}"
    );

    // An empty, cut command with no platform or timeout; the whole log as the tail.
    let mut f = failure(
        &action_digest(),
        "//app:lib",
        &stderr_tail(&json!(STDERR), false),
    );
    f["command"] = json!({"arguments": [], "environment_names": [], "working_directory": "",
        "platform": {}, "timeout_micros": null, "output_paths": [], "truncated": true});
    f["stdout"] = Value::Null;
    // The tail came, but the API gave no link to the log: the tail alone.
    f["stderr"]["href"] = Value::Null;
    let fake = Fake::default()
        .json(
            &format!("{base}/actions/{}", action_digest()),
            &action(&Value::Null),
        )
        .json(&failures_url(), &json!({"failures": [f]}));
    let (_, html) = page(&fake, &action_path(&action_digest()));
    assert!(html.contains("whole log, 4.0 KiB. </p><pre>"), "{html}");
    assert!(!html.contains("Download the whole log"), "{html}");
    assert!(html.contains("<dt>Platform</dt><dd>none</dd>"), "{html}");
    assert!(html.contains("<dt>Timeout</dt><dd>none set</dd>"), "{html}");
    assert!(html.contains("Cut: the API drops strings"), "{html}");
    assert_eq!(
        html.matches("<p class=\"hint\">None.</p>").count(),
        3,
        "{html}"
    );
}

/// Catches: the exit code of an action that ran missing or shown for one that did not
/// run (a refusal has none), a succeeded action read for failures (it is not in the
/// list), a failure list that cannot be read hiding the action's own answer.
#[test]
fn the_exit_code_and_the_failure_read_follow_the_outcome() {
    let base = format!("/v1/invocations/{ID}");
    let path = action_path(&action_digest());
    let with = |outcome: Value| {
        let mut a = action(&Value::Null);
        a["outcome"] = outcome;
        a["outputs"] = json!([]);
        Fake::default().json(&format!("{base}/actions/{}", action_digest()), &a)
    };
    for (outcome, code, reads) in [
        (json!({"ok": {"exit_code": 0}}), Some("0"), 1),
        (json!({"ok": {"exit_code": 7}}), Some("7"), 2),
        (json!({"refused": {"reason": "no"}}), None, 2),
        (json!({"timed_out": {}}), None, 2),
        (json!({"lost": {}}), None, 2),
        (json!(null), None, 1),
    ] {
        let fake = with(outcome.clone());
        let (status, html) = page(&fake, &path);
        assert_eq!(status, 200, "{outcome}: {html}");
        assert_eq!(fake.seen().len(), reads, "{outcome}: {:?}", fake.seen());
        let pair = |c: &str| format!("<dt>Exit code</dt><dd>{c}</dd>");
        match code {
            Some(c) => assert!(html.contains(&pair(c)), "{outcome}: {html}"),
            None => assert!(!html.contains("<dt>Exit code</dt>"), "{outcome}: {html}"),
        }
        // The list is not held by this fake: a failed action says so, a clean one does not.
        assert_eq!(
            html.contains("<h2>Failure</h2>"),
            reads == 2,
            "{outcome}: {html}"
        );
    }
}

/// Catches: the download link keeping the text view, losing the file name or another
/// parameter, or being a link at all for an address that is not root-relative.
#[test]
fn the_download_of_a_log_is_its_route_without_the_text_view() {
    let d = digest('5', 4096);
    let cases = [
        (
            format!("/v1/blobs/{d}?instance=main&name=stderr&inline=text"),
            Some(format!("/v1/blobs/{d}?instance=main&name=stderr")),
        ),
        (
            format!("/v1/blobs/{d}?inline=text&instance=a%26b&name=x%20y"),
            Some(format!("/v1/blobs/{d}?instance=a%26b&name=x+y")),
        ),
        (
            format!("/v1/blobs/{d}?inline=text"),
            Some(format!("/v1/blobs/{d}")),
        ),
        (format!("/v1/blobs/{d}"), Some(format!("/v1/blobs/{d}"))),
        ("https://evil.example/x?inline=text".to_owned(), None),
        ("//evil.example/x".to_owned(), None),
    ];
    for (href, want) in cases {
        let got = download(&href).map(|h| h.as_str().to_owned());
        assert_eq!(got, want, "{href}");
    }
}

/// Catches: a tail worded as the whole log when it is a part, or the reverse, and a
/// size of a log that was not read left out.
#[test]
fn a_log_is_worded_by_how_much_of_it_was_read() {
    let log = |tail: Option<&str>, tail_bytes: Option<u64>, truncated: Option<bool>| LogTail {
        href: None,
        bytes: 2048,
        tail: tail.map(str::to_owned),
        tail_bytes,
        truncated,
    };
    assert_eq!(stream_words(&log(None, None, None)), "not read (2.0 KiB)");
    assert_eq!(
        stream_words(&log(Some("x"), Some(1), Some(true))),
        "last 1 B of 2.0 KiB"
    );
    assert_eq!(
        stream_words(&log(Some("x"), Some(1), Some(false))),
        "whole log, 2.0 KiB"
    );
    assert_eq!(
        stream_words(&log(Some("x"), Some(1), None)),
        "whole log, 2.0 KiB"
    );
}
