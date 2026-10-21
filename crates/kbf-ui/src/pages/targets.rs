//! The Targets page: the tests the farm knows, each with a strip of its last builds'
//! outcomes (`docs/design/kbf-ui-v1.md` 4, "Targets"). Read-only; it filters by the
//! server's status words and pages with the server's own token, and it offers no text
//! search because the API has none (`?target=` is a `400` there).

use serde::Deserialize;

use super::strip::{GridDoc, grid_url, history, strip};
use super::{failed, pill, query_value, segment};
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::{Page, Reader};
use crate::{Answer, Source};

/// The page's path.
pub(crate) const TARGETS_PATH: &str = "/ui/targets";

/// Targets: the list of tests and one grid of their latest builds.
pub(crate) const TARGETS: Page = Page {
    name: "Targets",
    reads: &["/v1/tests", "/v1/grid"],
};

/// The status words `GET /v1/tests?status=` accepts; the page offers these and no other.
const STATUSES: [&str; 4] = ["passing", "failing", "flaky", "oom_killed"];

/// Rows of one page: the most the grid takes keys for.
const PAGE_SIZE: usize = 50;

/// Builds in each row's strip.
const STRIP_BUILDS: usize = 16;

/// `GET /v1/tests` and `GET /v1/shapes/{key}`: a page of shape rows.
#[derive(Debug, Deserialize)]
pub(super) struct ShapesDoc {
    pub(super) shapes: Vec<Shape>,
    pub(super) next_page: Option<String>,
    /// True when the answer covers only part of the instances.
    #[serde(default)]
    pub(super) partial: bool,
}

/// One `(instance, key, cfg)` of the shape catalog, with a test's status.
#[derive(Debug, Deserialize)]
pub(super) struct Shape {
    pub(super) instance: String,
    pub(super) key: String,
    pub(super) cfg: String,
    pub(super) kind: String,
    /// The label the server read from the build's paths.
    pub(super) target: Option<String>,
    pub(super) status: Option<String>,
    pub(super) flaky: Option<Flaky>,
    pub(super) first_seen: Option<String>,
    pub(super) last_seen: Option<String>,
}

/// The server's flake judgement of a test.
#[derive(Debug, Deserialize)]
pub(super) struct Flaky {
    pub(super) proven: bool,
}

/// `/ui/targets/{key}?instance=&cfg=`: one row's page.
pub(super) fn target_path(key: &str, instance: &str, cfg: &str) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("instance", instance);
    q.append_pair("cfg", cfg);
    format!("{TARGETS_PATH}/{}?{}", segment(key), q.finish())
}

/// What a row is called: its label, else its key.
pub(super) fn label(shape: &Shape) -> &str {
    shape.target.as_deref().unwrap_or(&shape.key)
}

/// A status word in its pill; a word the page does not know is shown as itself.
pub(super) fn status_pill(h: &mut Html, status: Option<&str>) {
    match status {
        Some("passing") => pill(h, "passing", "ok"),
        Some(word @ ("failing" | "oom_killed")) => pill(h, word, "fail"),
        Some(word) => pill(h, word, "other"),
        None => pill(h, "no status", "pending"),
    }
}

/// The query of the list: its status filter and page token.
fn list_query(status: Option<&str>, token: Option<&str>) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    if let Some(status) = status {
        q.append_pair("status", status);
    }
    if let Some(token) = token {
        q.append_pair("page_token", token);
    }
    q.finish()
}

/// The `/v1/tests` URL of one page of the list. Built outside the async page function:
/// a `form_urlencoded::Serializer` is not `Send`, and the page's future must be.
fn tests_url(status: Option<&str>, token: Option<&str>) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("page_size", &PAGE_SIZE.to_string());
    if let Some(status) = status {
        q.append_pair("status", status);
    }
    if let Some(token) = token {
        q.append_pair("page_token", token);
    }
    format!("/v1/tests?{}", q.finish())
}

