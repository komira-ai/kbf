//! The L2 backfill tool (`kbf-l2-backfill`) in the layering of `tests/layering.rs`: an operator
//! binary of its own, built on the frontend's tree writer (`kbf-front`'s `Sidecars`) and the
//! pack index (`kbf-pack-index`), and never part of `kbf`. Its normal tree links neither the
//! server nor a binary of the farm nor the transfer engine (`MUST_NOT_REACH`), and no crate of
//! the workspace depends on it, in any kind of dependency.

use std::process::Command;

use serde_json::Value;

use super::{Graph, load_graph, normal_closure};

/// The tool's package name.
const TOOL: &str = "kbf-l2-backfill";

/// What the tool's normal dependencies must not reach: the server, the `kbf` binary, the
/// installer, and the blob transfer engine. The tool reads and writes the store and the index
/// through the frontend's tree writer; a path to one of these would make it link a farm binary.
const MUST_NOT_REACH: &[&str] = &["kbf-server", "kbf-cli", "kbf-installer", "kbf-transfer"];

/// What the walk must see beneath the tool, or it proves nothing.
const MUST_REACH: &[&str] = &["kbf-front", "kbf-pack-index"];

/// The chains, from `root_id`, to every crate of [`MUST_NOT_REACH`] its normal closure
/// contains. Sorted.
fn violations(g: &Graph, root_id: &str) -> Vec<String> {
    let mut found: Vec<String> = normal_closure(g, root_id)
        .into_iter()
        .filter(|(dep, _)| MUST_NOT_REACH.contains(&g.names[dep].as_str()))
        .map(|(_, chain)| chain)
        .collect();
    found.sort();
    found
}

/// Asserts that the tool's normal tree reaches the frontend's tree writer and the pack index,
/// and none of [`MUST_NOT_REACH`].
#[test]
fn the_l2_backfill_tool_reaches_no_server_binary_or_transfer_crate() {
    let g = load_graph();
    let (id, root) = &g.members[TOOL];
    assert!(root.is_some(), "kbf-l2-backfill must have a library target");
    let closure = normal_closure(&g, id);
    let reached: Vec<&str> = closure.keys().map(|dep| g.names[dep].as_str()).collect();
    for name in MUST_REACH {
        assert!(
            reached.contains(name),
            "the walk must see {name}, or it proves nothing: {reached:?}"
        );
    }
    let found = violations(&g, id);
    assert!(found.is_empty(), "kbf-l2-backfill reaches {found:?}");
}

/// Each dependency on the tool among `packages` (of `cargo metadata --no-deps`), as
/// `<package> (<kind>)`.
fn dependents(packages: &[Value]) -> Vec<String> {
    let mut found = Vec::new();
    for package in packages {
        let name = package["name"].as_str().expect("package name");
        for dep in package["dependencies"].as_array().expect("dependencies") {
            if dep["name"] == TOOL {
                let kind = dep["kind"].as_str().unwrap_or("normal");
                found.push(format!("{name} ({kind})"));
            }
        }
    }
    found
}

/// Asserts that no crate of the workspace depends on the tool, in any kind of dependency, and
/// that the read sees the tool itself (so the check does not pass on an empty package list).
#[test]
fn no_crate_depends_on_the_l2_backfill_tool() {
    let cargo = std::env::var("CARGO").expect("CARGO is set when cargo runs a test");
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(out.status.success(), "cargo metadata failed");
    let meta: Value = serde_json::from_slice(&out.stdout).expect("cargo metadata emits JSON");
    let packages = meta["packages"].as_array().expect("packages");
    assert!(
        packages.iter().any(|p| p["name"] == TOOL),
        "kbf-l2-backfill is a workspace member"
    );
    let found = dependents(packages);
    assert!(found.is_empty(), "a crate depends on kbf-l2-backfill: {found:?}");
}

/// Asserts that a dependency on the tool is named whatever its kind, and that packages with
/// none give nothing.
#[test]
fn a_crate_that_depends_on_the_l2_backfill_tool_is_named() {
    let packages: Vec<Value> = serde_json::from_str(
        r#"[
            {"name": "kbf-l2-backfill", "dependencies": [{"name": "kbf-front", "kind": null}]},
            {"name": "kbf-cli", "dependencies": [{"name": "kbf-l2-backfill", "kind": null}]},
            {"name": "kbf-server", "dependencies": [{"name": "kbf-l2-backfill", "kind": "dev"}]},
            {"name": "kbf-installer", "dependencies": [{"name": "kbf-l2-backfill", "kind": "build"}]}
        ]"#,
    )
    .expect("the fixture is JSON");
    assert_eq!(
        dependents(&packages),
        ["kbf-cli (normal)", "kbf-server (dev)", "kbf-installer (build)"]
    );
    assert!(dependents(&packages[..1]).is_empty());
}

/// A graph of `names` (id, name) where `edges` are the normal dependencies.
fn graph(names: &[(&str, &str)], edges: &[(&str, &[&str])]) -> Graph {
    Graph {
        names: names
            .iter()
            .map(|(id, name)| ((*id).to_owned(), (*name).to_owned()))
            .collect(),
        sources: names.iter().map(|(id, _)| ((*id).to_owned(), None)).collect(),
        normal: edges
            .iter()
            .map(|(id, deps)| {
                (
                    (*id).to_owned(),
                    deps.iter().map(|d| (*d).to_owned()).collect(),
                )
            })
            .collect(),
        members: std::collections::BTreeMap::new(),
    }
}

/// Asserts that the evaluator names the chain to the server reached through another crate and
/// to the transfer engine reached directly, and nothing for the frontend and the pack index.
#[test]
fn a_tool_that_reaches_the_server_or_the_transfer_engine_names_each_chain() {
    let g = graph(
        &[
            ("t", TOOL),
            ("f", "kbf-front"),
            ("i", "kbf-pack-index"),
            ("s", "kbf-server"),
            ("x", "kbf-transfer"),
        ],
        &[("t", &["f", "i", "x"]), ("f", &["s"])],
    );
    assert_eq!(
        violations(&g, "t"),
        [
            "kbf-l2-backfill -> kbf-front -> kbf-server",
            "kbf-l2-backfill -> kbf-transfer"
        ]
    );
    assert!(violations(&g, "i").is_empty());
}
