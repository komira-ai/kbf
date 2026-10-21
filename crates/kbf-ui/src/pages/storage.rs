//! The Storage page: whether every blob has its copies elsewhere, and how each storage
//! member's write tier is doing, as `GET /v1/storage` answers it. Read-only. An absent
//! value is "unknown", never zero (`docs/api.md`, "GET /v1/storage"): a stale or missing
//! report, a node with no write tier and an older build all arrive as `null`.

use super::{bytes, definitions, failed, pill};
use crate::api::{StorageDoc, StorageNode};
use crate::html::{Html, attr};
use crate::layout;
use crate::read::{Page, Reader};
use crate::{Answer, Source};

/// The page's path.
pub(crate) const STORAGE_PATH: &str = "/ui/storage";

/// What a value the API did not send is called.
const UNKNOWN: &str = "unknown";

/// Storage: the one route that carries the members, their debts and their write tiers.
pub(crate) const STORAGE: Page = Page {
    name: "Storage",
    reads: &["/v1/storage"],
};

/// The Storage page.
pub(crate) async fn storage<S: Source>(source: &S) -> Answer {
    let mut r = Reader::new(source, &STORAGE);
    let doc: StorageDoc = match r.json("/v1/storage").await {
        Ok(doc) => doc,
        Err(e) => return failed(&STORAGE, STORAGE_PATH, r.asked(), &e),
    };
    let body = layout::page(STORAGE.name, STORAGE_PATH, r.asked(), |h| {
        h.el("h1", &[], |h| {
            h.text("Storage");
        });
        replication(h, &doc);
        members(h, &doc);
        write_tier(h, &doc);
    });
    Answer::html(200, body)
}

/// A count the API may not know.
fn count(n: Option<u64>) -> String {
    n.map_or_else(|| UNKNOWN.to_owned(), |n| n.to_string())
}

/// Bytes the API may not know.
fn size(n: Option<u64>) -> String {
    n.map_or_else(|| UNKNOWN.to_owned(), bytes)
}

/// An age in milliseconds the API may not know.
fn age(ms: Option<u64>) -> String {
    match ms {
        None => UNKNOWN.to_owned(),
        Some(ms) if ms < 1000 => format!("{ms} ms"),
        Some(ms) if ms < 60_000 => format!("{:.1} s", ms as f64 / 1000.0),
        Some(ms) => format!("{:.1} min", ms as f64 / 60_000.0),
    }
}

/// Member ids as words.
fn ids(ids: &[u64]) -> String {
    if ids.is_empty() {
        return "none".to_owned();
    }
    let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
    ids.join(", ")
}

/// The farm-wide answer: the under-replicated number, or that it is unknown and why.
fn replication(h: &mut Html, doc: &StorageDoc) {
    h.el("section", &[attr("class", "card")], |h| {
        h.el("h2", &[], |h| {
            h.text("Replication");
        });
        let (words, class) = match doc.under_replicated_blobs {
            Some(0) => ("every blob has its copies".to_owned(), "ok"),
            Some(n) => (format!("{n} blobs owe a copy"), "fail"),
            None => (
                "under-replicated blobs unknown: not every member is active and fresh".to_owned(),
                "pending",
            ),
        };
        h.el("p", &[], |h| pill(h, &words, class));
        let fields: [(&str, String); 6] = [
            ("Under-replicated blobs", count(doc.under_replicated_blobs)),
            (
                "Copies owed by reporting active members",
                doc.debts_total.to_string(),
            ),
            (
                "Active members",
                format!("{} of {}", doc.members_active, doc.members_total),
            ),
            ("Not active", ids(&doc.not_active)),
            ("Silent (stale report)", ids(&doc.silent)),
            ("Cannot say (no count)", ids(&doc.unreported)),
        ];
        definitions(h, &fields);
        if !doc.reasons.is_empty() {
            h.el("p", &[], |h| {
                h.text("Why it is not zero:");
            });
            h.el("ul", &[], |h| {
                for reason in &doc.reasons {
                    h.el("li", &[], |h| {
                        h.text(reason);
                    });
                }
            });
        }
    });
}

