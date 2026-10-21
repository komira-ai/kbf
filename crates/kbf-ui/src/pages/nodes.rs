//! The Nodes page (every node the server has seen, with its state, leases and what they
//! book) and the Node page (one node: capacity, booked, free and its leases).
//!
//! Both read only the nodes routes: `GET /v1/nodes` and `GET /v1/nodes/{id}`. Counts and
//! sums are the API's (`lease_count`, `booked`, `free`): a page adds up nothing. The list is
//! in memory on the server, so both pages say that a restart empties it until the daemons
//! reconnect.

use serde::Deserialize;

use super::{bytes, failed, pill, refresh, segment};
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::{Page, Reader};
use crate::{Answer, PREFIX, Source};

/// The Nodes page's path.
pub(crate) const NODES_PATH: &str = "/ui/nodes";

/// Nodes: the list.
pub(crate) const NODES: Page = Page {
    name: "Nodes",
    reads: &["/v1/nodes"],
};

/// Node: one node.
pub(crate) const NODE: Page = Page {
    name: "Node",
    reads: &["/v1/nodes/{id}"],
};

/// What a restart does to what these pages show.
const IN_MEMORY: &str = "The farm keeps its nodes in memory. After a server restart or a \
     frontend swap this holds only the nodes that have reconnected since, so an empty \
     list is not by itself an outage.";

/// `GET /v1/nodes`: the keys the page shows.
#[derive(Debug, Deserialize)]
struct NodesDoc {
    nodes: Vec<NodeRow>,
}

/// A node, as the list and the detail both give it.
#[derive(Debug, Deserialize)]
struct NodeRow {
    node_id: String,
    connected: bool,
    protocol: Option<u64>,
    software: Option<Software>,
    needs_attention: Vec<String>,
    placement: Tagged,
    phase: Tagged,
    update: Update,
    lease_count: u64,
    booked: Resources,
}

/// `GET /v1/nodes/{id}`: the entry and three more keys.
#[derive(Debug, Deserialize)]
struct NodeDoc {
    #[serde(flatten)]
    node: NodeRow,
    capacity: Resources,
    free: Resources,
    leases: Vec<String>,
}

/// What the node says it runs.
#[derive(Debug, Deserialize)]
struct Software {
    os_name: String,
    os_version: String,
    daemon_version: String,
}

/// A state the server tags: `{"state": "serving", ..}`. A state the page does not
/// know is shown as it is written.
#[derive(Debug, Deserialize)]
struct Tagged {
    state: String,
    /// A quarantine's why.
    reason: Option<String>,
}

/// Where the node's update is.
#[derive(Debug, Deserialize)]
struct Update {
    state: Option<String>,
    up_to_date: Option<bool>,
    availability: String,
}

/// A resource vector.
#[derive(Debug, Deserialize)]
struct Resources {
    cpu_millis: u64,
    memory_bytes: u64,
    gpus: u64,
    vms: u64,
}

/// The Nodes page; `?refresh=N` reloads it.
pub(crate) async fn nodes<S: Source>(source: &S, query: &str) -> Answer {
    let refresh = match refresh::parse(query) {
        Ok(r) => r,
        Err(why) => return refresh::refused(&NODES, NODES_PATH, &why),
    };
    let mut r = Reader::new(source, &NODES);
    let doc = match r.json::<NodesDoc>("/v1/nodes").await {
        Ok(doc) => doc,
        Err(e) => return failed(&NODES, NODES_PATH, r.asked(), &e),
    };
    let body = layout::page_refreshing(NODES.name, NODES_PATH, r.asked(), refresh, |h| {
        h.el("h1", &[], |h| {
            h.text("Nodes");
        });
        refresh::controls(h, NODES_PATH, refresh);
        h.el("p", &[attr("class", "hint")], |h| {
            h.text(IN_MEMORY);
        });
        if doc.nodes.is_empty() {
            h.el("p", &[], |h| {
                h.text("No node has registered with this server.");
            });
            return;
        }
        table(h, &doc.nodes);
    });
    Answer::html(200, body)
}

fn table(h: &mut Html, nodes: &[NodeRow]) {
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for name in [
                    "Node",
                    "Connection",
                    "Placement",
                    "Phase",
                    "Software",
                    "Update",
                    "Leases",
                    "Booked",
                    "Needs attention",
                ] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for node in nodes {
                row(h, node);
            }
        });
    });
}

fn row(h: &mut Html, node: &NodeRow) {
    let href = Href::root_relative(&node_page(&node.node_id))
        .expect("a node page's path is root-relative");
    h.el("tr", &[], |h| {
        h.el("th", &[attr("scope", "row")], |h| {
            h.link(&href, &node.node_id);
        });
        h.el("td", &[], |h| connection(h, node.connected));
        h.el("td", &[], |h| placement(h, &node.placement));
        h.el("td", &[], |h| phase(h, &node.phase));
        h.el("td", &[], |h| {
            h.text(&software(node.software.as_ref()));
        });
        h.el("td", &[], |h| {
            h.text(&update(&node.update));
        });
        h.el("td", &[], |h| {
            h.text(&node.lease_count.to_string());
        });
        h.el("td", &[], |h| {
            h.text(&resources(&node.booked));
        });
        h.el("td", &[], |h| attention(h, &node.needs_attention));
    });
}

