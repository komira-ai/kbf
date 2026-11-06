//! The job service client (`kbf-job-client`) in the layering of `tests/layering.rs`: a leaf
//! over the jobscheduler's `js-proto`, and no farm crate uses it yet: only the integration
//! tests do, as a dev-dependency (`tests/js_real_service.rs` drives it against the real
//! service). The design makes kbf a client of the job service that links none of the
//! scheduler's server crates; this is where that starts.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use serde_json::Value;

use super::{Graph, load_graph, normal_closure};

/// The client's package name.
const CLIENT: &str = "kbf-job-client";

/// The `js-*` crates the client may reach: the generated messages and service. What
/// `js-proto` needs is not a `js-*` crate.
const JS_ALLOWED: [&str; 1] = ["js-proto"];

/// Crates of crates.io whose names begin `js-` but that are not the scheduler's.
const NOT_OURS: [&str; 1] = ["js-sys"];

/// Whether `name` is a crate of the jobscheduler repository.
fn scheduler_crate(name: &str) -> bool {
    name.starts_with("js-") && !NOT_OURS.contains(&name)
}

/// The scheduler crates among `reached` that the client may not reach.
fn forbidden_js<'a>(reached: impl IntoIterator<Item = &'a str>) -> BTreeSet<&'a str> {
    reached
        .into_iter()
        .filter(|name| scheduler_crate(name) && !JS_ALLOWED.contains(name))
        .collect()
}

/// The chains, from `root_id`, to every crate its normal closure reaches that it may not: a
/// scheduler crate other than `js-proto`, or any kbf crate. Sorted.
fn violations(g: &Graph, root_id: &str) -> Vec<String> {
    let mut found: Vec<String> = normal_closure(g, root_id)
        .into_iter()
        .filter(|(dep, _)| {
            let name = g.names[dep].as_str();
            name.starts_with("kbf-") || !forbidden_js([name]).is_empty()
        })
        .map(|(_, chain)| chain)
        .collect();
    found.sort();
    found
}

/// Each dependency on the client among `packages` (of `cargo metadata --no-deps`), as
/// `<package> (<kind>)`, except the client's own dev-dependency on itself, which its tests
/// use for the fake.
fn dependents(packages: &[Value]) -> Vec<String> {
    let mut found = Vec::new();
    for package in packages {
        let name = package["name"].as_str().expect("package name");
        for dep in package["dependencies"].as_array().expect("dependencies") {
            if dep["name"] != CLIENT {
                continue;
            }
            let kind = dep["kind"].as_str().unwrap_or("normal");
            if name != CLIENT || kind != "dev" {
                found.push(format!("{name} ({kind})"));
            }
        }
    }
    found
}

/// Asserts that no crate of the workspace depends on the job client yet, in any kind of
/// dependency, but the integration tests, as a dev-dependency (the real-service tests); and
/// that the client's own manifest names itself once, as the dev-dependency of its tests (so
/// the walk reads the dependencies it checks). The host executor moving to
/// the client is the change that edits this test.
#[test]
fn only_the_integration_tests_depend_on_the_job_client_yet() {
    let cargo = std::env::var("CARGO").expect("CARGO is set when cargo runs a test");
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(out.status.success(), "cargo metadata failed");
    let meta: Value = serde_json::from_slice(&out.stdout).expect("cargo metadata emits JSON");
    let packages = meta["packages"].as_array().expect("packages");
    let client = packages
        .iter()
        .find(|p| p["name"] == CLIENT)
        .expect("kbf-job-client is a workspace member");
    let itself = client["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .filter(|d| d["name"] == CLIENT && d["kind"] == "dev")
        .count();
    assert_eq!(itself, 1, "the client's tests take it with the fake");
    let found = dependents(packages);
    assert_eq!(
        found,
        ["kbf-integration-test (dev)"],
        "a crate depends on the job client before the executor uses it"
    );
}

/// Asserts that a dependency on the client is named whatever its kind, and the client's own
/// dev-dependency is not, while a normal dependency of the client on itself would be.
#[test]
fn a_crate_that_depends_on_the_job_client_is_named() {
    let packages: Vec<Value> = serde_json::from_str(
        r#"[
            {"name": "kbf-job-client", "dependencies": [
                {"name": "kbf-job-client", "kind": "dev"}, {"name": "tonic", "kind": null}]},
            {"name": "kbf-server", "dependencies": [{"name": "kbf-job-client", "kind": null}]},
            {"name": "kbf-front", "dependencies": [{"name": "kbf-job-client", "kind": "dev"}]},
            {"name": "kbf-node", "dependencies": [{"name": "tonic", "kind": null}]}
        ]"#,
    )
    .unwrap();
    assert_eq!(
        dependents(&packages),
        ["kbf-server (normal)", "kbf-front (dev)"]
    );
    let only_itself = &packages[..1];
    assert!(dependents(only_itself).is_empty());
}

