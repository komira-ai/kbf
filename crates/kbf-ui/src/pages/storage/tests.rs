//! What the Storage page shows for each answer `GET /v1/storage` can give.

use serde_json::{Value, json};

use super::age;
use crate::tests::{
    Fake, body, fleet_farm, render, storage_doc, storage_farm, storage_node, storage_node_unknown,
};

fn page_of(doc: &Value) -> (u16, String) {
    let fake = Fake::default().json("/v1/storage", doc);
    let a = render(&fake, "/ui/storage");
    (a.status(), body(&a))
}

/// The `<tr>` of the first table row whose header cell is member `id`, in the table
/// under the heading `heading`.
fn row(html: &str, heading: &str, id: u64) -> String {
    let from = html.find(&format!("<h2>{heading}</h2>")).expect("heading");
    let table = &html[from..];
    let needle = format!("<tr><th scope=\"row\">{id}</th>");
    let start = table.find(&needle).expect("a row for the member");
    let end = table[start..].find("</tr>").expect("row end") + start;
    table[start..end].to_owned()
}

/// Catches: a page missing a part. The replication answer (the number, the debts, the
/// reasons), the member table (state, report freshness, debts, open writes, generation)
/// and every one of the seven heartbeat values of the write-tier table, each in its
/// unit. Mutants: the reasons list dropped; the used column written from the budget;
/// the flush counters swapped.
#[test]
fn the_page_shows_the_replication_the_members_and_the_write_tier() {
    let (status, html) = page_of(&storage_doc());
    assert_eq!(status, 200, "{html}");
    for text in [
        "<title>Storage - kbf</title>",
        "<h1>Storage</h1>",
        "<dt>Active members</dt><dd>2 of 2</dd>",
        "<dt>Copies owed by reporting active members</dt><dd>3</dd>",
        "<dt>Silent (stale report)</dt><dd>2</dd>",
        "<li>member 2 silent</li>",
        "<li>3 blobs owed</li>",
        "aria-current=\"page\">Storage</a>",
        "<code><a href=\"/v1/storage\">/v1/storage</a></code>",
    ] {
        assert!(html.contains(text), "{text}: {html}");
    }
    let members = row(&html, "Members", 1);
    for text in [
        "active</span>",
        "fresh</span>",
        "<td>3</td><td>0</td><td>12</td>",
    ] {
        assert!(members.contains(text), "{text}: {members}");
    }
    assert_eq!(
        row(&html, "Write tier", 1),
        "<tr><th scope=\"row\">1</th><td>1.0 MiB</td><td>2</td><td>4.2 s</td>\
         <td>100.0 GiB</td><td>2.0 MiB</td><td>0</td><td>5</td>"
    );
}

/// Catches: an unknown value shown as zero, or a real zero shown as unknown. A member whose
/// report says nothing shows "unknown" in every value cell (and the number of
/// under-replicated blobs is "unknown", with no zero in the answer's words); a member that
/// reports real zeros shows `0`, `0 B` and `0 ms`; and a known zero is the green answer,
/// not the unknown one. Mutants: `null` rendered as `0`; zero rendered as unknown.
#[test]
fn null_is_unknown_never_zero_and_a_real_zero_stays_zero() {
    let (_, html) = page_of(&storage_doc());
    assert!(
        html.contains("<dt>Under-replicated blobs</dt><dd>unknown</dd>"),
        "{html}"
    );
    assert!(html.contains("under-replicated blobs unknown"), "{html}");
    let silent = row(&html, "Write tier", 2);
    assert_eq!(silent.matches("<td>unknown</td>").count(), 7, "{silent}");
    assert!(!silent.contains("<td>0"), "{silent}");
    let members = row(&html, "Members", 2);
    assert_eq!(members.matches("<td>unknown</td>").count(), 3, "{members}");

    let mut zero = storage_node(3, "active");
    for key in [
        "unflushed_bytes",
        "unflushed_blocks",
        "oldest_unflushed_ms",
        "budget_bytes",
        "used_bytes",
        "flush_failures",
        "flush_conflicts",
    ] {
        zero[key] = json!(0);
    }
    let doc = json!({
        "members_total": 1, "members_active": 1, "not_active": [], "silent": [],
        "unreported": [], "debts_total": 0, "under_replicated_blobs": 0,
        "reasons": [], "nodes": [zero],
    });
    let (_, html) = page_of(&doc);
    assert_eq!(
        row(&html, "Write tier", 3),
        "<tr><th scope=\"row\">3</th><td>0 B</td><td>0</td><td>0 ms</td>\
         <td>0 B</td><td>0 B</td><td>0</td><td>0</td>"
    );
    assert!(html.contains("every blob has its copies"), "{html}");
    assert!(
        html.contains("<dt>Under-replicated blobs</dt><dd>0</dd>"),
        "{html}"
    );
    assert!(!html.contains("Why it is not zero"), "{html}");
}

