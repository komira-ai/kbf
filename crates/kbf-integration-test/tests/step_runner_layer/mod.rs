//! The step runner (`kbf-step-runner`) in the layering of `tests/layering.rs`: it links no crate
//! of this workspace but the hook contract, so the root part of the updater (`kbf-updater`) is
//! where it meets the rights crate.

use std::collections::BTreeSet;
use std::process::Command;

use serde_json::Value;

use super::{linked_by, load_graph, workspace_crates_beyond};

/// Asserts that the step runner (`kbf-step-runner`) links no crate of this workspace but the hook
/// contract (`update-hook`), that among its dependencies, dev-dependencies included, the only other
/// workspace crate is the registration client (`update-register`), whose tests it runs against
/// its server, and that the walk sees its dependencies (it reaches `update-hook` and `serde_json`).
/// The root part of the updater (`kbf-updater`) wires it to the rights crate; neither library gains
/// a product dependency for that.
#[test]
fn the_step_runner_reaches_only_the_hook_contract() {
    let g = load_graph();
    let (_, root) = g
        .members
        .get("kbf-step-runner")
        .expect("kbf-step-runner is a workspace member");
    assert!(root.is_some(), "kbf-step-runner must have a library target");
    let linked = linked_by("kbf-step-runner");
    assert!(
        linked.contains("update-hook") && linked.contains("serde_json"),
        "kbf-step-runner's tree must hold update-hook and serde_json, or this walk proves nothing: {linked:?}"
    );
    let violations = workspace_crates_beyond(&g, &linked, &["kbf-step-runner", "update-hook"]);
    assert!(
        violations.is_empty(),
        "kbf-step-runner links another crate of this workspace: {violations:?}"
    );

    let cargo = std::env::var("CARGO").expect("CARGO is set when cargo runs a test");
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(out.status.success(), "cargo metadata failed");
    let meta: Value = serde_json::from_slice(&out.stdout).expect("cargo metadata emits JSON");
    let packages = meta["packages"].as_array().expect("packages");
    let members: BTreeSet<&str> = packages
        .iter()
        .map(|p| p["name"].as_str().expect("package name"))
        .collect();
    let pkg = packages
        .iter()
        .find(|p| p["name"] == "kbf-step-runner")
        .expect("kbf-step-runner is a workspace member");
    let allowed = ["kbf-step-runner", "update-hook", "update-register"];
    let deps: BTreeSet<&str> = pkg["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .filter_map(|d| d["name"].as_str())
        .filter(|name| members.contains(name) && !allowed.contains(name))
        .collect();
    assert!(
        deps.is_empty(),
        "kbf-step-runner depends (even as a dev-dependency) on {deps:?}"
    );
}
