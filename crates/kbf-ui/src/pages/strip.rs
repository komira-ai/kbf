//! The pass/fail strip of a target (`docs/design/kbf-ui-v1.md` 4, "The target strip"):
//! one cell per build, read from `GET /v1/grid` for one shape, so that a target that
//! passes and fails on the same code is seen without reading a number.
//!
//! The strip runs **oldest build on the left, newest on the right**. A cell is a link to
//! its build with a glyph (never colour alone) and a label that names the outcome, the
//! build and when the build was first seen. The page draws what the route returns:
//! the only arithmetic is a count of the cells it draws.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::build_page;
use crate::html::{Href, Html, attr, url};

/// `GET /v1/grid?shapes=..&last=..`: the builds and, for each shape the builds ran, a
/// row with one cell per build.
#[derive(Debug, Deserialize)]
pub(super) struct GridDoc {
    /// The builds, newest first.
    columns: Vec<Column>,
    rows: Vec<Row>,
}

/// One build of the grid.
#[derive(Debug, Deserialize)]
struct Column {
    id: String,
    first_seen: String,
    /// False when the route read only part of the build's records.
    complete: bool,
}

/// One `(instance, key, cfg)` some build ran.
#[derive(Debug, Deserialize)]
struct Row {
    instance: String,
    key: String,
    cfg: String,
    cells: Vec<Cell>,
}

/// A row's result in one build: `pass`, `fail`, `oom_killed`, `infra`, `absent`, or a
/// word a newer server adds, which the strip shows as itself.
#[derive(Debug, Deserialize)]
struct Cell {
    status: String,
}

/// One cell of a strip: a build and what the target did in it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Run<'a> {
    /// The build's id.
    build: &'a str,
    /// When the build was first seen, as the route writes it.
    first_seen: &'a str,
    /// The grid's word for the cell.
    status: &'a str,
}

impl Run<'_> {
    /// The cell's accessible name.
    fn label(&self) -> String {
        format!(
            "{}, build {}, first seen {}",
            self.word(),
            self.build,
            self.first_seen
        )
    }

    /// The outcome in words: the grid's word, `not run` for an absent cell.
    fn word(&self) -> &str {
        match self.status {
            "absent" => "not run",
            other => other,
        }
    }
}

impl Run<'_> {
    /// The run as a table row: the build, when it was first seen, the outcome in words.
    pub(super) fn row(&self, h: &mut Html) {
        let (glyph, _) = mark(self.status);
        let outcome = self.word();
        let href = Href::root_relative(&build_page(self.build))
            .expect("a build page's path is root-relative");
        h.el("tr", &[], |h| {
            h.el("th", &[attr("scope", "row")], |h| {
                h.link(&href, self.build);
            });
            h.el("td", &[], |h| {
                h.text(self.first_seen);
            });
            h.el("td", &[], |h| {
                h.text(&format!("{glyph} {outcome}"));
            });
        });
    }
}

/// What a grid says about one row of one target.
#[derive(Debug)]
pub(super) struct History<'a> {
    /// The builds the grid covers, oldest first, with the target's cell in each.
    pub(super) runs: Vec<Run<'a>>,
    /// How many of the builds the route read only in part.
    pub(super) partial_builds: usize,
}

/// The history of `(instance, key, cfg)` in `doc`: its row's cells, oldest build first.
/// A build the row has no cell for (a short row) is shown as `missing`, not as a pass.
/// A target no build ran has no row: every build is `absent`.
pub(super) fn history<'a>(doc: &'a GridDoc, instance: &str, key: &str, cfg: &str) -> History<'a> {
    let row = doc
        .rows
        .iter()
        .find(|r| r.instance == instance && r.key == key && r.cfg == cfg);
    let runs = doc
        .columns
        .iter()
        .enumerate()
        .rev()
        .map(|(i, build)| Run {
            build: &build.id,
            first_seen: &build.first_seen,
            status: row.map_or("absent", |r| {
                r.cells.get(i).map_or("missing", |c| c.status.as_str())
            }),
        })
        .collect();
    History {
        runs,
        partial_builds: doc.columns.iter().filter(|c| !c.complete).count(),
    }
}

/// A cell's glyph and the class that colours it. The glyphs differ by shape, so that
/// no outcome is told by colour alone.
fn mark(status: &str) -> (&'static str, &'static str) {
    match status {
        "pass" => ("\u{2713}", "ok"),
        "fail" => ("\u{2717}", "fail"),
        "oom_killed" => ("!", "fail"),
        "absent" => ("\u{2013}", "pending"),
        _ => ("?", "other"),
    }
}

/// The strip: an ordered list, oldest first, one linked cell per run.
pub(super) fn strip(h: &mut Html, runs: &[Run<'_>]) {
    h.el(
        "ol",
        &[
            attr("class", "strip"),
            attr(
                "aria-label",
                "Recent builds, oldest on the left, newest on the right",
            ),
        ],
        |h| {
            for run in runs {
                let (glyph, class) = mark(run.status);
                let class = format!("cell {class}");
                let label = run.label();
                let href = Href::root_relative(&build_page(run.build))
                    .expect("a build page's path is root-relative");
                h.el("li", &[], |h| {
                    h.el(
                        "a",
                        &[
                            attr("class", &class),
                            url("href", &href),
                            attr("title", &label),
                            attr("aria-label", &label),
                        ],
                        |h| {
                            h.text(glyph);
                        },
                    );
                });
            }
        },
    );
}

/// The count line for `runs`: the failures among the builds that ran the target, out
/// of the builds covered, and every other outcome by name, so that a short or quiet
/// strip is not read as a clean one.
pub(super) fn tally(runs: &[Run<'_>]) -> String {
    let total = runs.len();
    if total == 0 {
        return "No build is held in history.".to_owned();
    }
    let mut by: BTreeMap<&str, usize> = BTreeMap::new();
    for run in runs {
        *by.entry(run.status).or_default() += 1;
    }
    let absent = by.remove("absent").unwrap_or(0);
    let ran = total - absent;
    if ran == 0 {
        return format!("None of the last {total} builds ran it.");
    }
    let failed = by.remove("fail").unwrap_or(0);
    let passed = by.remove("pass").unwrap_or(0);
    let mut kept = vec![format!("{passed} passed")];
    kept.extend(by.iter().map(|(word, n)| format!("{n} {word}")));
    let kept = kept.join(", ");
    let quiet = if absent > 0 {
        format!("; {absent} did not run it")
    } else {
        String::new()
    };
    format!(
        "{failed} failed of {ran} builds that ran it ({kept}) in the last {total} builds{quiet}."
    )
}

/// The `/v1/grid` URL for `keys` (16 hex digits each, joined by commas) over the last
/// `builds` builds, of `instance` when given.
pub(super) fn grid_url(keys: &str, builds: usize, instance: Option<&str>) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("shapes", keys);
    q.append_pair("last", &builds.to_string());
    if let Some(instance) = instance {
        q.append_pair("instance", instance);
    }
    format!("/v1/grid?{}", q.finish())
}

#[cfg(test)]
mod tests;