/// The Targets page.
pub(crate) async fn targets<S: Source>(source: &S, query: &str) -> Answer {
    let status = query_value(query, "status").filter(|s| !s.is_empty());
    let token = query_value(query, "page_token").filter(|s| !s.is_empty());
    if let Some(s) = status.as_deref().filter(|s| !STATUSES.contains(s)) {
        return unknown_status(s);
    }
    let mut r = Reader::new(source, &TARGETS);
    let asked = tests_url(status.as_deref(), token.as_deref());
    let doc: ShapesDoc = match r.json(&asked).await {
        Ok(doc) => doc,
        Err(e) => return failed(&TARGETS, TARGETS_PATH, r.asked(), &e),
    };
    // One grid for the page's rows. A grid that cannot be read removes the strips; it
    // never fails the list, and nothing is drawn in its place.
    let mut keys: Vec<&str> = Vec::new();
    for shape in &doc.shapes {
        if !keys.contains(&shape.key.as_str()) {
            keys.push(&shape.key);
        }
    }
    let grid = if keys.is_empty() {
        None
    } else {
        Some(
            r.json::<GridDoc>(&grid_url(&keys.join(","), STRIP_BUILDS, None))
                .await,
        )
    };
    let next = doc
        .next_page
        .as_deref()
        .and_then(|n| n.split_once('?'))
        .and_then(|(_, q)| query_value(q, "page_token"));
    let body = layout::page(TARGETS.name, TARGETS_PATH, r.asked(), |h| {
        h.el("h1", &[], |h| {
            h.text("Targets");
        });
        filters(h, status.as_deref());
        let hint = format!(
            "Tests the farm has run. Each strip is the last {STRIP_BUILDS} builds, \
             oldest on the left and newest on the right; the name of a cell says the \
             outcome, the build and when it was first seen. The API has no text \
             search, so the list is filtered by status only."
        );
        h.el("p", &[attr("class", "hint")], |h| {
            h.text(&hint);
        });
        if doc.partial {
            h.el("p", &[attr("class", "problem")], |h| {
                h.text("This list is partial: the index did not cover every instance.");
            });
        }
        if let Some(Err(e)) = &grid {
            super::problem(h, e);
        }
        if doc.shapes.is_empty() {
            h.el("p", &[], |h| {
                h.text("No test matches.");
            });
        } else {
            rows(h, &doc.shapes, grid.as_ref().and_then(|g| g.as_ref().ok()));
        }
        if let Some(token) = next {
            let more = format!(
                "{TARGETS_PATH}?{}",
                list_query(status.as_deref(), Some(&token))
            );
            let href = Href::root_relative(&more).expect("the list's path is root-relative");
            h.el("p", &[], |h| {
                h.link(&href, "Next page");
            });
        }
    });
    Answer::html(200, body)
}

/// The `400` page for a status filter the API does not know. Nothing is read.
fn unknown_status(status: &str) -> Answer {
    let body = layout::page(TARGETS.name, TARGETS_PATH, &[], |h| {
        h.el("h1", &[], |h| {
            h.text("Targets");
        });
        h.el("p", &[attr("class", "problem")], |h| {
            h.text("No status is called ")
                .el("code", &[], |h| {
                    h.text(status);
                })
                .text(&format!(": one of {}.", STATUSES.join(", ")));
        });
    });
    Answer::html(400, body)
}

/// The status filter: every word, and all.
fn filters(h: &mut Html, current: Option<&str>) {
    h.el("nav", &[attr("aria-label", "status filter")], |h| {
        h.el("ul", &[attr("class", "filters")], |h| {
            for word in std::iter::once(None).chain(STATUSES.iter().map(|s| Some(*s))) {
                let path = match word {
                    Some(w) => format!("{TARGETS_PATH}?{}", list_query(Some(w), None)),
                    None => TARGETS_PATH.to_owned(),
                };
                let href = Href::root_relative(&path).expect("the list's path is root-relative");
                h.el("li", &[], |h| {
                    let text = word.unwrap_or("all");
                    if word == current {
                        h.el(
                            "a",
                            &[
                                crate::html::url("href", &href),
                                attr("aria-current", "true"),
                            ],
                            |h| {
                                h.text(text);
                            },
                        );
                    } else {
                        h.link(&href, text);
                    }
                });
            }
        });
    });
}

/// The table: one row per shape, with its strip when the grid was read.
fn rows(h: &mut Html, shapes: &[Shape], grid: Option<&GridDoc>) {
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for head in [
                    "Target",
                    "Instance",
                    "Configuration",
                    "Status",
                    "Flaky (server's word)",
                    "Last builds",
                ] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(head);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for shape in shapes {
                row(h, shape, grid);
            }
        });
    });
}

fn row(h: &mut Html, shape: &Shape, grid: Option<&GridDoc>) {
    let href = Href::root_relative(&target_path(&shape.key, &shape.instance, &shape.cfg))
        .expect("a target page's path is root-relative");
    h.el("tr", &[], |h| {
        h.el("th", &[attr("scope", "row")], |h| {
            h.link(&href, label(shape));
        });
        h.el("td", &[], |h| {
            h.text(&shape.instance);
        });
        h.el("td", &[], |h| {
            h.text(&shape.cfg);
        });
        h.el("td", &[], |h| status_pill(h, shape.status.as_deref()));
        h.el("td", &[], |h| {
            h.text(flaky_word(shape));
        });
        h.el("td", &[], |h| {
            if let Some(grid) = grid {
                let found = history(grid, &shape.instance, &shape.key, &shape.cfg);
                strip(h, &found.runs);
            }
        });
    });
}

/// The server's flake judgement in words.
pub(super) fn flaky_word(shape: &Shape) -> &'static str {
    match &shape.flaky {
        Some(f) if f.proven => "proven",
        Some(_) => "not proven",
        None => "none",
    }
}

#[cfg(test)]
mod tests;
