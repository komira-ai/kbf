//! What the Target page shows for each answer the history routes can give.

use serde_json::{Value, json};

use crate::html::escape;
use crate::pages::history_fixtures::{
    BUILDS, FLAKY, HOSTILE, ONE_GRID_URL, ONE_URL, farm, grid, shape, strip_classes, strip_glyphs,
    strip_labels,
};
use crate::tests::{Fake, body, render};

const PAGE: &str = "/ui/targets/1111111111111111?instance=main&cfg=cfgA";

fn page(fake: &Fake, path: &str) -> (u16, String) {
    let a = render(fake, path);
    (a.status(), body(&a))
}

/// The label a cell of build number `i` (0 is the oldest) has with outcome `word`.
fn label(word: &str, i: usize) -> String {
    escape(&format!(
        "{word}, build {}, first seen 2026-10-0{}T00:00:00.000000Z",
        BUILDS[i],
        i + 1
    ))
}

/// Catches (the strip of P8): a flaky target whose pass and fail are not seen in run
/// order. Builds b1 to b4 pass, fail, pass, fail; the route lists them newest first, the
/// strip is oldest left. Mutants: the columns not reversed (newest left, so the labels
/// come out b4 first); a fail given the pass glyph or class; a cell labelled with
/// another build's outcome; the grid's row read for another target.
#[test]
fn the_strip_alternates_in_run_order_oldest_left() {
    let (status, html) = page(&farm(), PAGE);
    assert_eq!(status, 200, "{html}");
    assert_eq!(
        strip_labels(&html),
        [
            label("pass", 0),
            label("fail", 1),
            label("pass", 2),
            label("fail", 3)
        ]
    );
    assert_eq!(strip_classes(&html), ["ok", "fail", "ok", "fail"]);
    assert_eq!(
        strip_glyphs(&html),
        ["\u{2713}", "\u{2717}", "\u{2713}", "\u{2717}"]
    );
    assert!(
        html.contains("oldest on the left, newest on the right"),
        "{html}"
    );
}

/// Catches: a count line that is not the count of the cells drawn, or says nothing of
/// the builds that did not run the target. Mutants: failures counted as passes; the
/// total taken over the builds that ran it, not the builds covered.
#[test]
fn the_count_line_counts_the_failures_of_the_builds_that_ran_it() {
    let (_, html) = page(&farm(), PAGE);
    assert!(
        html.contains(
            "<p class=\"tally\">2 failed of 4 builds that ran it (2 passed) in the last 4 builds.</p>"
        ),
        "{html}"
    );
}

