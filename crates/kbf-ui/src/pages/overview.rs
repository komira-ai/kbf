//! The Overview page: is the farm healthy, on one screen.
//!
//! It reads `GET /v1/health` (readiness and the failing checks, nodes by state, the queue's
//! depth, the leases held, the storage summary) and `GET /v1/invocations` (the latest
//! builds), and adds nothing of its own: every number is one the API sent. A value the API
//! does not know is "unknown" or "not measured", never zero. Update state is not in the
//! health answer; the page links the Updates page for it.

use serde::Deserialize;

use super::{BUILDS_PATH, UPDATES_PATH, build_page, definitions, pill, refresh};
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::{Page, Reader};
use crate::{Answer, PREFIX, Source};

/// The latest builds the page asks for.
const BUILDS_URL: &str = "/v1/invocations?page_size=10";

/// Overview: the health summary and the latest builds.
pub(crate) const OVERVIEW: Page = Page {
    name: "Overview",
    reads: &["/v1/health", "/v1/invocations"],
};

/// `GET /v1/health`.
#[derive(Debug, Deserialize)]
struct Health {
    ready: bool,
    failing: Vec<Failing>,
    nodes: NodeCounts,
    queue: Queue,
    leases: u64,
    storage: Option<Storage>,
    /// Why `storage` is `null`, when the membership could not be read.
    #[serde(default)]
    storage_unavailable: Option<String>,
}

/// A readiness check that fails.
#[derive(Debug, Deserialize)]
struct Failing {
    check: String,
    reason: String,
}

/// The nodes, each counted once in the first state that holds.
#[derive(Debug, Deserialize)]
struct NodeCounts {
    total: u64,
    serving: u64,
    cordoned: u64,
    draining: u64,
    drained: u64,
    quarantined: u64,
    disconnected: u64,
}

/// The scheduler's queue.
#[derive(Debug, Deserialize)]
struct Queue {
    depth: u64,
    oldest_age_ms: Option<u64>,
}

/// The storage summary.
#[derive(Debug, Deserialize)]
struct Storage {
    members_active: u64,
    members_total: u64,
    under_replicated_blobs: Option<u64>,
}

/// A page of `GET /v1/invocations`.
#[derive(Debug, Deserialize)]
struct Builds {
    invocations: Vec<Listed>,
    /// Present when the next page exists.
    #[serde(default)]
    next_page: Option<String>,
    /// Some instance could not be read.
    #[serde(default)]
    partial: bool,
}

/// One build of the list.
#[derive(Debug, Deserialize)]
struct Listed {
    id: String,
    first_seen: String,
    last_seen: String,
    state: String,
    complete: bool,
    counts: ListedCounts,
    instances: Vec<String>,
}

/// The counters a list row carries.
#[derive(Debug, Deserialize)]
struct ListedCounts {
    ran: u64,
    hits: u64,
    failed: u64,
}

/// The Overview page; `?refresh=N` reloads it.
pub(crate) async fn overview<S: Source>(source: &S, query: &str) -> Answer {
    let refresh = match refresh::parse(query) {
        Ok(r) => r,
        Err(why) => return refresh::refused(&OVERVIEW, PREFIX, &why),
    };
    let mut r = Reader::new(source, &OVERVIEW);
    let health = r.json::<Health>("/v1/health").await;
    let builds = r.json::<Builds>(BUILDS_URL).await;
    // The page answers with the health's own status when that read failed, and does not
    // reload itself then.
    let (status, passed_on, refresh) = match &health {
        Err(e) => (e.status(), e.headers(), None),
        Ok(_) => (200, &[][..], refresh),
    };
    let body = layout::page_refreshing(OVERVIEW.name, PREFIX, r.asked(), refresh, |h| {
        h.el("h1", &[], |h| {
            h.text("Overview");
        });
        refresh::controls(h, PREFIX, refresh);
        match &health {
            Ok(health) => farm(h, health),
            Err(e) => super::problem(h, e),
        }
        latest_builds(h, &builds);
    });
    Answer::html(status, body).passing_on(passed_on)
}

fn farm(h: &mut Html, health: &Health) {
    h.el("h2", &[], |h| {
        h.text("Readiness");
    });
    h.el("p", &[], |h| {
        if health.ready {
            pill(h, "ready", "ok");
        } else {
            pill(h, "not ready", "fail");
        }
    });
    failing(h, &health.failing);

    h.el("h2", &[], |h| {
        h.text("Nodes");
    });
    node_counts(h, &health.nodes);
    let nodes = Href::root_relative("/ui/nodes").expect("the Nodes path is root-relative");
    h.el("p", &[], |h| {
        h.link(&nodes, "All nodes");
    });
    h.el("p", &[attr("class", "hint")], |h| {
        h.text(
            "Nodes, the queue and the leases are held in the server's memory: after a \
             restart or a frontend swap they count only what has reconnected since.",
        );
    });

    h.el("h2", &[], |h| {
        h.text("Queue, leases and storage");
    });
    let oldest = health
        .queue
        .oldest_age_ms
        .map_or_else(|| "not measured".to_owned(), |ms| format!("{ms} ms"));
    let fields: [(&str, String); 5] = [
        ("Queue depth", health.queue.depth.to_string()),
        ("Oldest queued operation", oldest),
        ("Leases held", health.leases.to_string()),
        ("Storage members", members(health)),
        ("Under-replicated blobs", under_replicated(health)),
    ];
    definitions(h, &fields);
    let storage = Href::root_relative("/ui/storage").expect("the Storage path is root-relative");
    h.el("p", &[], |h| {
        h.link(&storage, "Storage");
    });
    let updates = Href::root_relative(UPDATES_PATH).expect("the Updates path is root-relative");
    h.el("p", &[attr("class", "hint")], |h| {
        h.text("Update state is not part of this summary: see ")
            .link(&updates, "Updates")
            .text(".");
    });
}

