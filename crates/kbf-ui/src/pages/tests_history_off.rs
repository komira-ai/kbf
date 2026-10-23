//! What the pages that list history say on a farm that keeps none: the history routes
//! answer as unknown routes (`404`, no body), and the page says history is off.

use super::history_fixtures::{ONE_URL, farm as history_farm};
use crate::tests::{Fake, body, render};

fn page(fake: &Fake, path: &str) -> (u16, String) {
    let a = render(fake, path);
    (a.status(), body(&a))
}

/// Catches: the Builds list of a farm with no history answered `404` with the error
/// sentence, or `200` with no word of the flag; the lookup form lost with the list (a live
/// build can still be looked up); a filter dropped from the read; a missing single build
/// (its own `404` with `no_record`) said to be history off, or an API error with a message
/// on the list route. Catches: `is_history_off` always false; the status in `builds` left
/// as the API's; the history note replaced by `problem`.
#[test]
fn the_builds_list_of_a_farm_with_no_history_says_so() {
    let fake = Fake::default().with("/v1/invocations?state=open", 404, "");
    let (status, html) = page(&fake, "/ui/builds?state=open");
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("History is off on this farm"), "{html}");
    assert!(
        html.contains("<code>--history-objects=&lt;URI&gt;</code>"),
        "{html}"
    );
    assert!(
        html.contains("<form method=\"get\" action=\"/ui/builds\""),
        "the lookup stays: {html}"
    );
    assert!(!html.contains("class=\"problem"), "{html}");
    assert_eq!(fake.seen(), ["/v1/invocations?state=open"]);

    let missing = Fake::default().with(
        "/v1/invocations/b9",
        404,
        r#"{"error": "no_record", "id": "b9", "records_since": "2026-10-11T09:00:00Z"}"#,
    );
    let (status, html) = page(&missing, "/ui/builds?id=b9");
    assert_eq!(status, 404, "{html}");
    assert!(html.contains("no_record"), "{html}");
    assert!(!html.contains("History is off"), "{html}");

    let said = Fake::default().with("/v1/invocations", 404, r#"{"error": "no such thing"}"#);
    let (status, html) = page(&said, "/ui/builds");
    assert_eq!(status, 404, "{html}");
    assert!(!html.contains("History is off"), "{html}");
}

/// Catches: the Target page of a farm with no history answered `404`, and a shape the
/// history does not hold (`no_shape`) said to be history off. Catches: `is_history_off`
/// always true or always false.
#[test]
fn the_target_page_of_a_farm_with_no_history_says_so_and_a_missing_shape_is_a_404() {
    let off = Fake::default().with(ONE_URL, 404, "");
    let path = "/ui/targets/1111111111111111?instance=main&cfg=cfgA";
    let (status, html) = page(&off, path);
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("History is off on this farm"), "{html}");

    let rows = Fake::default().with("/v1/shapes/1111111111111111", 404, "");
    let (status, html) = page(&rows, "/ui/targets/1111111111111111");
    assert_eq!(status, 200, "the rows of a key too: {html}");
    assert!(html.contains("History is off on this farm"), "{html}");

    let gone = Fake::default().with(
        ONE_URL,
        404,
        r#"{"error": "no_shape", "key": "1111111111111111"}"#,
    );
    let (status, html) = page(&gone, path);
    assert_eq!(status, 404, "{html}");
    assert!(!html.contains("History is off"), "{html}");

    let (status, html) = page(&history_farm(), path);
    assert_eq!(status, 200, "{html}");
    assert!(!html.contains("History is off"), "{html}");
}
