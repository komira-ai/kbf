//! The step runner (`kbf-step-runner`) in the layering of `tests/layering.rs`: it links no crate
//! of this workspace but the hook contract, so the root part of the updater (`kbf-updater`) is
//! where it meets the rights crate.

use std::collections::BTreeSet;
use std::process::Command;

use serde_json::Value;

use super::{linked_by, load_graph};

/// What the step runner may link: itself and the hook contract.
const MAY_LINK: [&str; 2] = ["kbf-step-runner", "update-hook"];
/// What it may name as a dependency, dev-dependencies included: those, and the registration client
/// its tests run against its server.
const MAY_DEPEND_ON: [&str; 3] = ["kbf-step-runner", "update-hook", "update-register"];

/// The workspace crates among `linked` (what the step runner's tree reaches) that it may not link.
fn forbidden_links<'a>(
    members: impl IntoIterator<Item = &'a str>,
    linked: &BTreeSet<String>,
) -> Vec<&'a str> {
    members
        .into_iter()
        .filter(|name| linked.contains(*name) && !MAY_LINK.contains(name))
        .collect()
}

/// The workspace crates among `deps` (what the step runner's manifest names) it may not depend on.
fn forbidden_deps<'a>(
    deps: impl IntoIterator<Item = &'a str>,
    members: &BTreeSet<&str>,
) -> BTreeSet<&'a str> {
    deps.into_iter()
        .filter(|name| members.contains(name) && !MAY_DEPEND_ON.contains(name))
        .collect()
}

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
    let violations = forbidden_links(g.members.keys().map(String::as_str), &linked);
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
    let deps = forbidden_deps(
        pkg["dependencies"]
            .as_array()
            .expect("dependencies")
            .iter()
            .filter_map(|d| d["name"].as_str()),
        &members,
    );
    assert!(
        deps.is_empty(),
        "kbf-step-runner depends (even as a dev-dependency) on {deps:?}"
    );
}

/// Asserts that the rule of the step runner's layer rejects what it is there to reject: a tree
/// that reaches a product crate of the workspace (here `kbf-proto`) or the rights crate is named,
/// while crates outside the workspace and the hook contract pass.
#[test]
fn a_step_runner_that_links_a_workspace_crate_beyond_the_hook_contract_is_named() {
    let members = [
        "kbf-step-runner",
        "update-hook",
        "kbf-proto",
        "update-rights",
    ];
    let linked: BTreeSet<String> = ["update-hook", "serde_json", "kbf-proto", "update-rights"]
        .map(str::to_owned)
        .into();
    assert_eq!(
        forbidden_links(members, &linked),
        ["kbf-proto", "update-rights"]
    );
    let clean: BTreeSet<String> = ["update-hook", "serde_json"].map(str::to_owned).into();
    assert!(forbidden_links(members, &clean).is_empty());
}

/// Asserts that a dependency on a workspace crate beyond the hook contract and the registration
/// client is named, dev-dependencies included, and one outside the workspace is not.
#[test]
fn a_step_runner_that_depends_on_a_workspace_crate_beyond_its_two_is_named() {
    let members: BTreeSet<&str> = [
        "kbf-step-runner",
        "update-hook",
        "update-register",
        "kbf-updater",
    ]
    .into();
    let deps = ["update-hook", "update-register", "kbf-updater", "serde"];
    assert_eq!(
        forbidden_deps(deps, &members),
        BTreeSet::from(["kbf-updater"])
    );
    assert!(forbidden_deps(["update-hook", "serde"], &members).is_empty());
}