/// Catches: the table not being the strip's twin, newest first, so a cell would be known
/// only by hover; a build link that does not encode the id (a `/` in an id would name
/// another path). The strip itself is oldest left.
#[test]
fn the_table_lists_the_same_builds_newest_first_with_encoded_links() {
    let (_, html) = page(&farm(), PAGE);
    let at = |id: &str| {
        html.find(&format!("<th scope=\"row\"><a href=\"/ui/builds/{id}\">"))
            .unwrap_or_else(|| panic!("no row for {id}: {html}"))
    };
    let order = [
        at("b4%22q%2Fz"),
        at("b3%26amp%3Bx"),
        at("b2%3Ci%3E"),
        at("b1"),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    assert!(
        html.contains("<td>2026-10-04T00:00:00.000000Z</td><td>\u{2717} fail</td>"),
        "{html}"
    );
    // The cell of the strip links to the same page.
    assert!(
        html.contains("<a class=\"cell fail\" href=\"/ui/builds/b4%22q%2Fz\""),
        "{html}"
    );
}

/// Catches: an attacker-influenced string written as markup: the target label, a build
/// id, in the heading, the facts, the cells' names, the table and the footer.
#[test]
fn hostile_text_is_escaped_everywhere() {
    let (status, html) = page(&farm(), PAGE);
    assert_eq!(status, 200);
    assert!(html.contains(&escape(HOSTILE)), "{html}");
    for raw in ["<script", "<i>", "alert(1)</script>", "b4\"q"] {
        assert!(!html.contains(raw), "{raw}: {html}");
    }
    assert!(html.contains("b2&lt;i&gt;"), "{html}");
}

/// Catches: the facts of the row missing or under another's name: the server's label,
/// key, instance, configuration, its flake word and status, first and last seen.
#[test]
fn the_facts_are_the_servers() {
    let (_, html) = page(&farm(), PAGE);
    for (name, value) in [
        ("Target (read by the server)", escape(HOSTILE)),
        ("Key", FLAKY.to_owned()),
        ("Instance", "main".to_owned()),
        ("Configuration", "cfgA".to_owned()),
        ("Flaky (the server's word)", "proven".to_owned()),
        ("First seen", "2026-10-01T00:00:00.000000Z".to_owned()),
        ("Last seen", "2026-10-04T00:00:00.000000Z".to_owned()),
    ] {
        let pair = format!("<dt>{}</dt><dd>{value}</dd>", escape(name));
        assert!(html.contains(&pair), "{pair}: {html}");
    }
    assert!(
        html.contains("Status: <span class=\"pill fail\">failing</span>"),
        "{html}"
    );
    assert!(html.contains("aria-current=\"page\">Targets</a>"), "{html}");
}

/// Catches: a page that reads more or other than its two routes, or reads the grid for
/// a shape that is not the one asked (the grid is asked for the server's key and
/// instance, one shape, the last 50 builds).
#[test]
fn it_reads_the_row_and_one_grid() {
    let fake = farm();
    page(&fake, PAGE);
    assert_eq!(fake.seen(), [ONE_URL, ONE_GRID_URL]);
}

/// Catches: an outcome the page does not know drawn as a pass; the grid's `infra` and
/// `oom_killed` drawn as a fail or a pass; an absent cell drawn as a run. Each is shown
/// as itself, with a glyph that is not the pass glyph, and counted by name.
#[test]
fn an_unknown_outcome_is_shown_as_itself_not_as_a_pass() {
    let cells = ["wobbly", "infra", "oom_killed", "absent"];
    let one = grid(&[(FLAKY, cells.to_vec())]);
    let fake = farm().json(ONE_GRID_URL, &one);
    let (status, html) = page(&fake, PAGE);
    assert_eq!(status, 200, "{html}");
    // The grid lists newest first: the oldest build holds the last cell.
    assert_eq!(
        strip_labels(&html),
        [
            label("not run", 0),
            label("oom_killed", 1),
            label("infra", 2),
            label("wobbly", 3)
        ]
    );
    assert_eq!(strip_classes(&html), ["pending", "fail", "other", "other"]);
    assert_eq!(strip_glyphs(&html), ["\u{2013}", "!", "?", "?"]);
    assert!(
        html.contains(
            "0 failed of 3 builds that ran it (0 passed, 1 infra, 1 oom_killed, 1 wobbly) \
             in the last 4 builds; 1 did not run it."
        ),
        "{html}"
    );
}

/// Catches: a target no build ran drawn as a quiet pass (it has no row in the grid); a
/// row shorter than the builds filling in a pass for the builds it lacks.
#[test]
fn a_target_no_build_ran_and_a_short_row_are_not_passes() {
    let none = grid(&[]);
    let (status, html) = page(&farm().json(ONE_GRID_URL, &none), PAGE);
    assert_eq!(status, 200);
    assert_eq!(strip_classes(&html), ["pending"; 4]);
    assert!(html.contains("None of the last 4 builds ran it."), "{html}");

    let short = grid(&[(FLAKY, vec!["fail", "pass"])]);
    let (_, html) = page(&farm().json(ONE_GRID_URL, &short), PAGE);
    assert_eq!(strip_classes(&html), ["other", "other", "ok", "fail"]);
    assert_eq!(strip_glyphs(&html), ["?", "?", "\u{2713}", "\u{2717}"]);
    assert!(html.contains(&label("missing", 0)), "{html}");
}

/// Catches: a grid with no build, which read as "all passed"; a build read only in part
/// (the route stopped at its record cap) not said, so a missing run is read as not run.
#[test]
fn no_builds_and_partly_read_builds_are_said() {
    let empty = json!({"columns": [], "rows": [], "shapes": []});
    let (status, html) = page(&farm().json(ONE_GRID_URL, &empty), PAGE);
    assert_eq!(status, 200);
    assert!(html.contains("No build is held in history."), "{html}");
    assert!(!html.contains("<table class=\"rows\">"), "{html}");

    let mut partial = grid(&[(FLAKY, vec!["fail", "pass", "fail", "pass"])]);
    partial["columns"][1]["complete"] = Value::Bool(false);
    partial["columns"][3]["complete"] = Value::Bool(false);
    let (_, html) = page(&farm().json(ONE_GRID_URL, &partial), PAGE);
    assert!(
        html.contains("2 of these builds were read only in part"),
        "{html}"
    );
    let (_, whole) = page(&farm(), PAGE);
    assert!(!whole.contains("read only in part"), "{whole}");
}

/// Catches: a shape that is not a test drawn with a strip (it has no outcomes), or a
/// grid read for it; facts missing for a row with no status, flake state, label or
/// times (they are shown as unknown, not blank).
#[test]
fn a_shape_that_is_not_a_test_has_no_strip() {
    let compile = json!({
        "instance": "main", "key": FLAKY, "cfg": "cfgA", "kind": "other",
        "href": "/v1/shapes/x", "runs_href": "/v1/shapes/x/runs", "target": null,
    });
    let fake = Fake::default().json(ONE_URL, &compile);
    let (status, html) = page(&fake, PAGE);
    assert_eq!(status, 200, "{html}");
    assert_eq!(fake.seen(), [ONE_URL]);
    assert!(html.contains("This shape is not a test"), "{html}");
    assert!(!html.contains("class=\"strip\""), "{html}");
    assert!(html.contains(&format!(
        "<dt>Target (read by the server)</dt><dd>{FLAKY}</dd>"
    )));
    assert!(
        html.contains("<dt>First seen</dt><dd>unknown</dd>"),
        "{html}"
    );
    assert!(
        html.contains("<dt>Flaky (the server&#39;s word)</dt><dd>none</dd>"),
        "{html}"
    );
    assert!(
        html.contains("<span class=\"pill pending\">no status</span>"),
        "{html}"
    );
}

/// Catches: a grid that cannot be read failing the page, or drawing a strip in its
/// place; the row's facts lost with it.
#[test]
fn an_unreadable_grid_removes_the_strip_and_keeps_the_facts() {
    let fake = farm().down(ONE_GRID_URL);
    let (status, html) = page(&fake, PAGE);
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("connection refused"), "{html}");
    assert!(!html.contains("class=\"strip\""), "{html}");
    assert!(html.contains("<dt>Key</dt>"), "{html}");

    let malformed = farm().json(ONE_GRID_URL, &json!({"columns": 3}));
    let (status, html) = page(&malformed, PAGE);
    assert_eq!(status, 200);
    assert!(html.contains("a document this page cannot read"), "{html}");
}

