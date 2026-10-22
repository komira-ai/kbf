//! What the Leases page shows for each answer the read API can give.

use serde_json::json;

use super::super::sched::fixtures::{
    HOSTILE, NEXT_OF_NODE_A, lease, leases_doc, node_a_page, sched_farm,
};
use crate::tests::{Fake, body, render};

fn page(fake: &Fake, path: &str) -> (u16, String) {
    let a = render(fake, path);
    (a.status(), body(&a))
}

/// Catches: a lease column missing or shown under another's header (every number in
/// the fixture is its own: age 41.2 s, in-phase 40.1 s), an age left in raw
/// milliseconds, a phase shown only by colour (its word is in the pill), a node that is
/// not a link to its own leases, the term left out, a header without `scope`.
/// Mutants: `age_ms` and `since_ms` swapped; the phase word dropped from the pill; the
/// node written as plain text.
#[test]
fn every_lease_shows_its_fields_in_a_labelled_table() {
    let fake = sched_farm();
    let (status, html) = page(&fake, "/ui/leases");
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("<title>Leases - kbf</title>"), "{html}");
    assert!(html.contains("<h1>Leases</h1>"), "{html}");
    assert!(html.contains("<code>1792000000123456</code>"), "{html}");
    assert!(html.contains("3 leases on this page."), "{html}");
    for name in [
        "Lease",
        "Node",
        "Phase",
        "Age",
        "In phase",
        "Operation",
        "Instance",
        "Digest",
        "Platform",
        "QoS",
    ] {
        assert!(
            html.contains(&format!("<th scope=\"col\">{name}</th>")),
            "{html}"
        );
    }
    let first = html.split("<tr>").nth(2).expect("the first lease row");
    for cell in [
        "<td><code>1792.0</code></td>",
        "<td><a href=\"/ui/leases?node=node-a\">node-a</a></td>",
        "<td><span class=\"pill ok\">running</span></td>",
        "<td>41.2 s</td><td>40.1 s</td>",
        "<td><code>op-1792.0</code></td>",
        "<td>main</td>",
        "<td>arch=x86_64,os=linux</td>",
        "<td>ci</td>",
    ] {
        assert!(first.contains(cell), "{cell} not in {first}");
    }
    assert!(
        first.contains(&format!("<code>{}/140</code>", "ab".repeat(32))),
        "{first}"
    );
    assert!(
        html.contains("<span class=\"pill pending\">granted</span>"),
        "{html}"
    );
    assert!(
        html.contains("<span class=\"pill pending\">proposed</span>"),
        "{html}"
    );
    assert!(html.contains("<td>2 min 0 s</td>"), "{html}");
    assert!(html.contains("<td>1 min 1 s</td>"), "{html}");
    assert!(!html.contains("Next page"), "{html}");
    assert_eq!(fake.seen(), ["/v1/leases", "/v1/health"]);
}

/// Catches: the filter left out or not filled in with the current node, a node typed
/// into the form never reaching the route, an empty submit read as a node (the API
/// answers it `400`), "All nodes" missing on a filtered page or shown on an unfiltered
/// one, a query key of the page's own handed on to the route (it would be `400`).
/// Mutants: `node` not forwarded; the empty-node check dropped; a stray key forwarded.
#[test]
fn the_node_filter_is_forwarded_and_an_empty_one_is_not() {
    let fake = sched_farm();
    let (status, html) = page(&fake, "/ui/leases?node=node-a&stray=1");
    assert_eq!(status, 200, "{html}");
    assert_eq!(fake.seen(), ["/v1/leases?node=node-a", "/v1/health"]);
    assert!(
        html.contains("2 leases on this page for node node-a."),
        "{html}"
    );
    assert!(
        html.contains("<form method=\"get\" action=\"/ui/leases\""),
        "{html}"
    );
    assert!(html.contains("<label for=\"node\">Node</label>"), "{html}");
    assert!(
        html.contains("name=\"node\" type=\"text\" value=\"node-a\""),
        "{html}"
    );
    assert!(
        html.contains("<a href=\"/ui/leases\">All nodes</a>"),
        "{html}"
    );

    let fake = sched_farm();
    let (_, html) = page(&fake, "/ui/leases?node=");
    assert_eq!(fake.seen(), ["/v1/leases", "/v1/health"]);
    assert!(html.contains("value=\"\""), "{html}");
    assert!(!html.contains("All nodes"), "{html}");
}