/// Catches: a stale report not marked as stale. The API's own lists decide: a silent
/// member is "stale", one that cannot say is "cannot say", one that is not active is
/// "not judged", and only an active member in none of them is "fresh". Mutant: the
/// silent list ignored, so a stale member reads fresh; "fresh" given to every member.
#[test]
fn a_stale_report_is_marked_in_words() {
    let doc = json!({
        "members_total": 4, "members_active": 3, "not_active": [4], "silent": [2],
        "unreported": [3], "debts_total": 0, "under_replicated_blobs": null,
        "reasons": ["member 4 not active"],
        "nodes": [
            storage_node(1, "active"),
            storage_node_unknown(2, "active"),
            storage_node_unknown(3, "active"),
            storage_node_unknown(4, "draining"),
        ],
    });
    let (_, html) = page_of(&doc);
    for (id, word) in [
        (1, "fresh"),
        (2, "stale"),
        (3, "cannot say"),
        (4, "not judged"),
    ] {
        let members = row(&html, "Members", id);
        assert!(
            members.contains(&format!("{word}</span>")),
            "{id}: {members}"
        );
        for other in ["fresh", "stale", "cannot say", "not judged"] {
            if other != word {
                assert!(
                    !members.contains(&format!("{other}</span>")),
                    "{id}: {members}"
                );
            }
        }
    }
    assert!(html.contains("<dt>Not active</dt><dd>4</dd>"), "{html}");
    assert!(
        html.contains("<dt>Cannot say (no count)</dt><dd>3</dd>"),
        "{html}"
    );
}

/// Catches: a member's state shown by colour alone, and a planned-down member shown as
/// merely down. Every state is words in its pill; `planned_down` adds "(planned)".
/// Mutant: the planned suffix dropped.
#[test]
fn a_state_is_in_words_and_a_planned_down_is_said() {
    let mut down = storage_node_unknown(2, "down");
    down["planned_down"] = json!(true);
    let doc = json!({
        "members_total": 3, "members_active": 1, "not_active": [2, 3], "silent": [],
        "unreported": [], "debts_total": 0, "under_replicated_blobs": null,
        "reasons": [], "nodes": [storage_node(1, "active"), down, storage_node(3, "joining")],
    });
    let (_, html) = page_of(&doc);
    assert!(row(&html, "Members", 1).contains("pill ok\">active</span>"));
    assert!(row(&html, "Members", 2).contains("pill fail\">down (planned)</span>"));
    assert!(row(&html, "Members", 3).contains("pill pending\">joining</span>"));
}

/// Catches: an attacker-influenced string written as markup. A member state and a reason
/// carry `<script>`, quotes and `&`; they reach the page as text, and no element they name
/// exists. Mutant: a reason written through a raw write (none exists, so the mutant is a
/// reason written unescaped by hand in the page, which this assertion sees).
#[test]
fn hostile_strings_reach_the_page_as_text() {
    let hostile = "<script>alert(\"x\")</script> & 'y'";
    let mut node = storage_node(1, hostile);
    node["state"] = json!(hostile);
    let doc = json!({
        "members_total": 1, "members_active": 0, "not_active": [1], "silent": [],
        "unreported": [], "debts_total": 0, "under_replicated_blobs": null,
        "reasons": [hostile], "nodes": [node],
    });
    let (status, html) = page_of(&doc);
    assert_eq!(status, 200, "{html}");
    let escaped = "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; &#39;y&#39;";
    assert!(html.contains(&format!("<li>{escaped}</li>")), "{html}");
    assert!(html.contains(&format!("{escaped}</span>")), "{html}");
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("alert(\"x\")"), "{html}");
}

