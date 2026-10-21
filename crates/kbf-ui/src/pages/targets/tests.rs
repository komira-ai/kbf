//! What the Targets page shows for each answer the history routes can give.

use serde_json::json;

use crate::html::escape;
use crate::pages::history_fixtures::{
    BUILDS, FLAKY, HOSTILE, LIST_GRID_URL, LIST_URL, STEADY, farm, grid, shape, strip_classes,
    strip_labels,
};
use crate::tests::{Fake, body, render};

fn page(fake: &Fake, path: &str) -> (u16, String) {
    let a = render(fake, path);
    (a.status(), body(&a))
}

/// The markup of each row of the table, after its header.
fn rows(html: &str) -> Vec<&str> {
    html.split("<tr><th scope=\"row\">").skip(1).collect()
}

/// Catches: a row's strip drawn from another row of the grid (matched by position, not
/// by instance, key and configuration), or oldest right. The flaky test alternates; the
/// steady one passed in the two oldest builds and was not run since.
#[test]
fn each_row_has_its_own_strip_oldest_left() {
    let (status, html) = page(&farm(), "/ui/targets");
    assert_eq!(status, 200, "{html}");
    let rows = rows(&html);
    assert_eq!(rows.len(), 2);
    assert_eq!(strip_classes(rows[0]), ["ok", "fail", "ok", "fail"]);
    assert_eq!(strip_classes(rows[1]), ["ok", "ok", "pending", "pending"]);
    let label = |i: usize, word: &str| {
        escape(&format!(
            "{word}, build {}, first seen 2026-10-0{}T00:00:00.000000Z",
            BUILDS[i],
            i + 1
        ))
    };
    assert_eq!(
        strip_labels(rows[1]),
        [
            label(0, "pass"),
            label(1, "pass"),
            label(2, "not run"),
            label(3, "not run")
        ]
    );
}

/// Catches: two configurations of one test (one key) drawn with each other's strip: the
/// grid's rows are told apart by instance, key and configuration.
#[test]
fn rows_of_one_key_are_told_apart_by_configuration() {
    let mut other = shape(FLAKY, None, "passing", false);
    other["cfg"] = "cfgB".into();
    let a = shape(FLAKY, None, "failing", false);
    let mut grid = grid(&[(FLAKY, vec!["fail", "fail", "fail", "fail"])]);
    let mut b = grid["rows"][0].clone();
    b["cfg"] = "cfgB".into();
    b["cells"] =
        json!([{"status": "pass"}, {"status": "pass"}, {"status": "pass"}, {"status": "pass"}]);
    grid["rows"].as_array_mut().expect("rows").push(b);
    let fake = Fake::default()
        .json(LIST_URL, &json!({"shapes": [a, other]}))
        // Both rows share a key, so it is asked once.
        .json("/v1/grid?shapes=1111111111111111&last=16", &grid);
    let (status, html) = page(&fake, "/ui/targets");
    assert_eq!(status, 200, "{html}");
    let rows = rows(&html);
    assert_eq!(strip_classes(rows[0]), ["fail"; 4]);
    assert_eq!(strip_classes(rows[1]), ["ok"; 4]);
    assert_eq!(fake.seen().len(), 2, "{:?}", fake.seen());
}

