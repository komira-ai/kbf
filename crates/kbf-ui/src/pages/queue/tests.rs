//! What the Queue page shows for each answer the read API can give.

use serde_json::{Value, json};

use super::super::sched::fixtures::{HOSTILE, queue_doc, sched_farm};
use crate::tests::{Fake, body, render};

fn page(fake: &Fake, path: &str) -> (u16, String) {
    let a = render(fake, path);
    (a.status(), body(&a))
}

fn with(doc: &Value) -> Fake {
    Fake::default().json("/v1/queue", doc)
}

/// Catches: a part of the queue left off the page or shown under another's header: the
/// depth and the oldest age in words, one row per platform and QoS (each number its own),
/// the reason with its count and example, the avoided operation with each node as a link
/// to that node's leases. Mutants: depth and oldest swapped; the QoS column dropped; the
/// reason or the count not written; the nodes written as plain text.
#[test]
fn the_page_shows_depth_groups_reasons_and_avoided_operations() {
    let fake = sched_farm();
    let (status, html) = page(&fake, "/ui/queue");
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("<title>Queue - kbf</title>"), "{html}");
    assert!(
        html.contains("<dt>Operations queued</dt><dd>5</dd>"),
        "{html}"
    );
    assert!(
        html.contains("<dt>Oldest waiting</dt><dd>1 min 1 s</dd>"),
        "{html}"
    );
    for heading in [
        "By platform and QoS",
        "Waiting because no worker can run them",
        "Kept off some nodes",
    ] {
        assert!(html.contains(&format!("<h2>{heading}</h2>")), "{html}");
    }
    for name in [
        "Platform",
        "QoS",
        "Queued",
        "Oldest",
        "Reason",
        "Operations",
        "Example",
        "Digest",
        "Operation",
        "Instance",
        "Avoids nodes",
    ] {
        assert!(
            html.contains(&format!("<th scope=\"col\">{name}</th>")),
            "{html}"
        );
    }
    assert!(
        html.contains("<td>os=macos</td><td>ci</td><td>4</td><td>1 min 1 s</td>"),
        "{html}"
    );
    assert!(
        html.contains("<td>any</td><td>interactive</td><td>1</td><td>900 ms</td>"),
        "{html}"
    );
    assert!(
        html.contains("node-a lacks os=macos</td><td>4</td>"),
        "{html}"
    );
    assert!(
        html.contains("<code>op-3</code> on instance main"),
        "{html}"
    );
    assert!(
        html.contains(&format!("<code>{}/77</code>", "cd".repeat(32))),
        "{html}"
    );
    assert!(html.contains("<code>op-9</code>"), "{html}");
    assert!(
        html.contains(
            "<a href=\"/ui/leases?node=node-b\">node-b</a>, \
             <a href=\"/ui/leases?node=node-c\">node-c</a>"
        ),
        "{html}"
    );
    assert!(!html.contains("Showing"), "{html}");
    assert!(html.contains("empty until the daemons reconnect"), "{html}");
    assert_eq!(fake.seen(), ["/v1/queue", "/v1/health"]);
}

/// Catches: an empty queue shown as blank tables or as a guess: the page says nothing is
/// queued, nothing waits (the API's null age), and that no operation is blocked or kept
/// off a node. Mutant: a null `oldest_age_ms` shown as `0 ms`.
#[test]
fn an_empty_queue_says_so() {
    let doc = json!({"depth": 0, "oldest_age_ms": null, "by_platform": [], "waiting": [],
        "avoided": [], "avoided_total": 0});
    let (status, html) = page(&with(&doc), "/ui/queue");
    assert_eq!(status, 200);
    assert!(
        html.contains("<dt>Operations queued</dt><dd>0</dd>"),
        "{html}"
    );
    assert!(
        html.contains("<dt>Oldest waiting</dt><dd>nothing waits</dd>"),
        "{html}"
    );
    assert!(html.contains("Nothing is queued."), "{html}");
    assert!(
        html.contains("No queued operation lacks a worker that can run it"),
        "{html}"
    );
    assert!(
        html.contains("No queued operation is kept off a node."),
        "{html}"
    );
    assert!(!html.contains("<table"), "{html}");
}