/// Catches: a document the page cannot read passing as an empty or zero page, and an
/// API failure losing its status. A member id that is not a number is `502` with the
/// reason as escaped text; the API's `404` (no state store) and `503` (membership
/// unreadable) are the page's own status, with the error text. Mutant: a failed read
/// answered `200`.
#[test]
fn a_failed_or_unreadable_read_is_the_pages_status() {
    let doc = json!({
        "members_total": 1, "members_active": 1, "not_active": [], "silent": [],
        "unreported": [], "debts_total": 0, "under_replicated_blobs": 0,
        "reasons": [], "nodes": [{"id": "<b>1</b>", "state": "active"}],
    });
    let (status, html) = page_of(&doc);
    assert_eq!(status, 502, "{html}");
    assert!(html.contains("a document this page cannot read"), "{html}");
    assert!(!html.contains("<b>1</b>"), "{html}");

    for (code, error) in [(404, "no state store"), (503, "storage health task failed")] {
        let fake = Fake::default().with("/v1/storage", code, json!({"error": error}).to_string());
        let a = render(&fake, "/ui/storage");
        let html = body(&a);
        assert_eq!(a.status(), code, "{html}");
        assert!(html.contains(error), "{html}");
        assert!(html.contains("<h1>Storage</h1>"), "{html}");
    }
}

/// Catches: a farm with no member shown as a table of nothing or as healthy. It says no
/// member is registered, and the under-replicated number is unknown. Mutant: the empty
/// case writing the table head only.
#[test]
fn a_farm_with_no_member_says_so() {
    let doc = json!({
        "members_total": 0, "members_active": 0, "not_active": [], "silent": [],
        "unreported": [], "debts_total": 0, "under_replicated_blobs": null,
        "reasons": ["no storage member is registered"], "nodes": [],
    });
    let (status, html) = page_of(&doc);
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("No storage member is registered."), "{html}");
    assert!(!html.contains("<table"), "{html}");
    assert!(
        html.contains("<li>no storage member is registered</li>"),
        "{html}"
    );
}

/// Catches: tables an assistive reader cannot navigate. Each table has column headers
/// with `scope="col"`, each row a row header, and the page has the one `h1` and the
/// section headings. Mutant: the row header written as a plain cell.
#[test]
fn the_tables_have_headers_and_the_page_has_headings() {
    let a = render(&storage_farm(), "/ui/storage");
    let html = body(&a);
    assert_eq!(html.matches("<h1>").count(), 1, "{html}");
    for heading in ["Replication", "Members", "Write tier"] {
        assert!(html.contains(&format!("<h2>{heading}</h2>")), "{heading}");
    }
    assert_eq!(html.matches("<table").count(), 2, "{html}");
    assert_eq!(html.matches("<th scope=\"col\">").count(), 6 + 8, "{html}");
    assert_eq!(html.matches("<th scope=\"row\">").count(), 4, "{html}");
}

/// Catches: a page nobody can reach. The Overview lists it, every page's navigation links
/// it, and the path routes to it (a neighbouring path does not). Mutants: the Overview
/// entry dropped; the route dropped (the page is a 404).
#[test]
fn the_page_is_reachable_from_the_overview_and_the_navigation() {
    let overview = body(&render(&fleet_farm(), "/ui"));
    // The Overview's own list, not the navigation every page has.
    let overview = &overview[overview.find("<main>").expect("main")..];
    assert!(
        overview.contains("<p><a href=\"/ui/storage\">Storage</a></p>"),
        "{overview}"
    );
    let builds = body(&render(&storage_farm(), "/ui/builds"));
    assert!(
        builds.contains("<li><a href=\"/ui/storage\">Storage</a></li>"),
        "{builds}"
    );
    assert_eq!(render(&storage_farm(), "/ui/storage").status(), 200);
    assert_eq!(render(&storage_farm(), "/ui/storage/x").status(), 404);
}

/// Catches: an age written in the wrong unit at a boundary. Milliseconds below a second,
/// seconds below a minute, minutes after; unknown for none. Mutant: the second boundary
/// compared at 100 instead of 1000.
#[test]
fn an_age_is_in_the_unit_that_fits() {
    assert_eq!(age(None), "unknown");
    assert_eq!(age(Some(0)), "0 ms");
    assert_eq!(age(Some(999)), "999 ms");
    assert_eq!(age(Some(1000)), "1.0 s");
    assert_eq!(age(Some(4200)), "4.2 s");
    assert_eq!(age(Some(60_000)), "1.0 min");
    assert_eq!(
        age(Some(u64::MAX)),
        format!("{:.1} min", u64::MAX as f64 / 60_000.0)
    );
}