/// Catches: a next page that points at the API instead of this page, one that loses the
/// node, the page size or the token (a re-encoded `+` would be a different token), a
/// page that follows a `next_page` that is not a `/v1/leases` link of the API (another
/// host, another route, a protocol-relative URL) or carries a key of its own, and a
/// page that cannot be read again from the link it wrote.
/// Mutants: the link written on the route's path; the token dropped; the path check
/// removed.
#[test]
fn the_next_page_is_the_apis_link_on_this_page() {
    let fake = sched_farm().json(
        "/v1/leases?node=node-a&page_size=2&page_token=1792.1.00ff%2Bgg",
        &leases_doc(),
    );
    let (_, html) = page(&fake, "/ui/leases?node=node-a");
    let next = "/ui/leases?node=node-a&amp;page_size=2&amp;page_token=1792.1.00ff%2Bgg";
    assert!(
        html.contains(&format!(
            "<p class=\"pager\"><a href=\"{next}\">Next page</a></p>"
        )),
        "{html}"
    );
    assert!(NEXT_OF_NODE_A.starts_with("/v1/leases?"));

    // Following the link reads the route with the same three values.
    let (status, _) = page(
        &fake,
        "/ui/leases?node=node-a&page_size=2&page_token=1792.1.00ff%2Bgg",
    );
    assert_eq!(status, 200);
    // The last read is the banner's; the one before it is the leases.
    let seen = fake.seen();
    assert_eq!(
        seen.iter().rev().nth(1).map(String::as_str),
        Some("/v1/leases?node=node-a&page_size=2&page_token=1792.1.00ff%2Bgg")
    );

    // A key of the API's own that the page does not know is not carried over.
    let mut doc = node_a_page();
    doc["next_page"] = json!("/v1/leases?page_size=5&extra=1&page_token=t");
    let fake = Fake::default().json("/v1/leases", &doc);
    let (_, html) = page(&fake, "/ui/leases");
    assert!(
        html.contains("<a href=\"/ui/leases?page_size=5&amp;page_token=t\">Next page</a>"),
        "{html}"
    );

    for foreign in [
        "https://evil.example/v1/leases?page_token=t",
        "//evil.example/v1/leases?page_token=t",
        "/v1/queue?page_token=t",
        "/v1/leases",
        "/v1/leases/../queue?x=1",
        "javascript:alert(1)",
    ] {
        let mut doc = node_a_page();
        doc["next_page"] = json!(foreign);
        let fake = Fake::default().json("/v1/leases", &doc);
        let (status, html) = page(&fake, "/ui/leases");
        assert_eq!(status, 200, "{foreign}");
        assert!(!html.contains("Next page"), "{foreign}: {html}");
        assert!(!html.contains("evil.example"), "{foreign}: {html}");
    }
}

/// Catches: an empty list read as an outage or left blank (the page says what it
/// means, and whether a node filter is why), and the memory note missing.
#[test]
fn an_empty_farm_says_so_and_why_it_may_be_empty() {
    let empty = json!({"term": "5", "next_page": null, "items": []});
    let fake = Fake::default()
        .json("/v1/leases", &empty)
        .json("/v1/leases?node=ghost", &empty);
    let (status, html) = page(&fake, "/ui/leases");
    assert_eq!(status, 200);
    assert!(html.contains("The scheduler holds no lease."), "{html}");
    assert!(html.contains("0 leases on this page."), "{html}");
    assert!(html.contains("empty until the daemons reconnect"), "{html}");
    assert!(!html.contains("<table"), "{html}");
    let (_, html) = page(&fake, "/ui/leases?node=ghost");
    assert!(
        html.contains("That node holds no lease the scheduler knows of."),
        "{html}"
    );
}

/// Catches: an answer the API gave that the page hides or turns into a guess: its
/// status is the page's (a bad token is `400`, with the filter still there to correct
/// it), a `401`'s challenge and a `503`'s retry time are passed on, a route that cannot
/// be reached or answers a document the page cannot read is `502`.
/// Mutants: the status forced to 200; the headers not passed on; the filter dropped
/// from the failed page.
#[test]
fn a_failed_read_keeps_the_apis_status_and_the_filter() {
    let url = "/v1/leases?node=node-a";
    let fake = Fake::default().with(
        url,
        400,
        r#"{"error": "page_token is not one this route made for this node filter"}"#,
    );
    let (status, html) = page(&fake, "/ui/leases?node=node-a");
    assert_eq!(status, 400);
    assert!(
        html.contains("page_token is not one this route made"),
        "{html}"
    );
    assert!(html.contains("value=\"node-a\""), "{html}");

    let fake = Fake::default().with_headers(
        "/v1/leases",
        401,
        &[
            ("www-authenticate", "Bearer realm=\"kbf\""),
            ("set-cookie", "s=1"),
        ],
        r#"{"error": "no"}"#,
    );
    let answer = render(&fake, "/ui/leases");
    assert_eq!(answer.status(), 401);
    assert!(
        answer
            .headers()
            .contains(&("www-authenticate", "Bearer realm=\"kbf\""))
    );
    assert_eq!(answer.headers().len(), 4);

    let fake = Fake::default().with_headers("/v1/leases", 503, &[("retry-after", "9")], "{}");
    assert!(
        render(&fake, "/ui/leases")
            .headers()
            .contains(&("retry-after", "9"))
    );

    assert_eq!(
        render(&Fake::default().down("/v1/leases"), "/ui/leases").status(),
        502
    );
    let fake = Fake::default().json(
        "/v1/leases",
        &json!({"term": "1", "items": [{"lease": "x"}]}),
    );
    let (status, html) = page(&fake, "/ui/leases");
    assert_eq!(status, 502);
    assert!(html.contains("a document this page cannot read"), "{html}");
}