/// Catches: a row not linking to its Target page with its instance and configuration
/// (the page could not tell the rows of a key apart); the label written unescaped; a
/// test with no label shown blank instead of by its key.
#[test]
fn rows_link_to_the_target_page_and_the_label_is_escaped() {
    let (_, html) = page(&farm(), "/ui/targets");
    let rows = rows(&html);
    assert!(
        rows[0].starts_with(&format!(
            "<a href=\"/ui/targets/{FLAKY}?instance=main&amp;cfg=cfgA\">{}</a>",
            escape(HOSTILE)
        )),
        "{}",
        rows[0]
    );
    assert!(
        rows[1].starts_with(&format!(
            "<a href=\"/ui/targets/{STEADY}?instance=main&amp;cfg=cfgA\">{STEADY}</a>"
        )),
        "{}",
        rows[1]
    );
    for raw in ["<script", "alert(1)</script>"] {
        assert!(!html.contains(raw), "{raw}: {html}");
    }
    assert!(
        rows[0].contains("<span class=\"pill fail\">failing</span>"),
        "{}",
        rows[0]
    );
    assert!(
        rows[1].contains("<span class=\"pill ok\">passing</span>"),
        "{}",
        rows[1]
    );
    assert!(rows[0].contains("<td>proven</td>"), "{}", rows[0]);
    assert!(rows[1].contains("<td>not proven</td>"), "{}", rows[1]);
    assert!(
        html.contains("<th scope=\"col\">Last builds</th>"),
        "{html}"
    );
}

/// Catches: a page that reads other routes than its two, or reads the grid once per row,
/// or for more builds than its strip shows.
#[test]
fn it_reads_the_list_and_one_grid() {
    let fake = farm();
    page(&fake, "/ui/targets");
    assert_eq!(fake.seen(), [LIST_URL, LIST_GRID_URL]);
}

/// Catches: the status filter not passed to the API, not marked as the current one, or
/// a list with no match read for a grid of no shapes (the grid refuses an empty list).
#[test]
fn a_status_filter_is_passed_on_and_an_empty_list_reads_no_grid() {
    let fake = Fake::default().json(
        "/v1/tests?page_size=50&status=failing",
        &json!({"shapes": []}),
    );
    let (status, html) = page(&fake, "/ui/targets?status=failing");
    assert_eq!(status, 200, "{html}");
    assert_eq!(fake.seen(), ["/v1/tests?page_size=50&status=failing"]);
    assert!(html.contains("No test matches."), "{html}");
    assert!(!html.contains("<table"), "{html}");
    assert!(
        html.contains("<a href=\"/ui/targets?status=failing\" aria-current=\"true\">failing</a>"),
        "{html}"
    );
    for word in ["passing", "flaky", "oom_killed"] {
        assert!(
            html.contains(&format!("<a href=\"/ui/targets?status={word}\">{word}</a>")),
            "{word}: {html}"
        );
    }
    assert!(html.contains("<a href=\"/ui/targets\">all</a>"), "{html}");
}

/// Catches: a status the API does not know sent to it (a `400` page from the API instead
/// of ours), or written as markup; the filter links offering a status the API refuses.
#[test]
fn an_unknown_status_is_refused_before_any_read() {
    let fake = Fake::default();
    for (query, shown) in [("status=slower", "slower"), ("status=%3Cb%3E", "&lt;b&gt;")] {
        let (status, html) = page(&fake, &format!("/ui/targets?{query}"));
        assert_eq!(status, 400, "{html}");
        assert!(html.contains(&format!("<code>{shown}</code>")), "{html}");
        assert!(
            html.contains("one of passing, failing, flaky, oom_killed"),
            "{html}"
        );
    }
    assert!(fake.seen().is_empty());
}

/// Catches: paging that drops the API's token or the filter, or builds the next link from
/// the API's own URL (which would leave the UI); a token sent back without encoding.
#[test]
fn the_next_page_keeps_the_filter_and_the_token_encoded() {
    let first = Fake::default()
        .json(
            "/v1/tests?page_size=50&status=failing",
            &json!({"shapes": [shape(FLAKY, None, "failing", false)],
                "next_page": "/v1/tests?page_size=50&status=failing&page_token=t%2B1%26x"}),
        )
        .json(
            "/v1/grid?shapes=1111111111111111&last=16",
            &grid(&[(FLAKY, vec!["fail", "fail", "fail", "fail"])]),
        );
    let (_, html) = page(&first, "/ui/targets?status=failing");
    assert!(
        html.contains(
            "<a href=\"/ui/targets?status=failing&amp;page_token=t%2B1%26x\">Next page</a>"
        ),
        "{html}"
    );

    // Following it asks the API for the same token.
    let second = Fake::default().json(
        "/v1/tests?page_size=50&status=failing&page_token=t%2B1%26x",
        &json!({"shapes": []}),
    );
    let (status, html) = page(&second, "/ui/targets?status=failing&page_token=t%2B1%26x");
    assert_eq!(status, 200, "{html}");
    assert!(!html.contains("Next page"), "{html}");
}

