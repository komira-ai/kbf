//! What the Overview shows for each answer `GET /v1/health` and `GET /v1/invocations`
//! can give. Each test names the defect it catches.

use serde_json::{Value, json};

use crate::html::escape;
use crate::pages::build_page;
use crate::tests::{Fake, HOSTILE, LATEST_URL, body, fleet_farm, health_doc, latest_build, render};

fn page(fake: &Fake, path: &str) -> (u16, String) {
    let a = render(fake, path);
    (a.status(), body(&a))
}

/// A farm answering `health` and an empty build list.
fn farm_with(health: &Value) -> Fake {
    Fake::default()
        .json("/v1/health", health)
        .json(LATEST_URL, &json!({"invocations": []}))
}

/// `<dt>name</dt><dd>value</dd>`.
fn field(name: &str, value: &str) -> String {
    format!("<dt>{name}</dt><dd>{value}</dd>")
}

/// Catches: the Overview not reading `/v1/health` and the build list (it read nothing
/// before); a count shown under another's label (every count has its own value in the
/// fixture); a null oldest age or under-replicated count shown as a number; the
/// storage members, queue depth or leases dropped; the latest builds not linked to
/// their pages or their counters swapped; a failing check or the readiness missing.
#[test]
fn the_overview_shows_health_counts_and_the_latest_builds() {
    let fake = fleet_farm();
    let (status, html) = page(&fake, "/ui");
    assert_eq!(status, 200, "{html}");
    assert_eq!(fake.seen(), ["/v1/health", LATEST_URL]);
    assert!(html.contains("<title>Overview - kbf</title>"), "{html}");
    assert!(
        html.contains("<span class=\"pill fail\">not ready</span>"),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            "<th scope=\"row\">store</th><td>{}</td>",
            escape(HOSTILE)
        )),
        "{html}"
    );
    for (n, label) in [
        (21, "nodes"),
        (5, "serving"),
        (3, "cordoned"),
        (2, "draining"),
        (4, "drained"),
        (1, "quarantined"),
        (6, "disconnected"),
    ] {
        let cell = format!("<span class=\"n\">{n}</span><span class=\"label\">{label}</span>");
        assert!(html.contains(&cell), "{label}: {html}");
    }
    for (name, value) in [
        ("Queue depth", "9"),
        ("Oldest queued operation", "not measured"),
        ("Leases held", "13"),
        ("Storage members", "2 of 3 active"),
        ("Under-replicated blobs", "unknown"),
    ] {
        assert!(html.contains(&field(name, value)), "{name}: {html}");
    }
    assert!(
        html.contains("<a href=\"/ui/nodes\">All nodes</a>"),
        "{html}"
    );
    assert!(!html.contains("/ui/updates"), "{html}");
    assert!(
        html.contains(
            "<th scope=\"row\"><a href=\"/ui/builds/build-1\">build-1</a></th>\
             <td>closed, complete</td>"
        ),
        "{html}"
    );
    assert!(
        html.contains("<td>11</td><td>12</td><td>13</td>"),
        "ran, hits, failed in their columns: {html}"
    );
    assert!(html.contains("(older builds exist)"), "{html}");
}

/// Catches: markup from the API written as markup anywhere on the page (a failing
/// check's reason, a build id and its instance names), and a build id not encoded as one
/// path segment in its link. Catches: `Html::text` writing raw.
#[test]
fn hostile_strings_render_as_text() {
    let (_, html) = page(&fleet_farm(), "/ui");
    for needle in ["<script", "<img", "onerror=alert(2)>"] {
        assert!(!html.contains(needle), "{needle}: {html}");
    }
    assert!(html.contains(&escape(HOSTILE)), "{html}");
    let link = build_page(HOSTILE);
    assert!(link.starts_with("/ui/builds/%3Cscript%3E"), "{link}");
    assert!(html.contains(&format!("<a href=\"{link}\">")), "{html}");
}

/// Catches: a ready farm shown as not ready or the reverse (catches: the condition
/// inverted), and a failing table kept when nothing fails.
#[test]
fn a_ready_farm_says_ready_and_lists_no_failing_check() {
    let mut health = health_doc();
    health["ready"] = json!(true);
    health["failing"] = json!([]);
    let (status, html) = page(&farm_with(&health), "/ui");
    assert_eq!(status, 200);
    assert!(
        html.contains("<span class=\"pill ok\">ready</span>"),
        "{html}"
    );
    assert!(!html.contains("not ready"), "{html}");
    assert!(html.contains("No check fails."), "{html}");
    assert!(!html.contains("Failing check"), "{html}");
}

