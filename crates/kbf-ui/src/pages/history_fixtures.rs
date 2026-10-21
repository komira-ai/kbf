//! The documents the Targets and Target pages are tested against: a typed fake of the
//! history routes (`GET /v1/tests`, `/v1/shapes/{key}`, `/v1/grid`) with one flaky test
//! whose label, builds and configuration are hostile text.

use serde_json::{Value, json};

use crate::tests::Fake;

/// The flaky test's shape key.
pub(crate) const FLAKY: &str = "1111111111111111";
/// A test that passes.
pub(crate) const STEADY: &str = "2222222222222222";

/// The label of the flaky test: markup a hostile build could send.
pub(crate) const HOSTILE: &str = "//app:<script>alert(1)</script>\"'&";
/// The builds, oldest first, with hostile ids.
pub(crate) const BUILDS: [&str; 4] = ["b1", "b2<i>", "b3&amp;x", "b4\"q/z"];

/// The first strip in `html`, as the markup of its list.
fn first_strip(html: &str) -> &str {
    let start = html.find("<ol class=\"strip\"").expect("a strip");
    let end = start + html[start..].find("</ol>").expect("a closed strip");
    &html[start..end]
}

/// The text after each `marker` in `strip`, up to the next `"` or `<`, past the first
/// `skip` pieces.
fn after(strip: &str, marker: &str, skip: usize) -> Vec<String> {
    strip
        .split(marker)
        .skip(skip)
        .map(|piece| {
            piece
                .split(['"', '<'])
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

/// The (escaped) accessible name of each cell of the first strip of `html`, in order.
pub(crate) fn strip_labels(html: &str) -> Vec<String> {
    // The list's own label comes before the cells'.
    after(first_strip(html), "aria-label=\"", 2)
}

/// The class of each cell of the first strip of `html`, in order.
pub(crate) fn strip_classes(html: &str) -> Vec<String> {
    after(first_strip(html), "class=\"cell ", 1)
}

/// The glyph of each cell of the first strip of `html`, in order.
pub(crate) fn strip_glyphs(html: &str) -> Vec<String> {
    // The list's own tag ends in `">`, then the first cell's link tag runs to its last
    // attribute, whose quote is followed by `>` and the glyph.
    after(first_strip(html), "\">", 2)
}

/// A shape row as `/v1/tests` lists it.
pub(crate) fn shape(key: &str, target: Option<&str>, status: &str, proven: bool) -> Value {
    json!({
        "instance": "main", "key": key, "cfg": "cfgA", "kind": "test",
        "href": format!("/v1/shapes/{key}?instance=main&cfg=cfgA"),
        "runs_href": format!("/v1/shapes/{key}/runs?instance=main&cfg=cfgA"),
        "target": target, "first_seen": "2026-10-01T00:00:00.000000Z",
        "last_seen": "2026-10-04T00:00:00.000000Z",
        "status": status,
        "flaky": {"proven": proven, "proof_digest": null, "since": null},
    })
}

/// A grid of the `BUILDS` (newest first, as the route answers) and the rows given, each
/// with its cells in the same order.
pub(crate) fn grid(rows: &[(&str, Vec<&str>)]) -> Value {
    let columns: Vec<Value> = BUILDS
        .iter()
        .enumerate()
        .rev()
        .map(|(i, id)| {
            json!({"id": id, "href": format!("/v1/invocations/{id}"),
                "first_seen": format!("2026-10-0{}T00:00:00.000000Z", i + 1), "complete": true})
        })
        .collect();
    let rows: Vec<Value> = rows
        .iter()
        .map(|(key, cells)| {
            let cells: Vec<Value> = cells.iter().map(|s| json!({"status": s})).collect();
            json!({"instance": "main", "key": key, "cfg": "cfgA", "cells": cells})
        })
        .collect();
    json!({"columns": columns, "rows": rows, "shapes": []})
}

/// The list URL of the first page.
pub(crate) const LIST_URL: &str = "/v1/tests?page_size=50";
/// The grid URL the list reads for the flaky and the steady test.
pub(crate) const LIST_GRID_URL: &str =
    "/v1/grid?shapes=1111111111111111%2C2222222222222222&last=16";
/// The flaky test's own row.
pub(crate) const ONE_URL: &str = "/v1/shapes/1111111111111111?instance=main&cfg=cfgA";
/// The grid its page reads.
pub(crate) const ONE_GRID_URL: &str = "/v1/grid?shapes=1111111111111111&last=50&instance=main";

/// The flaky test: it fails in the newest and third newest builds and passes between.
pub(crate) const ALTERNATING: [&str; 4] = ["pass", "fail", "pass", "fail"];

/// A farm whose flaky test alternates (oldest first: pass, fail, pass, fail) and whose
/// steady test passed in the two oldest builds and was not run since.
pub(crate) fn farm() -> Fake {
    // Cells are in column order, newest first.
    let newest_first = |oldest_first: &[&'static str]| -> Vec<&'static str> {
        oldest_first.iter().rev().copied().collect()
    };
    let flaky = newest_first(&ALTERNATING);
    let steady = ["absent", "absent", "pass", "pass"];
    let both = grid(&[(FLAKY, flaky.clone()), (STEADY, steady.to_vec())]);
    let one = grid(&[(FLAKY, flaky)]);
    let flaky_row = shape(FLAKY, Some(HOSTILE), "failing", true);
    Fake::default()
        .json(
            LIST_URL,
            &json!({"shapes": [flaky_row.clone(), shape(STEADY, None, "passing", false)]}),
        )
        .json(LIST_GRID_URL, &both)
        .json(ONE_URL, &flaky_row)
        .json(ONE_GRID_URL, &one)
        .json(
            "/v1/shapes/1111111111111111",
            &json!({"shapes": [flaky_row]}),
        )
}