/// The Node page of node `id`; `?refresh=N` reloads it.
pub(crate) async fn node<S: Source>(source: &S, id: &str, query: &str) -> Answer {
    let here = node_page(id);
    let refresh = match refresh::parse(query) {
        Ok(r) => r,
        Err(why) => return refresh::refused(&NODE, &here, &why),
    };
    let mut r = Reader::new(source, &NODE);
    let doc = match r
        .json::<NodeDoc>(&format!("/v1/nodes/{}", segment(id)))
        .await
    {
        Ok(doc) => doc,
        Err(e) => return failed(&NODE, &here, r.asked(), &e),
    };
    let body = layout::page_refreshing(NODE.name, &here, r.asked(), refresh, |h| {
        let all = Href::root_relative(NODES_PATH).expect("the Nodes path is root-relative");
        h.el("p", &[attr("class", "crumbs")], |h| {
            h.link(&all, "Nodes");
        });
        h.el("h1", &[], |h| {
            h.text("Node ").el("code", &[], |h| {
                h.text(&doc.node.node_id);
            });
        });
        refresh::controls(h, &here, refresh);
        h.el("p", &[attr("class", "hint")], |h| {
            h.text(IN_MEMORY);
        });
        summary(h, &doc.node);
        h.el("h2", &[], |h| {
            h.text("Capacity");
        });
        capacity(h, &doc);
        h.el("h2", &[], |h| {
            h.text("Leases");
        });
        leases(h, &doc.leases);
        h.el("h2", &[], |h| {
            h.text("Needs attention");
        });
        attention(h, &doc.node.needs_attention);
    });
    Answer::html(200, body)
}

fn summary(h: &mut Html, node: &NodeRow) {
    let protocol = node
        .protocol
        .map_or_else(|| "unknown".to_owned(), |p| p.to_string());
    h.el("dl", &[attr("class", "fields")], |h| {
        field(h, "Connection", |h| connection(h, node.connected));
        field(h, "Placement", |h| placement(h, &node.placement));
        field(h, "Phase", |h| phase(h, &node.phase));
        field(h, "Protocol", |h| {
            h.text(&protocol);
        });
        field(h, "Software", |h| {
            h.text(&software(node.software.as_ref()));
        });
        field(h, "Update", |h| {
            h.text(&update(&node.update));
        });
        field(h, "Leases held", |h| {
            h.text(&node.lease_count.to_string());
        });
    });
}

/// One term of a definition list.
fn field(h: &mut Html, name: &str, value: impl FnOnce(&mut Html)) {
    h.el("dt", &[], |h| {
        h.text(name);
    });
    h.el("dd", &[], value);
}

/// Capacity, booked and free as a table, one row each.
fn capacity(h: &mut Html, doc: &NodeDoc) {
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for name in ["", "CPU (millicores)", "Memory", "GPUs", "VMs"] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for (name, r) in [
                ("Capacity", &doc.capacity),
                ("Booked", &doc.node.booked),
                ("Free", &doc.free),
            ] {
                h.el("tr", &[], |h| {
                    h.el("th", &[attr("scope", "row")], |h| {
                        h.text(name);
                    });
                    for cell in [
                        r.cpu_millis.to_string(),
                        bytes(r.memory_bytes),
                        r.gpus.to_string(),
                        r.vms.to_string(),
                    ] {
                        h.el("td", &[], |h| {
                            h.text(&cell);
                        });
                    }
                });
            }
        });
    });
}

fn leases(h: &mut Html, leases: &[String]) {
    if leases.is_empty() {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text("This node holds no lease.");
        });
        return;
    }
    h.el("ul", &[], |h| {
        for lease in leases {
            h.el("li", &[], |h| {
                h.el("code", &[], |h| {
                    h.text(lease);
                });
            });
        }
    });
}

/// What an operator must do on the node, one line each, or that nothing is needed.
fn attention(h: &mut Html, items: &[String]) {
    if items.is_empty() {
        h.text("none");
        return;
    }
    h.el("ul", &[], |h| {
        for item in items {
            h.el("li", &[], |h| {
                h.text(item);
            });
        }
    });
}

/// The node's stream, in words and a pill.
fn connection(h: &mut Html, connected: bool) {
    if connected {
        pill(h, "connected", "ok");
    } else {
        pill(h, "disconnected", "fail");
    }
}

fn placement(h: &mut Html, p: &Tagged) {
    let class = match p.state.as_str() {
        "serving" => "ok",
        "drain_paused" => "fail",
        _ => "pending",
    };
    pill(h, &p.state, class);
}

fn phase(h: &mut Html, p: &Tagged) {
    let class = match p.state.as_str() {
        "serving" => "ok",
        "quarantined" => "fail",
        _ => "pending",
    };
    let words = match &p.reason {
        Some(reason) => format!("{}: {reason}", p.state),
        None => p.state.clone(),
    };
    pill(h, &words, class);
}

/// The OS and daemon the node reported, or that it reported none.
fn software(s: Option<&Software>) -> String {
    s.map_or_else(
        || "not reported".to_owned(),
        |s| {
            format!(
                "{} {}, daemon {}",
                s.os_name, s.os_version, s.daemon_version
            )
        },
    )
}

/// Where the node's update is, in words; a verdict the server cannot give is "not judged".
fn update(u: &Update) -> String {
    let verdict = match u.up_to_date {
        Some(true) => "up to date",
        Some(false) => "behind",
        None => "not judged",
    };
    match &u.state {
        Some(state) => format!("{verdict}, step {state}"),
        None => format!("{verdict} ({})", u.availability),
    }
}

/// A resource vector in words.
fn resources(r: &Resources) -> String {
    format!(
        "{} mCPU, {}, {} GPU, {} VM",
        r.cpu_millis,
        bytes(r.memory_bytes),
        r.gpus,
        r.vms
    )
}

/// `/ui/nodes/{id}`.
pub(crate) fn node_page(id: &str) -> String {
    format!("{PREFIX}/nodes/{}", segment(id))
}

#[cfg(test)]
mod tests;