/// Catches: an answer of the API that is not 200 shown as a page of another status, or
/// losing the header that status depends on: a key no instance may read is `404`, an
/// overloaded API `503` with its `Retry-After`.
#[test]
fn the_apis_refusals_are_the_pages() {
    let gone = Fake::default().with(
        ONE_URL,
        404,
        r#"{"error": "no_shape", "message": "no shape 1111111111111111 is known"}"#,
    );
    let (status, html) = page(&gone, PAGE);
    assert_eq!(status, 404);
    assert!(html.contains("no_shape"), "{html}");
    let busy = Fake::default().with_headers(
        ONE_URL,
        503,
        &[("Retry-After", "7")],
        r#"{"error": "busy"}"#,
    );
    let a = render(&busy, PAGE);
    assert_eq!(a.status(), 503);
    assert!(a.headers().contains(&("retry-after", "7")));
    assert_eq!(busy.seen(), [ONE_URL]);
}

/// Catches: a key given with no instance and configuration answered with a strip of an
/// arbitrary row; its rows not linked with the instance and configuration encoded (a
/// `&` in an instance would end the value); rows that were cut off not said.
#[test]
fn a_key_without_an_instance_lists_its_rows_with_encoded_links() {
    let mut row = shape(FLAKY, None, "failing", false);
    row["instance"] = "i&<1>".into();
    row["cfg"] = "c\"'".into();
    let fake = Fake::default().json("/v1/shapes/1111111111111111", &json!({"shapes": [row]}));
    let (status, html) = page(&fake, "/ui/targets/1111111111111111");
    assert_eq!(status, 200, "{html}");
    assert_eq!(fake.seen(), ["/v1/shapes/1111111111111111"]);
    assert!(
        html.contains(
            "<a href=\"/ui/targets/1111111111111111?instance=i%26%3C1%3E&amp;cfg=c%22%27\">\
             i&amp;&lt;1&gt;, c&quot;&#39;</a>"
        ),
        "{html}"
    );
    assert!(!html.contains("More rows exist"), "{html}");

    // Only one of the two given is the same as none.
    let (_, only) = page(&fake, "/ui/targets/1111111111111111?instance=main");
    assert!(only.contains("This key has these rows"), "{only}");

    let more = Fake::default().json(
        "/v1/shapes/1111111111111111",
        &json!({"shapes": [], "next_page": "/v1/shapes/1111111111111111?page_token=t", "partial": true}),
    );
    let (_, html) = page(&more, "/ui/targets/1111111111111111");
    assert!(
        html.contains("More rows exist than this page lists."),
        "{html}"
    );
    // Either sign alone says so: a next page, or an answer over part of the instances.
    for doc in [
        json!({"shapes": [], "next_page": "/v1/shapes/1111111111111111?page_token=t"}),
        json!({"shapes": [], "partial": true}),
    ] {
        let fake = Fake::default().json("/v1/shapes/1111111111111111", &doc);
        let (_, html) = page(&fake, "/ui/targets/1111111111111111");
        assert!(
            html.contains("More rows exist than this page lists."),
            "{html}"
        );
    }
}