/// Catches: an unknown storage state shown as zero or as healthy: a null
/// `under_replicated_blobs` is "unknown" and a real zero is "0" (catches: the count
/// defaulted to 0); a storage that cannot be read shown without its reason, or with
/// the reason as markup; a server with no storage members shown as unreadable.
#[test]
fn storage_is_unknown_unavailable_or_absent_and_never_a_made_up_zero() {
    let mut zero = health_doc();
    zero["storage"]["under_replicated_blobs"] = json!(0);
    let (_, html) = page(&farm_with(&zero), "/ui");
    assert!(
        html.contains(&field("Under-replicated blobs", "0")),
        "{html}"
    );

    let mut unreadable = health_doc();
    unreadable["storage"] = Value::Null;
    unreadable["storage_unavailable"] = json!(HOSTILE);
    let (status, html) = page(&farm_with(&unreadable), "/ui");
    assert_eq!(status, 200);
    let members = format!("unknown ({})", escape(HOSTILE));
    assert!(html.contains(&field("Storage members", &members)), "{html}");
    assert!(
        html.contains(&field("Under-replicated blobs", "unknown")),
        "{html}"
    );
    assert!(!html.contains("<script"), "{html}");

    let mut none = health_doc();
    none["storage"] = Value::Null;
    let (_, html) = page(&farm_with(&none), "/ui");
    assert!(
        html.contains(&field(
            "Storage members",
            "none: this server keeps no storage members"
        )),
        "{html}"
    );
}

/// Catches: a known oldest age dropped (shown as "not measured" when the API sends it).
#[test]
fn a_known_oldest_queue_age_is_shown() {
    let mut health = health_doc();
    health["queue"]["oldest_age_ms"] = json!(1234);
    let (_, html) = page(&farm_with(&health), "/ui");
    assert!(
        html.contains(&field("Oldest queued operation", "1234 ms")),
        "{html}"
    );
}

/// Catches: a build list that does not answer failing the whole page (the health is
/// still the point), shown as "no builds", or shown without a way to a build by id; a
/// `partial` list shown as complete; an empty list shown as a failure.
#[test]
fn the_build_list_is_optional_and_honest() {
    let fake = Fake::default().json("/v1/health", &health_doc()).with(
        LATEST_URL,
        404,
        r#"{"error": "no_history"}"#,
    );
    let (status, html) = page(&fake, "/ui");
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("The list of builds is not available ("),
        "{html}"
    );
    assert!(html.contains("no_history"), "{html}");
    assert!(
        html.contains("<a href=\"/ui/builds\">Builds page</a>"),
        "{html}"
    );
    assert!(
        html.contains(&field("Queue depth", "9")),
        "the health is still shown"
    );

    // A farm with no history answers the list as an unknown route: say so, name the flag.
    let off = Fake::default()
        .json("/v1/health", &health_doc())
        .with(LATEST_URL, 404, "");
    let (status, html) = page(&off, "/ui");
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("History is off on this farm"), "{html}");
    assert!(html.contains("--history-objects"), "{html}");
    assert!(!html.contains("not available"), "{html}");

    let (status, html) = page(&farm_with(&health_doc()), "/ui");
    assert_eq!(status, 200);
    assert!(html.contains("The farm has recorded no build."), "{html}");
    assert!(!html.contains("not available"), "{html}");

    let partial = Fake::default().json("/v1/health", &health_doc()).json(
        LATEST_URL,
        &json!({"invocations": [latest_build("b")], "partial": true}),
    );
    let (_, html) = page(&partial, "/ui");
    assert!(
        html.contains("Partial: some instance could not be read"),
        "{html}"
    );
    assert!(!html.contains("older builds exist"), "{html}");
}

/// Catches: a health read that failed shown as a healthy farm or as a bare `200`; the
/// API's `Retry-After` not passed on with its `503`; a page that reloads itself at an
/// outage (`?refresh=` is ignored when the health did not read); an unreachable API not
/// `502`; a health document without the keys the page needs shown as a guess.
#[test]
fn a_health_that_does_not_read_answers_its_status_and_does_not_refresh() {
    let fake = Fake::default().with_headers(
        "/v1/health",
        503,
        &[("Retry-After", "30"), ("set-cookie", "s=1")],
        r#"{"error": "draining"}"#,
    );
    let a = render(&fake, "/ui?refresh=10");
    let html = body(&a);
    assert_eq!(a.status(), 503, "{html}");
    assert!(
        a.headers().contains(&("retry-after", "30")),
        "{:?}",
        a.headers()
    );
    assert!(html.contains("draining"), "{html}");
    assert!(
        !html.contains("not ready") && !html.contains("No check fails."),
        "{html}"
    );
    assert!(!html.contains("http-equiv"), "{html}");

    let (status, html) = page(&Fake::default().down("/v1/health"), "/ui");
    assert_eq!(status, 502, "{html}");

    let malformed = Fake::default().json("/v1/health", &json!({"ready": true}));
    let (status, html) = page(&malformed, "/ui");
    assert_eq!(status, 502, "{html}");
    assert!(html.contains("cannot read"), "{html}");
}
