//! The Leases page: what the scheduler holds now, from `GET /v1/leases`: each lease's
//! node, phase, age, operation, digest, platform and QoS, filtered to one node and
//! paged as the API pages it. The page computes nothing: the ages are the API's, the
//! next page is the API's own link carried over to this page's path.

use form_urlencoded::Serializer;

use super::sched::{Lease, LeasesDoc, age, code, memory_note, node_link};
use super::{pill, problem};
use crate::html::{Href, Html, attr, url};
use crate::layout;
use crate::read::{Page, ReadError, Reader};
use crate::{Answer, Source};

/// The page's path.
pub(crate) const LEASES_PATH: &str = "/ui/leases";

/// The route the page reads.
const ROUTE: &str = "/v1/leases";

/// The query keys the page hands to the route, in the order it writes them. Any other
/// key of the page's own query is dropped, so the API's `400` for an unknown key is
/// never the page's doing.
const FORWARDED: [&str; 3] = ["node", "page_size", "page_token"];

/// Leases: `/v1/leases`.
pub(crate) const LEASES: Page = Page {
    name: "Leases",
    reads: &[ROUTE],
};

/// The forwarded pairs of `query`: the first value of each key of [`FORWARDED`], in
/// that order, without an empty `node` (the form submitted with nothing typed).
fn forwarded(query: &str) -> Vec<(&'static str, String)> {
    let pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    FORWARDED
        .into_iter()
        .filter_map(|key| {
            let (_, value) = pairs.iter().find(|(k, _)| k == key)?;
            (key != "node" || !value.is_empty()).then(|| (key, value.clone()))
        })
        .collect()
}

/// `base` with `pairs` as its query.
fn with_query(base: &str, pairs: &[(&str, String)]) -> String {
    if pairs.is_empty() {
        return base.to_owned();
    }
    let mut query = Serializer::new(String::new());
    for (key, value) in pairs {
        query.append_pair(key, value);
    }
    format!("{base}?{}", query.finish())
}

/// This page's link to the page after the one read: the API's `next_page` with its
/// forwarded keys on this page's path. `None` for a link that is not a `/v1/leases`
/// link of the API, which the page does not follow.
fn next_link(next_page: &str) -> Option<Href> {
    let (path, query) = next_page.split_once('?')?;
    (path == ROUTE).then_some(())?;
    let pairs = forwarded(query);
    Href::root_relative(&with_query(LEASES_PATH, &pairs))
}

/// The Leases page; `query` is the page's own (`node`, `page_size`, `page_token`).
pub(crate) async fn leases<S: Source>(source: &S, query: &str) -> Answer {
    let pairs = forwarded(query);
    let node = pairs
        .iter()
        .find(|(k, _)| *k == "node")
        .map(|(_, v)| v.as_str());
    let mut r = Reader::new(source, &LEASES);
    let doc = r.json::<LeasesDoc>(&with_query(ROUTE, &pairs)).await;
    let (status, passed_on) = match &doc {
        Ok(_) => (200, &[][..]),
        Err(e) => (e.status(), e.headers()),
    };
    let body = layout::page(LEASES.name, LEASES_PATH, r.asked(), |h| {
        h.el("h1", &[], |h| {
            h.text("Leases");
        });
        match &doc {
            Ok(doc) => held(h, doc, node),
            Err(e) => failure(h, node, e),
        }
    });
    Answer::html(status, body).passing_on(passed_on)
}

/// The page when the read failed: the filter stays, so a bad value can be corrected.
fn failure(h: &mut Html, node: Option<&str>, e: &ReadError) {
    filter(h, node);
    problem(h, e);
}

/// The filter, the summary, the table and the pager of a page that was read.
fn held(h: &mut Html, doc: &LeasesDoc, node: Option<&str>) {
    memory_note(h);
    filter(h, node);
    h.el("p", &[], |h| {
        h.text("Scheduler term ");
        code(h, &doc.term);
        h.text(&format!(
            ": {} on this page{}.",
            noun(doc.items.len(), "lease", "leases"),
            node.map_or_else(String::new, |n| format!(" for node {n}")),
        ));
    });
    if doc.items.is_empty() {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text(match node {
                Some(_) => "That node holds no lease the scheduler knows of.",
                None => "The scheduler holds no lease.",
            });
        });
    } else {
        table(h, &doc.items);
    }
    if let Some(next) = doc.next_page.as_deref().and_then(next_link) {
        h.el("p", &[attr("class", "pager")], |h| {
            h.link(&next, "Next page");
        });
    }
}

/// `n` of a noun.
fn noun(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The node filter: a GET form to this page, and a way back to every node.
fn filter(h: &mut Html, node: Option<&str>) {
    let action = Href::root_relative(LEASES_PATH).expect("the Leases path is root-relative");
    h.el(
        "form",
        &[
            attr("method", "get"),
            url("action", &action),
            attr("class", "lookup"),
        ],
        |h| {
            h.el("label", &[attr("for", "node")], |h| {
                h.text("Node");
            });
            h.void(
                "input",
                &[
                    attr("id", "node"),
                    attr("name", "node"),
                    attr("type", "text"),
                    attr("value", node.unwrap_or_default()),
                    attr("autocomplete", "off"),
                    attr("spellcheck", "false"),
                ],
            );
            h.void("input", &[attr("type", "submit"), attr("value", "Filter")]);
            if node.is_some() {
                h.link(&action, "All nodes");
            }
        },
    );
}

/// A phase's pill: the word always, the colour only as a second cue.
fn phase(h: &mut Html, phase: &str) {
    let class = match phase {
        "running" => "ok",
        "granted" | "proposed" => "pending",
        _ => "other",
    };
    pill(h, phase, class);
}

fn table(h: &mut Html, rows: &[Lease]) {
    h.el("div", &[attr("class", "scroll")], |h| {
        h.el("table", &[attr("class", "rows")], |h| {
            h.el("thead", &[], |h| {
                h.el("tr", &[], |h| {
                    for name in [
                        "Lease",
                        "Node",
                        "Phase",
                        "Age",
                        "In phase",
                        "Operation",
                        "Instance",
                        "Digest",
                        "Platform",
                        "QoS",
                    ] {
                        h.el("th", &[attr("scope", "col")], |h| {
                            h.text(name);
                        });
                    }
                });
            });
            h.el("tbody", &[], |h| {
                for lease in rows {
                    row(h, lease);
                }
            });
        });
    });
}

fn row(h: &mut Html, lease: &Lease) {
    h.el("tr", &[], |h| {
        h.el("td", &[], |h| code(h, &lease.lease));
        h.el("td", &[], |h| node_link(h, &lease.node));
        h.el("td", &[], |h| phase(h, &lease.phase));
        h.el("td", &[], |h| {
            h.text(&age(lease.age_ms));
        });
        h.el("td", &[], |h| {
            h.text(&age(lease.since_ms));
        });
        h.el("td", &[], |h| code(h, &lease.operation));
        h.el("td", &[], |h| {
            h.text(&lease.instance);
        });
        h.el("td", &[], |h| code(h, &lease.digest));
        h.el("td", &[], |h| {
            h.text(&lease.platform);
        });
        h.el("td", &[], |h| {
            h.text(&lease.qos);
        });
    });
}

#[cfg(test)]
mod tests;
