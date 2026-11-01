//! The Queue page: what waits for a lease, from `GET /v1/queue`: the depth and the
//! oldest age, the queue per platform and QoS level, the operations no live worker can
//! run with the scheduler's reason, and the operations placement keeps off some nodes.

use self::whole::whole_node;
use super::sched::{Avoided, Group, QueueDoc, Waiting, age, code, memory_note, node_link};
use super::started::{self, HEALTH};
use super::{definitions, failed};
use crate::html::{Html, attr};
use crate::layout;
use crate::read::{Page, Reader};
use crate::{Answer, Source};

/// The page's path.
pub(crate) const QUEUE_PATH: &str = "/ui/queue";

/// Queue: `/v1/queue`, which takes no query.
pub(crate) const QUEUE: Page = Page {
    name: "Queue",
    reads: &["/v1/queue", HEALTH],
};

/// The Queue page.
pub(crate) async fn queue<S: Source>(source: &S) -> Answer {
    let mut r = Reader::new(source, &QUEUE);
    let doc = match r.json::<QueueDoc>("/v1/queue").await {
        Ok(doc) => doc,
        Err(e) => return failed(&QUEUE, QUEUE_PATH, r.asked(), &e),
    };
    let started = started::read(&mut r).await;
    let body = layout::page(QUEUE.name, QUEUE_PATH, r.asked(), |h| {
        h.el("h1", &[], |h| {
            h.text("Queue");
        });
        memory_note(h, &started);
        let oldest = doc
            .oldest_age_ms
            .map_or_else(|| "nothing waits".to_owned(), age);
        definitions(
            h,
            &[
                ("Operations queued", doc.depth.to_string()),
                ("Oldest waiting", oldest),
            ],
        );
        by_platform(h, &doc.by_platform);
        waiting(h, &doc.waiting);
        avoided(h, &doc.avoided, doc.avoided_total);
        if let Some(whole) = &doc.whole_node {
            whole_node(h, whole);
        }
    });
    Answer::html(200, body)
}

pub(super) fn heading(h: &mut Html, text: &str) {
    h.el("h2", &[], |h| {
        h.text(text);
    });
}

pub(super) fn hint(h: &mut Html, text: &str) {
    h.el("p", &[attr("class", "hint")], |h| {
        h.text(text);
    });
}

/// A table with `columns` as its headers and `rows` written by `body`.
pub(super) fn table(h: &mut Html, columns: &[&str], body: impl FnOnce(&mut Html)) {
    h.el("div", &[attr("class", "scroll")], |h| {
        h.el("table", &[attr("class", "rows")], |h| {
            h.el("thead", &[], |h| {
                h.el("tr", &[], |h| {
                    for name in columns {
                        h.el("th", &[attr("scope", "col")], |h| {
                            h.text(name);
                        });
                    }
                });
            });
            h.el("tbody", &[], body);
        });
    });
}

pub(super) fn cell(h: &mut Html, text: &str) {
    h.el("td", &[], |h| {
        h.text(text);
    });
}

fn by_platform(h: &mut Html, groups: &[Group]) {
    heading(h, "By platform and QoS");
    if groups.is_empty() {
        hint(h, "Nothing is queued.");
        return;
    }
    table(h, &["Platform", "QoS", "Queued", "Oldest"], |h| {
        for g in groups {
            h.el("tr", &[], |h| {
                cell(h, &g.platform);
                cell(h, &g.qos);
                cell(h, &g.depth.to_string());
                cell(h, &age(g.oldest_age_ms));
            });
        }
    });
}

fn waiting(h: &mut Html, groups: &[Waiting]) {
    heading(h, "Waiting because no worker can run them");
    if groups.is_empty() {
        hint(
            h,
            "No queued operation lacks a worker that can run it; those queued only wait for room.",
        );
        return;
    }
    table(h, &["Reason", "Operations", "Example", "Digest"], |h| {
        for g in groups {
            h.el("tr", &[], |h| {
                cell(h, &g.reason);
                cell(h, &g.count.to_string());
                h.el("td", &[], |h| {
                    code(h, &g.example_operation);
                    h.text(&format!(" on instance {}", g.example_instance));
                });
                h.el("td", &[], |h| code(h, &g.example_digest));
            });
        }
    });
}

fn avoided(h: &mut Html, rows: &[Avoided], total: u64) {
    heading(h, "Kept off some nodes");
    if rows.is_empty() && total == 0 {
        hint(h, "No queued operation is kept off a node.");
        return;
    }
    table(
        h,
        &["Operation", "Instance", "Digest", "Avoids nodes"],
        |h| {
            for a in rows {
                h.el("tr", &[], |h| {
                    h.el("td", &[], |h| code(h, &a.operation));
                    cell(h, &a.instance);
                    h.el("td", &[], |h| code(h, &a.digest));
                    h.el("td", &[], |h| {
                        for (i, node) in a.nodes.iter().enumerate() {
                            if i > 0 {
                                h.text(", ");
                            }
                            node_link(h, node);
                        }
                    });
                });
            }
        },
    );
    if u64::try_from(rows.len()).is_ok_and(|shown| total > shown) {
        hint(
            h,
            &format!(
                "Showing {} of {total}: the API lists the first of them.",
                rows.len()
            ),
        );
    }
}

mod whole;

#[cfg(test)]
mod tests;