/// How fresh a member's report is, in words, from the lists the API gives.
fn report(doc: &StorageDoc, node: &StorageNode) -> &'static str {
    if doc.silent.contains(&node.id) {
        "stale"
    } else if doc.unreported.contains(&node.id) {
        "cannot say"
    } else if doc.not_active.contains(&node.id) {
        "not judged"
    } else {
        "fresh"
    }
}

fn cell(h: &mut Html, text: &str) {
    h.el("td", &[], |h| {
        h.text(text);
    });
}

fn head(h: &mut Html, columns: &[&str]) {
    h.el("thead", &[], |h| {
        h.el("tr", &[], |h| {
            for name in columns {
                h.el("th", &[attr("scope", "col")], |h| {
                    h.text(name);
                });
            }
        });
    });
}

/// Each member: its state, its report's freshness and its repair work.
fn members(h: &mut Html, doc: &StorageDoc) {
    h.el("h2", &[], |h| {
        h.text("Members");
    });
    if doc.nodes.is_empty() {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text("No storage member is registered.");
        });
        return;
    }
    h.el("table", &[attr("class", "rows")], |h| {
        head(
            h,
            &[
                "Member",
                "State",
                "Report",
                "Repair debts",
                "Open writes",
                "Map generation",
            ],
        );
        h.el("tbody", &[], |h| {
            for node in &doc.nodes {
                h.el("tr", &[], |h| {
                    h.el("th", &[attr("scope", "row")], |h| {
                        h.text(&node.id.to_string());
                    });
                    let class = match node.state.as_str() {
                        "active" => "ok",
                        "down" => "fail",
                        "joining" | "catching_up" | "draining" => "pending",
                        _ => "other",
                    };
                    let state = if node.planned_down {
                        format!("{} (planned)", node.state)
                    } else {
                        node.state.clone()
                    };
                    h.el("td", &[], |h| pill(h, &state, class));
                    let fresh = report(doc, node);
                    let class = if fresh == "fresh" { "ok" } else { "pending" };
                    h.el("td", &[], |h| pill(h, fresh, class));
                    cell(h, &count(node.repair_debts));
                    cell(h, &count(node.open_writes));
                    cell(h, &count(node.generation));
                });
            }
        });
    });
}

/// Each member's write-tier health from its heartbeat: acknowledged data not yet in the
/// cold store, the oldest such block's age, the tier's budget and use, and flush
/// trouble.
fn write_tier(h: &mut Html, doc: &StorageDoc) {
    h.el("h2", &[], |h| {
        h.text("Write tier");
    });
    h.el("p", &[attr("class", "hint")], |h| {
        h.text(
            "Unflushed data is acknowledged but on the node's local disk only: what losing \
             that disk would take. unknown means the report is stale, the node has not \
             reported since it registered, it has no write tier or it is an older build; \
             it is never zero.",
        );
    });
    if doc.nodes.is_empty() {
        return;
    }
    h.el("table", &[attr("class", "rows")], |h| {
        head(
            h,
            &[
                "Member",
                "Unflushed bytes",
                "Unflushed blocks",
                "Oldest unflushed",
                "Budget",
                "Used",
                "Flush failures",
                "Flush conflicts",
            ],
        );
        h.el("tbody", &[], |h| {
            for node in &doc.nodes {
                h.el("tr", &[], |h| {
                    h.el("th", &[attr("scope", "row")], |h| {
                        h.text(&node.id.to_string());
                    });
                    cell(h, &size(node.unflushed_bytes));
                    cell(h, &count(node.unflushed_blocks));
                    cell(h, &age(node.oldest_unflushed_ms));
                    cell(h, &size(node.budget_bytes));
                    cell(h, &size(node.used_bytes));
                    cell(h, &count(node.flush_failures));
                    cell(h, &count(node.flush_conflicts));
                });
            }
        });
    });
}

#[cfg(test)]
mod tests;
