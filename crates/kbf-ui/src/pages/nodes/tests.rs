//! What the Nodes and Node pages show for each answer `GET /v1/nodes` and
//! `GET /v1/nodes/{id}` can give. Each test names the defect it catches.

use serde_json::json;

use super::node_page;
use crate::html::escape;
use crate::tests::{
    Fake, HOSTILE, body, fleet_farm, fleet_node, fleet_node_detail, fleet_nodes, render,
};

fn page(fake: &Fake, path: &str) -> (u16, String) {
    let a = render(fake, path);
    (a.status(), body(&a))
}

/// A list holding `nodes` only.
fn list_of(nodes: &[serde_json::Value]) -> Fake {
    Fake::default().json(
        "/v1/nodes",
        &json!({"server": {"version": "v", "commit": "c"}, "nodes": nodes,
            "oldest_protocol": null, "protocol_unknown": 0}),
    )
}

/// Catches: the list reading a route but `/v1/nodes`; a node not linked to its page; a
/// value shown under another's column (the lease count and the booking have their own
/// values in the fixture, as do the placement and the phase); a disconnected node shown
/// as connected, or a quarantine shown without its reason; a status given by colour
/// only (every pill carries its words); a row header missing.
#[test]
fn the_list_shows_each_node_with_its_state_leases_and_booking() {
    let fake = fleet_farm();
    let (status, html) = page(&fake, "/ui/nodes");
    assert_eq!(status, 200, "{html}");
    assert_eq!(fake.seen(), ["/v1/nodes", "/v1/health"]);
    assert!(html.contains("<title>Nodes - kbf</title>"), "{html}");
    assert!(html.contains("aria-current=\"page\">Nodes</a>"), "{html}");
    let row = html
        .split("<tr>")
        .find(|r| r.contains("href=\"/ui/nodes/mac-1\""))
        .expect("the row of mac-1");
    for text in [
        "<th scope=\"row\"><a href=\"/ui/nodes/mac-1\">mac-1</a></th>",
        "<span class=\"pill ok\">connected</span>",
        "<span class=\"pill pending\">draining</span>",
        "<span class=\"pill ok\">serving</span>",
        "macOS 15.1, daemon 0.1.0",
        "<td>7</td>",
        "3500 mCPU, 2.0 GiB, 1 GPU, 4 VM",
        "<td>none</td>",
    ] {
        assert!(row.contains(text), "{text}: {row}");
    }
    let hostile = html
        .split("<tr>")
        .find(|r| r.contains("%3Cscript%3E"))
        .expect("the row of the hostile node");
    for text in [
        "<span class=\"pill fail\">disconnected</span>",
        &format!(
            "<span class=\"pill fail\">quarantined: {}</span>",
            escape(HOSTILE)
        ),
        &format!("<span class=\"pill pending\">{}</span>", escape(HOSTILE)),
        &format!("<li>{}</li>", escape(HOSTILE)),
        "<td>1</td>",
        "500 mCPU",
    ] {
        assert!(hostile.contains(text), "{text}: {hostile}");
    }
    assert!(html.contains("<th scope=\"col\">Booked</th>"), "{html}");
}

/// Catches: markup from a node's strings (its id, software, attention items, phase
/// reason) written as markup on either page (mutant: `Html::text` writing
/// raw), and a node id not encoded as one path segment in its link and in the route read.
#[test]
fn hostile_node_strings_render_as_text() {
    let (_, html) = page(&fleet_farm(), "/ui/nodes");
    for needle in ["<script", "<img", "onerror=alert(2)>"] {
        assert!(!html.contains(needle), "{needle}: {html}");
    }
    assert!(html.contains(&escape(HOSTILE)), "{html}");
    let link = node_page(HOSTILE);
    assert!(html.contains(&format!("<a href=\"{link}\">")), "{html}");

    let id = crate::pages::segment(HOSTILE);
    let mut detail = fleet_node_detail();
    for (k, v) in crate::tests::hostile_node().as_object().expect("an object") {
        detail[k] = v.clone();
    }
    let fake = Fake::default().json(&format!("/v1/nodes/{id}"), &detail);
    let (status, html) = page(&fake, &link);
    assert_eq!(status, 200, "{html}");
    assert_eq!(
        fake.seen(),
        [format!("/v1/nodes/{id}"), "/v1/health".to_owned()]
    );
    for needle in ["<script", "<img", "onerror=alert(2)>"] {
        assert!(!html.contains(needle), "{needle}: {html}");
    }
    assert!(
        html.contains(&format!("<h1>Node <code>{}</code></h1>", escape(HOSTILE))),
        "{html}"
    );
    assert!(
        html.contains(&format!("<li><code>{}</code></li>", escape(HOSTILE))),
        "the lease: {html}"
    );
}

/// Catches: an empty list shown as a table or as an outage; the restart note missing
/// (the list is in memory, so empty is not an outage), on either page.
#[test]
fn an_empty_list_says_so_and_both_pages_say_the_list_is_in_memory() {
    let (status, html) = page(&list_of(&[]), "/ui/nodes");
    assert_eq!(status, 200);
    assert!(
        html.contains("No node has registered with this server."),
        "{html}"
    );
    assert!(!html.contains("<table"), "{html}");
    assert!(
        html.contains("The farm keeps its nodes in memory."),
        "{html}"
    );
    let (_, html) = page(&fleet_farm(), "/ui/nodes/mac-1");
    assert!(
        html.contains("The farm keeps its nodes in memory."),
        "{html}"
    );
}