/// Catches (the red test of every page, `docs/design/ui-data.md` 8): any string the
/// API sends, or the visitor types, written as markup: node, lease id, phase,
/// operation, instance, digest, platform, QoS and term, and the node filter in the form
/// and in the URL it reads. (Every field is JSON, so none is non-UTF-8 here.)
/// Mutant: `Html::text` writing raw.
#[test]
fn hostile_strings_are_text() {
    let mut item = lease(HOSTILE, HOSTILE, HOSTILE, 1, 2);
    for key in ["operation", "instance", "digest", "platform", "qos"] {
        item[key] = json!(HOSTILE);
    }
    let doc = json!({"term": HOSTILE, "next_page": null, "items": [item]});
    // The filter as the page writes it into the route's URL.
    let filter = "%22%3E%3Cscript%3Ealert%281%29%3C%2Fscript%3E";
    let fake = Fake::default()
        .json("/v1/leases", &doc)
        .json(&format!("/v1/leases?node={filter}"), &doc);
    for path in ["/ui/leases".to_owned(), format!("/ui/leases?node={filter}")] {
        let (status, html) = page(&fake, &path);
        assert_eq!(status, 200, "{path}: {html}");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("alert(1)</script>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;&quot;&#39;&amp;"),
            "{html}"
        );
        // The node link carries the id percent-encoded, not as markup.
        assert!(
            html.contains("href=\"/ui/leases?node=%3Cscript%3E"),
            "{html}"
        );
    }
    let (_, html) = page(&fake, &format!("/ui/leases?node={filter}"));
    assert!(
        html.contains("value=\"&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;\""),
        "{html}"
    );
}

/// Catches: an age that rounds into the next unit or reads in the wrong one.
/// Mutants: a unit boundary moved by one; seconds shown rounded.
#[test]
fn ages_read_in_their_largest_units_truncated() {
    use super::super::sched::age;
    for (ms, want) in [
        (0, "0 ms"),
        (999, "999 ms"),
        (1_000, "1.0 s"),
        (41_250, "41.2 s"),
        (59_999, "59.9 s"),
        (60_000, "1 min 0 s"),
        (200_000, "3 min 20 s"),
        (3_599_999, "59 min 59 s"),
        (3_600_000, "1 h 0 min"),
        (7_500_000, "2 h 5 min"),
        (86_399_999, "23 h 59 min"),
        (86_400_000, "1 d 0 h"),
        (356_400_000, "4 d 3 h"),
        (u64::MAX, "213503982334 d 14 h"),
    ] {
        assert_eq!(age(ms), want, "{ms}");
    }
}

/// Catches: the page's routes in the registry: `/ui/leases` served, the trailing slash
/// and a sub-path not, and the page on the navigation, marked as the current page.
#[test]
fn the_page_is_routed_and_on_the_navigation() {
    let fake = sched_farm();
    for path in ["/ui/leases/", "/ui/leases/x"] {
        assert_eq!(render(&fake, path).status(), 404, "{path}");
    }
    assert!(fake.seen().is_empty());
    let (_, html) = page(&fake, "/ui/leases");
    assert!(
        html.contains("<a href=\"/ui/leases\" aria-current=\"page\">Leases</a>"),
        "{html}"
    );
    assert!(html.contains("<a href=\"/ui/queue\">Queue</a>"), "{html}");
    let (_, overview) = page(&fake, "/ui");
    for link in ["/ui/leases", "/ui/queue"] {
        assert!(
            overview.contains(&format!("<a href=\"{link}\">")),
            "{overview}"
        );
    }
}