/// Asserts that the job client's normal tree reaches `js-proto` and no other scheduler
/// crate (no `js-server-core`, `js-sched`, `js-state`, `js-store`, `js-usage`, ...), and no
/// kbf crate; that the walk sees `js-proto` and `tonic`, or it would prove nothing; and that
/// `js-proto` comes from the jobscheduler repository by the same source and revision as every
/// other scheduler crate of the graph.
#[test]
fn the_job_client_reaches_only_js_proto() {
    let g = load_graph();
    let (id, root) = &g.members[CLIENT];
    assert!(root.is_some(), "kbf-job-client must have a library target");
    let closure = normal_closure(&g, id);
    let reached: Vec<&str> = closure.keys().map(|dep| g.names[dep].as_str()).collect();
    assert!(
        reached.contains(&"js-proto") && reached.contains(&"tonic"),
        "the walk must see js-proto and tonic, or it proves nothing: {reached:?}"
    );
    let found = violations(&g, id);
    assert!(found.is_empty(), "the job client reaches {found:?}");

    let sources: BTreeSet<Option<&str>> = g
        .names
        .iter()
        .filter(|(_, name)| scheduler_crate(name))
        .map(|(id, _)| g.sources[id].as_deref())
        .collect();
    assert_eq!(
        sources.len(),
        1,
        "the scheduler crates come from {sources:?}"
    );
    let proto = closure
        .keys()
        .find(|dep| g.names[*dep] == "js-proto")
        .map(|dep| g.sources[dep].as_deref());
    assert_eq!(proto, sources.first().copied());
    let source = proto.flatten().unwrap_or_default();
    assert!(
        source.starts_with("git+https://github.com/komira-ai/jobscheduler?rev="),
        "js-proto comes from {source}"
    );
}

/// Asserts that the rule names a scheduler crate other than `js-proto` and passes
/// `js-proto`, crates outside the scheduler, and `js-sys` of crates.io.
#[test]
fn a_job_client_that_reaches_a_server_crate_is_named() {
    let reached = ["js-proto", "tonic", "js-sched", "js-server-core", "js-sys"];
    assert_eq!(
        forbidden_js(reached),
        BTreeSet::from(["js-sched", "js-server-core"])
    );
    assert!(forbidden_js(["js-proto", "tonic", "prost"]).is_empty());
}

/// Asserts that the evaluator names the chain to every forbidden crate of a graph in which the
/// client reaches a kbf crate through `tonic` and `js-sched` directly, and nothing for the
/// allowed `js-proto` and `prost`. Catches an evaluator that stops at the first hit or skips
/// kbf crates.
#[test]
fn a_graph_that_reaches_kbf_and_scheduler_crates_names_both_chains() {
    let ids = [
        ("c", "kbf-job-client"),
        ("t", "tonic"),
        ("k", "kbf-types"),
        ("s", "js-sched"),
        ("p", "js-proto"),
        ("r", "prost"),
    ];
    let g = Graph {
        names: ids
            .iter()
            .map(|(id, name)| ((*id).to_owned(), (*name).to_owned()))
            .collect(),
        sources: ids.iter().map(|(id, _)| ((*id).to_owned(), None)).collect(),
        normal: [
            ("c", vec!["t", "s", "p"]),
            ("t", vec!["k", "r"]),
            ("p", vec!["r"]),
        ]
        .into_iter()
        .map(|(id, deps)| (id.to_owned(), deps.into_iter().map(str::to_owned).collect()))
        .collect(),
        members: BTreeMap::new(),
    };
    assert_eq!(
        violations(&g, "c"),
        [
            "kbf-job-client -> js-sched",
            "kbf-job-client -> tonic -> kbf-types"
        ]
    );
    assert!(violations(&g, "p").is_empty());
}