/// Catches: a key or an instance taken from the URL written into an API URL without
/// encoding (a key `a<b` or `a/../b` naming another route), a query that is not UTF-8
/// passed on as bytes, and a path with an empty key answered as a page.
#[test]
fn the_keys_in_the_url_are_encoded_for_the_api() {
    let fake = Fake::default();
    let (status, _) = page(&fake, "/ui/targets/a%3Cb%2Fc");
    assert_eq!(status, 404);
    assert_eq!(fake.seen(), ["/v1/shapes/a%3Cb%2Fc"]);

    let fake = Fake::default();
    let (status, _) = page(&fake, "/ui/targets/a%3Cb%2Fc?instance=main&cfg=c");
    assert_eq!(status, 404);
    assert_eq!(fake.seen(), ["/v1/shapes/a%3Cb%2Fc?instance=main&cfg=c"]);

    let fake = Fake::default();
    let (status, _) = page(&fake, "/ui/targets/k?instance=%FF&cfg=a%26b");
    assert_eq!(status, 404);
    assert_eq!(fake.seen(), ["/v1/shapes/k?instance=%EF%BF%BD&cfg=a%26b"]);

    let fake = Fake::default();
    for path in [
        "/ui/targets/",
        "/ui/targets/k/extra",
        "/ui/targets/%2e%2e",
        "/ui/targets/.",
    ] {
        let (status, html) = page(&fake, path);
        assert_eq!(status, 404, "{path}");
        assert!(html.contains("No page is at"), "{path}: {html}");
    }
    assert!(fake.seen().is_empty());
}