/// The failing checks as a table, or that none fails.
fn failing(h: &mut Html, failing: &[Failing]) {
    if failing.is_empty() {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text("No check fails.");
        });
        return;
    }
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for name in ["Failing check", "Reason"] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for f in failing {
                h.el("tr", &[], |h| {
                    h.el("th", &[attr("scope", "row")], |h| {
                        h.text(&f.check);
                    });
                    h.el("td", &[], |h| {
                        h.text(&f.reason);
                    });
                });
            }
        });
    });
}

fn node_counts(h: &mut Html, n: &NodeCounts) {
    let cells: [(&str, u64); 7] = [
        ("nodes", n.total),
        ("serving", n.serving),
        ("cordoned", n.cordoned),
        ("draining", n.draining),
        ("drained", n.drained),
        ("quarantined", n.quarantined),
        ("disconnected", n.disconnected),
    ];
    h.el("ul", &[attr("class", "counts")], |h| {
        for (label, value) in cells {
            h.el("li", &[], |h| {
                h.el("span", &[attr("class", "n")], |h| {
                    h.text(&value.to_string());
                })
                .el("span", &[attr("class", "label")], |h| {
                    h.text(label);
                });
            });
        }
    });
}

/// How many storage members are active, or why that is not known.
fn members(health: &Health) -> String {
    match (&health.storage, &health.storage_unavailable) {
        (Some(s), _) => format!("{} of {} active", s.members_active, s.members_total),
        (None, Some(why)) => format!("unknown ({why})"),
        (None, None) => "none: this server keeps no storage members".to_owned(),
    }
}

/// The under-replicated blob count: absent is "unknown", never zero.
fn under_replicated(health: &Health) -> String {
    health
        .storage
        .as_ref()
        .and_then(|s| s.under_replicated_blobs)
        .map_or_else(|| "unknown".to_owned(), |n| n.to_string())
}

fn latest_builds(h: &mut Html, builds: &Result<Builds, crate::read::ReadError>) {
    h.el("h2", &[], |h| {
        h.text("Latest builds");
    });
    let all = Href::root_relative(BUILDS_PATH).expect("the Builds path is root-relative");
    match builds {
        Err(e) => {
            h.el("p", &[attr("class", "hint")], |h| {
                h.text("The list of builds is not available (")
                    .text(&e.to_string())
                    .text("). Look a build up by its id on the ")
                    .link(&all, "Builds page")
                    .text(".");
            });
        }
        Ok(builds) if builds.invocations.is_empty() => {
            h.el("p", &[attr("class", "hint")], |h| {
                h.text("The farm has recorded no build. ")
                    .link(&all, "Look a build up by id")
                    .text(".");
            });
        }
        Ok(builds) => {
            if builds.partial {
                h.el("p", &[attr("class", "hint")], |h| {
                    h.text("Partial: some instance could not be read, its builds may be missing.");
                });
            }
            build_table(h, &builds.invocations);
            h.el("p", &[], |h| {
                h.link(&all, "All builds");
                if builds.next_page.is_some() {
                    h.text(" (older builds exist)");
                }
            });
        }
    }
}

fn build_table(h: &mut Html, builds: &[Listed]) {
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for name in [
                    "Build",
                    "State",
                    "First seen",
                    "Last seen",
                    "Ran",
                    "Cache hits",
                    "Failed",
                    "Instances",
                ] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for b in builds {
                let href = Href::root_relative(&build_page(&b.id))
                    .expect("a build page's path is root-relative");
                let state = format!("{}{}", b.state, if b.complete { ", complete" } else { "" });
                h.el("tr", &[], |h| {
                    h.el("th", &[attr("scope", "row")], |h| {
                        h.link(&href, &b.id);
                    });
                    for cell in [
                        &state,
                        &b.first_seen,
                        &b.last_seen,
                        &b.counts.ran.to_string(),
                        &b.counts.hits.to_string(),
                        &b.counts.failed.to_string(),
                        &b.instances.join(", "),
                    ] {
                        h.el("td", &[], |h| {
                            h.text(cell);
                        });
                    }
                });
            }
        });
    });
}

#[cfg(test)]
mod tests;
