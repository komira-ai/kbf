//! The manifest holds none of the job scheduler's server crates, in the layering of
//! `tests/layering.rs`.
//!
//! kbf is a client of the job service: scheduling runs in the service, so no crate of this
//! workspace links the scheduler's server (`js-server-core`) or its store (`js-store`), and the
//! lease client that talked to the service's lease server (`kbf-lease-client`) is gone. Unlike
//! the walks of `tests/layering.rs`, which follow normal edges, this one follows every edge
//! `cargo metadata` resolves (normal, dev and build): a dev-dependency on a server crate links
//! it into the crate's tests all the same.

use std::collections::{BTreeMap, VecDeque};

use serde_json::Value;

use super::workspace_metadata;

/// The crates of the jobscheduler repository that run the scheduler: no workspace crate reaches
/// them.
const SCHEDULER_SERVER_CRATES: [&str; 2] = ["js-server-core", "js-store"];

/// The name of the deleted lease client: no package of the graph carries it.
const REMOVED_PACKAGE: &str = "kbf-lease-client";

/// A fixture in which `kbf-alpha` reaches `js-store` through `kbf-beta`, by a dev-dependency edge
/// and then a normal one, and a package is named `kbf-lease-client`.
const FORBIDDEN: &str = include_str!("forbidden_server_crates.json");

/// A fixture in which the same two members reach `js-sched` alone, by normal, dev and build edges.
const WRAPPER_CRATES_ONLY: &str = include_str!("wrapper_crates_only.json");

/// The resolved graph of `cargo metadata`, over every kind of edge.
struct EdgeGraph {
    /// Package id to package name.
    names: BTreeMap<String, String>,
    /// The ids of the workspace members, ordered by name.
    members: Vec<String>,
    /// Package id to the ids of its dependencies, of any kind.
    edges: BTreeMap<String, Vec<String>>,
}

fn edge_graph(meta: &Value) -> EdgeGraph {
    let names: BTreeMap<String, String> = meta["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|pkg| {
            (
                pkg["id"].as_str().expect("package id").to_owned(),
                pkg["name"].as_str().expect("package name").to_owned(),
            )
        })
        .collect();
    let mut members: Vec<String> = meta["workspace_members"]
        .as_array()
        .expect("workspace_members")
        .iter()
        .map(|id| id.as_str().expect("member id").to_owned())
        .collect();
    members.sort_by_key(|id| names[id].clone());
    let edges: BTreeMap<String, Vec<String>> = meta["resolve"]["nodes"]
        .as_array()
        .expect("resolve.nodes")
        .iter()
        .map(|node| {
            let deps = node["deps"]
                .as_array()
                .expect("deps")
                .iter()
                .map(|dep| dep["pkg"].as_str().expect("dep pkg").to_owned())
                .collect();
            (node["id"].as_str().expect("node id").to_owned(), deps)
        })
        .collect();
    EdgeGraph {
        names,
        members,
        edges,
    }
}

/// Every package reachable from `root` through any edge, with the shortest chain of names that
/// reaches it.
fn closure(g: &EdgeGraph, root: &str) -> BTreeMap<String, String> {
    let mut seen = BTreeMap::new();
    let mut queue = VecDeque::from([(root.to_owned(), g.names[root].clone())]);
    while let Some((id, chain)) = queue.pop_front() {
        for dep in g.edges.get(&id).into_iter().flatten() {
            if seen.contains_key(dep) {
                continue;
            }
            let dep_chain = format!("{chain} -> {}", g.names[dep]);
            seen.insert(dep.clone(), dep_chain.clone());
            queue.push_back((dep.clone(), dep_chain));
        }
    }
    seen
}

/// What the graph of `meta` holds that it may not, one message each, sorted: a chain from a
/// workspace member to a scheduler server crate, and a package named `kbf-lease-client`.
fn violations(meta: &Value) -> Vec<String> {
    let g = edge_graph(meta);
    let mut found = Vec::new();
    for member in &g.members {
        for (dep, chain) in closure(&g, member) {
            let name = g.names[&dep].as_str();
            if SCHEDULER_SERVER_CRATES.contains(&name) {
                found.push(format!(
                    "{chain}: {name} is a crate of the job scheduler's server, which no workspace crate may link"
                ));
            }
        }
    }
    for name in g.names.values().filter(|name| *name == REMOVED_PACKAGE) {
        found.push(format!(
            "{name}: the crate was deleted, and no package of the graph may carry its name"
        ));
    }
    found.sort();
    found
}

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).expect("the fixture is JSON")
}

// The wrapper crates `kbf-sched`, `kbf-caps`, `kbf-estimator`, `kbf-sim`, `kbf-state` and
// `kbf-usage`, and `kbf-daemon` (through `js-agent-usage`), keep their `js-*` dependencies:
// the legacy farm core uses them until it is removed, after the cutover to the job service. So
// they are not checked here: only the scheduler's server crates and the lease client are.
/// Asserts that no workspace member reaches `js-server-core` or `js-store` through a normal, dev
/// or build edge, and that no package is named `kbf-lease-client`. The walk is shown to see the
/// job service client's own dependency, `js-proto`, so the check does not pass on an empty walk.
#[test]
fn the_manifest_has_no_scheduler_server_crates() {
    let meta = workspace_metadata();
    let g = edge_graph(&meta);
    let client = g
        .members
        .iter()
        .find(|id| g.names[*id] == "kbf-job-client")
        .expect("kbf-job-client is a workspace member");
    assert!(
        closure(&g, client)
            .values()
            .any(|chain| chain.ends_with(" -> js-proto")),
        "the walk from kbf-job-client does not reach js-proto, so it proves nothing"
    );
    let found = violations(&meta);
    assert!(
        found.is_empty(),
        "the manifest holds scheduler server crates:\n{}",
        found.join("\n")
    );
}

/// Asserts that the check names the offending member, the whole path through the dev edge to the
/// server crate, and the package that carries the deleted lease client's name, each by exact
/// message; and that it passes a graph in which members reach `js-sched` alone, by normal, dev
/// and build edges. Catches a walk that follows normal edges only, or reports a crate without
/// the path that reaches it.
#[test]
fn the_layering_test_names_the_offender_and_the_path() {
    let js_store =
        "js-store is a crate of the job scheduler's server, which no workspace crate may link";
    assert_eq!(
        violations(&fixture(FORBIDDEN)),
        [
            format!("kbf-alpha -> kbf-beta -> js-store: {js_store}"),
            format!("kbf-beta -> js-store: {js_store}"),
            "kbf-lease-client: the crate was deleted, and no package of the graph may carry its name"
                .to_owned(),
        ]
    );
    assert_eq!(
        violations(&fixture(WRAPPER_CRATES_ONLY)),
        Vec::<String>::new()
    );
}