/// Catches: an absent value shown as a value: software never reported is "not
/// reported".
#[test]
fn absent_software_is_not_guessed() {
    let mut node = fleet_node("old-1", "serving", 0, 0);
    node["software"] = serde_json::Value::Null;
    let (_, html) = page(&list_of(&[node]), "/ui/nodes");
    assert!(html.contains("<td>not reported</td>"), "{html}");
}

/// Catches: the detail reading a route but `/v1/nodes/{id}`; capacity, booked and free
/// swapped or taken from another row (every cell has its own value); the leases dropped;
/// a free the page computed (the fixture's free is not capacity less booked, so a page
/// that computed it shows another number); an unknown protocol shown as a number.
#[test]
fn the_node_shows_capacity_booked_free_and_its_leases_as_the_api_says() {
    let fake = fleet_farm();
    let (status, html) = page(&fake, "/ui/nodes/mac-1");
    assert_eq!(status, 200, "{html}");
    assert_eq!(fake.seen(), ["/v1/nodes/mac-1", "/v1/health"]);
    assert!(html.contains("<title>Node - kbf</title>"), "{html}");
    for text in [
        "<h1>Node <code>mac-1</code></h1>",
        "<a href=\"/ui/nodes\">Nodes</a>",
        "<th scope=\"row\">Capacity</th><td>8000</td><td>16.0 GiB</td><td>2</td><td>6</td>",
        "<th scope=\"row\">Booked</th><td>3500</td><td>2.0 GiB</td><td>1</td><td>4</td>",
        "<th scope=\"row\">Free</th><td>4500</td><td>14.0 GiB</td><td>1</td><td>2</td>",
        "<li><code>117399224320012061.42</code></li>",
        "<dt>Protocol</dt><dd>3</dd>",
        "<dt>Leases held</dt><dd>2</dd>",
        "<dt>Placement</dt><dd><span class=\"pill pending\">draining</span></dd>",
        "<dt>Software</dt><dd>macOS 15.1, daemon 0.1.0</dd>",
    ] {
        assert!(html.contains(text), "{text}: {html}");
    }
    let mut idle = fleet_node_detail();
    idle["protocol"] = serde_json::Value::Null;
    idle["leases"] = json!([]);
    idle["lease_count"] = json!(0);
    let (_, html) = page(
        &Fake::default().json("/v1/nodes/mac-1", &idle),
        "/ui/nodes/mac-1",
    );
    assert!(html.contains("<dt>Protocol</dt><dd>unknown</dd>"), "{html}");
    assert!(html.contains("This node holds no lease."), "{html}");
}

/// Catches: a node the API has no record of shown as `200` or as `502`; the API's
/// `429` without its `Retry-After`, or reloading at once; an id that names no page (empty,
/// not UTF-8, a dot segment, a path below the node) answered with a page or a read.
#[test]
fn an_unknown_node_is_404_and_a_path_that_names_no_node_reads_nothing() {
    let fake = Fake::default().with("/v1/nodes/ghost", 404, r#"{"error": "no_node"}"#);
    let (status, html) = page(&fake, "/ui/nodes/ghost");
    assert_eq!(status, 404, "{html}");
    assert!(html.contains("no_node"), "{html}");

    let busy = Fake::default().with_headers(
        "/v1/nodes/mac-1",
        429,
        &[("retry-after", "7")],
        r#"{"error": "slow_down"}"#,
    );
    let a = render(&busy, "/ui/nodes/mac-1?refresh=5");
    assert_eq!(a.status(), 429);
    assert!(a.headers().contains(&("retry-after", "7")));
    assert!(!body(&a).contains("http-equiv"));

    let fake = fleet_farm();
    for path in [
        "/ui/nodes/",
        "/ui/nodes/%FF",
        "/ui/nodes/.",
        "/ui/nodes/%2e%2E",
        "/ui/nodes/mac-1/leases",
    ] {
        let (status, _) = page(&fake, path);
        assert_eq!(status, 404, "{path}");
    }
    assert!(fake.seen().is_empty(), "{:?}", fake.seen());
}

/// Catches: the list failing quietly when the API is down or answers a document the page
/// cannot read (not `502`), or a node list without the keys the page needs shown as empty.
#[test]
fn a_list_that_does_not_read_is_502() {
    let (status, _) = page(&Fake::default().down("/v1/nodes"), "/ui/nodes");
    assert_eq!(status, 502);
    let (status, html) = page(
        &Fake::default().json("/v1/nodes", &json!({"nodes": [{"node_id": "x"}]})),
        "/ui/nodes",
    );
    assert_eq!(status, 502, "{html}");
    let (status, _) = page(
        &Fake::default().json("/v1/nodes", &fleet_nodes()),
        "/ui/nodes",
    );
    assert_eq!(status, 200);
}
