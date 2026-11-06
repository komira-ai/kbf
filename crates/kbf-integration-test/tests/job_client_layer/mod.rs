//! The job service client (`kbf-job-client`) in the layering of `tests/layering.rs`: a leaf
//! over the jobscheduler's `js-proto`, and one crate of the workspace uses it: `kbf-server`,
//! whose host executor submits its jobs to the job service through it, as a normal dependency
//! and, for the fake job server of its tests, as a dev-dependency. The design makes kbf a
//! client of the job service that links none of the scheduler's server crates, and the host
//! executor no longer leases hosts: no file of it names the lease client.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::{Graph, load_graph, normal_closure};

/// The client's package name.
const CLIENT: &str = "kbf-job-client";

/// The one crate that uses the client.
const USER: &str = "kbf-server";

/// The crate the host executor must not name.
const LEASE_CLIENT: &str = "kbf_lease_client";

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

/// Asserts that the only crate of the workspace that depends on the job client is the server
/// (the host executor), as a normal dependency and as a dev-dependency for the fake job
/// server, and that the client's own manifest names itself once, as the dev-dependency of its
/// tests (so the walk reads the dependencies it checks).
#[test]
fn only_the_server_depends_on_the_job_client() {
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
    let mut found = dependents(packages);
    found.sort();
    assert_eq!(
        found,
        [format!("{USER} (dev)"), format!("{USER} (normal)")],
        "the server alone depends on the job client, normally and with the fake"
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

/// The files among `files` (each a path and its text) that name the lease client, by path.
fn naming_the_lease_client(files: &[(PathBuf, String)]) -> Vec<&Path> {
    files
        .iter()
        .filter(|(_, text)| text.contains(LEASE_CLIENT))
        .map(|(path, _)| path.as_path())
        .collect()
}

/// Every `.rs` file under `dir`, with its text, in path order.
fn rust_files(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    entries.sort();
    let mut found = Vec::new();
    for path in entries {
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            found.push((path, text));
        }
    }
    found
}

/// Asserts that no `.rs` file of the host executor (`kbf-server/src/executor`) names
/// `kbf_lease_client`: the executor submits jobs and leases no host. The walk must see the
/// executor's `mod.rs` and its submit module, or it would prove nothing.
#[test]
fn an_executor_file_that_names_the_lease_client_is_refused() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../kbf-server/src/executor");
    let files = rust_files(&dir);
    for seen in ["mod.rs", "submit.rs"] {
        assert!(
            files.iter().any(|(path, _)| path.ends_with(seen)),
            "the walk of {} does not see {seen}",
            dir.display()
        );
    }
    let found = naming_the_lease_client(&files);
    assert!(
        found.is_empty(),
        "the host executor names the lease client in {found:?}"
    );
}

/// Asserts that the scan names a file that uses the lease client, by path, and only that file:
/// not one that merely talks of the job client, nor an empty one.
#[test]
fn a_synthetic_executor_file_that_names_the_lease_client_is_named() {
    let files = [
        (
            PathBuf::from("executor/clean.rs"),
            "use kbf_job_client::Client;\n".to_owned(),
        ),
        (
            PathBuf::from("executor/leasing.rs"),
            "use kbf_lease_client::Client;\n".to_owned(),
        ),
        (PathBuf::from("executor/empty.rs"), String::new()),
    ];
    assert_eq!(
        naming_the_lease_client(&files),
        [Path::new("executor/leasing.rs")]
    );
}
