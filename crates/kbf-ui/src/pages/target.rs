//! The Target page: one test's recent builds as a pass/fail strip, to spot flakiness at
//! a glance (`docs/design/kbf-ui-v1.md` 4, "Target"). Read-only. The page's label, status
//! and flake word are the server's; the page computes none of them, only a count of the
//! cells it draws.
//!
//! `{key}` is the server's shape key, a lookup and nothing else. A key is told apart by
//! `?instance=&cfg=` (the Targets list links with both); with either missing, the page
//! lists the key's rows and links each.

use super::strip::{GridDoc, History, grid_url, history, strip, tally};
use super::targets::{Shape, ShapesDoc, TARGETS_PATH, flaky_word, label, status_pill, target_path};
use super::{definitions, failed, problem, query_value, segment};
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::{Page, Reader};
use crate::{Answer, Source};

/// Target: the row (or the key's rows) and one grid of its builds.
pub(crate) const TARGET: Page = Page {
    name: "Target",
    reads: &["/v1/shapes/{key}", "/v1/grid"],
};

/// Builds in the strip.
const STRIP_BUILDS: usize = 50;

/// The Target page for shape key `key`.
pub(crate) async fn target<S: Source>(source: &S, key: &str, query: &str) -> Answer {
    let mut r = Reader::new(source, &TARGET);
    match (query_value(query, "instance"), query_value(query, "cfg")) {
        (Some(instance), Some(cfg)) => one(&mut r, key, &instance, &cfg).await,
        _ => rows_of(&mut r, key).await,
    }
}

/// `/v1/shapes/{key}?instance=&cfg=`. Built outside the async page functions: a
/// `form_urlencoded::Serializer` is not `Send`, and the pages' futures must be.
fn shape_url(key: &str, instance: &str, cfg: &str) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("instance", instance);
    q.append_pair("cfg", cfg);
    format!("/v1/shapes/{}?{}", segment(key), q.finish())
}

/// The page of one `(instance, key, cfg)`.
async fn one<S: Source>(r: &mut Reader<'_, S>, key: &str, instance: &str, cfg: &str) -> Answer {
    let shape_url = shape_url(key, instance, cfg);
    let shape: Shape = match r.json(&shape_url).await {
        Ok(shape) => shape,
        Err(e) => return failed(&TARGET, TARGETS_PATH, r.asked(), &e),
    };
    // The grid names rows by the server's own key, not the path's spelling of it.
    let grid = if shape.kind == "test" {
        Some(
            r.json::<GridDoc>(&grid_url(&shape.key, STRIP_BUILDS, Some(&shape.instance)))
                .await,
        )
    } else {
        None
    };
    let title = label(&shape).to_owned();
    let body = layout::page(TARGET.name, TARGETS_PATH, r.asked(), |h| {
        h.el("h1", &[], |h| {
            h.text("Target ").el("code", &[], |h| {
                h.text(&title);
            });
        });
        crumbs(h);
        facts(h, &shape);
        h.el("section", &[attr("class", "card")], |h| {
            h.el("h2", &[], |h| {
                h.text("Recent builds");
            });
            match &grid {
                None => {
                    h.el("p", &[], |h| {
                        h.text("This shape is not a test: the farm keeps no outcomes for it.");
                    });
                }
                Some(Err(e)) => problem(h, e),
                Some(Ok(doc)) => {
                    let found = history(doc, &shape.instance, &shape.key, &shape.cfg);
                    builds(h, &found);
                }
            }
        });
    });
    Answer::html(200, body)
}

fn crumbs(h: &mut Html) {
    let href = Href::root_relative(TARGETS_PATH).expect("the list's path is root-relative");
    h.el("p", &[attr("class", "crumbs")], |h| {
        h.link(&href, "Targets");
    });
}

/// What the catalog row says, with the status as a pill.
fn facts(h: &mut Html, shape: &Shape) {
    let none = || "unknown".to_owned();
    let fields: [(&str, String); 7] = [
        ("Target (read by the server)", label(shape).to_owned()),
        ("Key", shape.key.clone()),
        ("Instance", shape.instance.clone()),
        ("Configuration", shape.cfg.clone()),
        ("Flaky (the server's word)", flaky_word(shape).to_owned()),
        ("First seen", shape.first_seen.clone().unwrap_or_else(none)),
        ("Last seen", shape.last_seen.clone().unwrap_or_else(none)),
    ];
    definitions(h, &fields);
    h.el("p", &[], |h| {
        h.text("Status: ");
        status_pill(h, shape.status.as_deref());
    });
}

/// The strip, its count line and the same cells as a table, newest first, so that no
/// cell is known only by its name on hover.
fn builds(h: &mut Html, found: &History<'_>) {
    let line = tally(&found.runs);
    h.el("p", &[attr("class", "tally")], |h| {
        h.text(&line);
    });
    if found.partial_builds > 0 {
        let note = format!(
            "{} of these builds were read only in part (they hold more records than the \
             route reads), so a missing run may be a run it did not reach.",
            found.partial_builds
        );
        h.el("p", &[attr("class", "problem")], |h| {
            h.text(&note);
        });
    }
    h.el("p", &[attr("class", "hint")], |h| {
        h.text(
            "One cell per build, oldest on the left, newest on the right: \u{2713} pass, \
             \u{2717} fail, ! killed for memory, \u{2013} the build did not run it, ? any other \
             word. The outcome is the target's last result in that build. Each cell links to its build.",
        );
    });
    strip(h, &found.runs);
    if found.runs.is_empty() {
        return;
    }
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for head in ["Build (newest first)", "Build first seen", "Outcome"] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(head);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for run in found.runs.iter().rev() {
                run.row(h);
            }
        });
    });
}

/// The page of a key given without an instance and configuration: its rows.
async fn rows_of<S: Source>(r: &mut Reader<'_, S>, key: &str) -> Answer {
    let url = format!("/v1/shapes/{}", segment(key));
    let doc: ShapesDoc = match r.json(&url).await {
        Ok(doc) => doc,
        Err(e) => return failed(&TARGET, TARGETS_PATH, r.asked(), &e),
    };
    let body = layout::page(TARGET.name, TARGETS_PATH, r.asked(), |h| {
        h.el("h1", &[], |h| {
            h.text("Target ").el("code", &[], |h| {
                h.text(key);
            });
        });
        crumbs(h);
        h.el("p", &[attr("class", "hint")], |h| {
            h.text("This key has these rows, each an instance and a configuration:");
        });
        h.el("ul", &[], |h| {
            for shape in &doc.shapes {
                let href =
                    Href::root_relative(&target_path(&shape.key, &shape.instance, &shape.cfg))
                        .expect("a target page's path is root-relative");
                h.el("li", &[], |h| {
                    h.link(&href, &format!("{}, {}", shape.instance, shape.cfg));
                });
            }
        });
        if doc.next_page.is_some() || doc.partial {
            h.el("p", &[attr("class", "problem")], |h| {
                h.text("More rows exist than this page lists.");
            });
        }
    });
    Answer::html(200, body)
}

#[cfg(test)]
mod tests;