/// Catches: the next-page link of an unfiltered list gaining a `status` of its own, or
/// losing the token.
#[test]
fn the_next_page_of_an_unfiltered_list_has_only_the_token() {
    let fake = Fake::default()
        .json(
            LIST_URL,
            &json!({"shapes": [shape(FLAKY, None, "failing", false)],
                "next_page": "/v1/tests?page_size=50&page_token=t"}),
        )
        .json(
            "/v1/grid?shapes=1111111111111111&last=16",
            &grid(&[(FLAKY, vec!["fail", "fail", "fail", "fail"])]),
        );
    let (_, html) = page(&fake, "/ui/targets");
    assert!(
        html.contains("<a href=\"/ui/targets?page_token=t\">Next page</a>"),
        "{html}"
    );
}

/// Catches: a partial list shown as the whole one; a grid that cannot be read failing the
/// list or leaving cells that look like data.
#[test]
fn a_partial_list_and_an_unreadable_grid_are_said() {
    let partial = Fake::default().json(LIST_URL, &json!({"shapes": [], "partial": true}));
    let (_, html) = page(&partial, "/ui/targets");
    assert!(html.contains("This list is partial"), "{html}");
    let (_, whole) = page(
        &Fake::default().json(LIST_URL, &json!({"shapes": []})),
        "/ui/targets",
    );
    assert!(!whole.contains("partial"), "{whole}");

    let fake = farm().down(LIST_GRID_URL);
    let (status, html) = page(&fake, "/ui/targets");
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("connection refused"), "{html}");
    assert!(!html.contains("class=\"strip\""), "{html}");
    assert_eq!(rows(&html).len(), 2);
}

/// Catches: the API's refusals shown as another page: history off or a bad token is its
/// own status, an overloaded API keeps its `Retry-After`.
#[test]
fn the_apis_refusals_are_the_pages() {
    let off = Fake::default().with(
        LIST_URL,
        404,
        r#"{"error": "history is off: start the farm with --history-objects"}"#,
    );
    let (status, html) = page(&off, "/ui/targets");
    assert_eq!(status, 404);
    assert!(html.contains("--history-objects"), "{html}");
    let busy = Fake::default().with_headers(LIST_URL, 503, &[("Retry-After", "3")], "{}");
    let a = render(&busy, "/ui/targets");
    assert_eq!(a.status(), 503);
    assert!(a.headers().contains(&("retry-after", "3")));
}

/// Catches: a status word drawn as a pass or left out: the server's `oom_killed` is a
/// failure mark, `flaky` and a word added later are shown as themselves in the neutral
/// mark, and a row with no status says so.
#[test]
fn every_status_word_has_its_own_mark() {
    let mut none = shape("4444444444444444", None, "x", false);
    none["status"] = serde_json::Value::Null;
    let shapes: Vec<_> = [
        shape("1111111111111111", None, "oom_killed", false),
        shape("2222222222222222", None, "flaky", false),
        shape("3333333333333333", None, "slower", false),
        none,
    ]
    .into();
    let fake = Fake::default()
        .json(LIST_URL, &json!({"shapes": shapes}))
        .down(
            "/v1/grid?shapes=1111111111111111%2C2222222222222222%2C3333333333333333%2C4444444444444444&last=16",
        );
    let (_, html) = page(&fake, "/ui/targets");
    let rows = rows(&html);
    for (row, pill) in rows.iter().zip([
        "pill fail\">oom_killed",
        "pill other\">flaky",
        "pill other\">slower",
        "pill pending\">no status",
    ]) {
        assert!(row.contains(pill), "{pill}: {row}");
    }
}