/// Catches: the avoided list cut at the API's bound without saying so (it lists at most
/// 100 of `avoided_total`), and the note shown when nothing is cut.
/// Mutant: the comparison reversed.
#[test]
fn a_cut_avoided_list_says_how_many_there_are() {
    let mut doc = queue_doc();
    doc["avoided_total"] = json!(250);
    let (_, html) = page(&with(&doc), "/ui/queue");
    assert!(html.contains("Showing 1 of 250"), "{html}");
    doc["avoided_total"] = json!(1);
    let (_, html) = page(&with(&doc), "/ui/queue");
    assert!(!html.contains("Showing"), "{html}");
}

/// Catches: a node id that is not encoded in the link to its leases (a space, `&` and
/// `#` would split or end the query), and a link that leaves this server.
#[test]
fn an_avoided_node_is_encoded_in_its_link() {
    let mut doc = queue_doc();
    doc["avoided"][0]["nodes"] = json!(["a b&c#d", "//evil.example"]);
    let (_, html) = page(&with(&doc), "/ui/queue");
    assert!(
        html.contains("<a href=\"/ui/leases?node=a+b%26c%23d\">a b&amp;c#d</a>"),
        "{html}"
    );
    assert!(
        html.contains("<a href=\"/ui/leases?node=%2F%2Fevil.example\">//evil.example</a>"),
        "{html}"
    );
}

/// Catches (the red test of every page): any string the API sends written as markup: the
/// platform, QoS, reason, operation, instance, digest and node ids.
/// Mutant: `Html::text` writing raw.
#[test]
fn hostile_strings_are_text() {
    let doc = json!({"depth": 1, "oldest_age_ms": 1,
        "by_platform": [{"platform": HOSTILE, "qos": HOSTILE, "depth": 1, "oldest_age_ms": 1}],
        "waiting": [{"reason": HOSTILE, "count": 1, "example_operation": HOSTILE,
            "example_instance": HOSTILE, "example_digest": HOSTILE}],
        "avoided": [{"operation": HOSTILE, "instance": HOSTILE, "digest": HOSTILE,
            "nodes": [HOSTILE]}],
        "avoided_total": 1});
    let (status, html) = page(&with(&doc), "/ui/queue");
    assert_eq!(status, 200, "{html}");
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("alert(1)</script>"), "{html}");
    let escaped = "&lt;script&gt;alert(1)&lt;/script&gt;&quot;&#39;&amp;";
    assert!(html.matches(escaped).count() >= 10, "{html}");
}

/// Catches: the API's status hidden or guessed at: a `503` with its retry time, a route
/// that cannot be reached or answers a document the page cannot read is `502`, and the
/// page's own query is not handed to a route that takes none (it answers `400`).
/// Mutants: the status forced to 200; the headers not passed on; the query forwarded.
#[test]
fn a_failed_read_keeps_the_apis_status() {
    let fake = Fake::default().with_headers("/v1/queue", 503, &[("retry-after", "9")], "{}");
    let answer = render(&fake, "/ui/queue");
    assert_eq!(answer.status(), 503);
    assert!(answer.headers().contains(&("retry-after", "9")));
    assert_eq!(
        render(&Fake::default().down("/v1/queue"), "/ui/queue").status(),
        502
    );
    let (status, html) = page(&with(&json!({"depth": 1})), "/ui/queue");
    assert_eq!(status, 502);
    assert!(html.contains("a document this page cannot read"), "{html}");

    let fake = sched_farm();
    let (status, _) = page(&fake, "/ui/queue?node=x&page_size=1");
    assert_eq!(status, 200);
    assert_eq!(fake.seen(), ["/v1/queue", "/v1/health"]);
}

/// Catches: the page's routes in the registry: `/ui/queue` served with the page marked as
/// the current one on the navigation, a sub-path and a trailing slash not.
#[test]
fn the_page_is_routed_and_on_the_navigation() {
    let fake = sched_farm();
    for path in ["/ui/queue/", "/ui/queue/x"] {
        assert_eq!(render(&fake, path).status(), 404, "{path}");
    }
    assert!(fake.seen().is_empty());
    let (_, html) = page(&fake, "/ui/queue");
    assert!(
        html.contains("<a href=\"/ui/queue\" aria-current=\"page\">Queue</a>"),
        "{html}"
    );
    assert!(html.contains("<a href=\"/ui/leases\">Leases</a>"), "{html}");
}
