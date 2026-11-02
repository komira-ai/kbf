//! The MDM host layer (`mdm-core`, `host-step`, `mdm-host`) stays apart from the farm.
//!
//! The MDM installer is its own binary (`mdm-host`), beside the update coordinator and never
//! folded into the `kbf` binary, and it will leave this repository with the coordinator. So
//! its three crates link no other crate of this workspace, and the farm links none of them.
//! This test reads `cargo tree` for each crate (the features of that crate built alone) and
//! `cargo metadata` for the direct dependencies of every kind.
//!
//! Catches:
//! - an MDM crate that links `kbf-server` or the `kbf` binary's crate (`kbf-cli`), directly
//!   or through another crate, or names either as a dev-dependency (the failure names them);
//! - an MDM crate that links, or takes as a dev-dependency, any other crate of this workspace
//!   (`kbf-testkit` included, which would bring the jobscheduler repository with it);
//! - the order inside the layer broken: `mdm-core` and `host-step` link neither each other nor
//!   `mdm-host`; only `mdm-host` links both;
//! - `mdm-core`, which renders bytes from settings text, reaching an async runtime, the
//!   network or a random source (the crates `FORBIDDEN` names), or its `lib.rs` no longer
//!   denying the `clippy.toml` lists of clock, sleep, socket and hashed-collection items;
//! - `kbf-cli` or `kbf-server` linking an MDM crate (the MDM folded into the farm);
//! - `mdm-host` no longer building its own binary named `mdm-host`;
//! - an MDM crate with `kbf` in its name, or renamed or removed, and a walk that cannot see
//!   the crates it should (`mdm-host` reaching `mdm-core`, `host-step` and `ureq`), which
//!   would make every check above pass on an empty tree.
//!
//! The rules themselves are the function [`violations`], which the tests at the end run on
//! made-up trees that break each rule.

use std::collections::BTreeSet;
use std::process::Command;

use serde_json::Value;

/// The MDM crates, lowest first: a crate may link only crates listed before it.
const MDM_CRATES: &[&str] = &["mdm-core", "host-step", "mdm-host"];

/// What each MDM crate may link of the others.
const MAY_LINK: &[(&str, &[&str])] = &[
    ("mdm-core", &[]),
    ("host-step", &[]),
    ("mdm-host", &["mdm-core", "host-step"]),
];

/// The farm's crates named in a failure of their own: the server library and the `kbf`
/// binary's crate.
const FARM: &[&str] = &["kbf-server", "kbf-cli"];

/// Crates the pure renderer (`mdm-core`) must not reach: async runtimes, network stacks and
/// random sources.
const FORBIDDEN: &[&str] = &[
    "tokio",
    "tonic",
    "hyper",
    "reqwest",
    "ureq",
    "rand",
    "rand_core",
    "rand_chacha",
    "getrandom",
    "mio",
    "socket2",
];

const DENY_ATTR: &str = "#![deny(clippy::disallowed_methods, clippy::disallowed_types)]";

/// What one MDM crate holds: the names `cargo tree` prints for its normal edges, and the
/// names of its direct dev-dependencies.
struct Tree<'a> {
    name: &'a str,
    linked: BTreeSet<String>,
    dev: BTreeSet<String>,
}

/// Every rule the MDM crates break, given their trees and the workspace's member names.
/// Empty when they keep them all.
fn violations(trees: &[Tree<'_>], members: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    for tree in trees {
        let name = tree.name;
        if name.contains("kbf") {
            out.push(format!("{name}: an MDM crate is named without kbf"));
        }
        let allowed: &[&str] = MAY_LINK
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(&[], |(_, a)| a);
        for (how, set) in [("links", &tree.linked), ("dev-depends on", &tree.dev)] {
            for dep in set {
                if dep == name {
                    continue;
                }
                if FARM.contains(&dep.as_str()) {
                    out.push(format!("{name} {how} {dep}, a crate of the farm"));
                } else if members.contains(dep) && !allowed.contains(&dep.as_str()) {
                    out.push(format!(
                        "{name} {how} {dep}, another crate of this workspace"
                    ));
                }
            }
        }
        if name == "mdm-core" {
            for dep in FORBIDDEN {
                if tree.linked.contains(*dep) {
                    out.push(format!("mdm-core links {dep}, which a pure crate may not"));
                }
            }
        }
    }
    out
}

/// The names `cargo tree` prints for the normal edges of `package` built alone.
fn linked_by(package: &str) -> BTreeSet<String> {
    let cargo = std::env::var("CARGO").expect("CARGO is set when cargo runs a test");
    let out = Command::new(cargo)
        .args(["tree", "--locked", "--edges", "normal", "--prefix", "none"])
        .args(["--format", "{p}", "--package", package])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("cargo tree prints text")
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

/// The workspace's packages, from `cargo metadata --no-deps`.
fn workspace_packages() -> Vec<Value> {
    let cargo = std::env::var("CARGO").expect("CARGO is set when cargo runs a test");
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(out.status.success(), "cargo metadata failed");
    let meta: Value = serde_json::from_slice(&out.stdout).expect("cargo metadata emits JSON");
    meta["packages"].as_array().expect("packages").clone()
}

fn package<'a>(packages: &'a [Value], name: &str) -> &'a Value {
    packages
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("{name} is a workspace member"))
}

/// The direct dev-dependencies of a package.
fn dev_dependencies(pkg: &Value) -> BTreeSet<String> {
    pkg["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .filter(|d| d["kind"] == "dev")
        .filter_map(|d| d["name"].as_str().map(str::to_owned))
        .collect()
}

/// The source path of the target of `pkg` with this kind and name.
fn target_path(pkg: &Value, kind: &str, name: &str) -> Option<String> {
    pkg["targets"]
        .as_array()
        .expect("targets")
        .iter()
        .find(|t| {
            t["name"] == name
                && t["kind"]
                    .as_array()
                    .is_some_and(|k| k.iter().any(|k| k == kind))
        })
        .and_then(|t| t["src_path"].as_str().map(str::to_owned))
}

/// Asserts that the MDM crates link no crate of the farm and no other crate of this workspace,
/// in their normal trees or among their dev-dependencies, in the layer's own order, and that
/// the walk sees what it should, so the check cannot pass on an empty tree.
#[test]
fn the_mdm_crates_link_no_farm_crate_and_no_other_workspace_crate() {
    let packages = workspace_packages();
    let members: BTreeSet<String> = packages
        .iter()
        .map(|p| p["name"].as_str().expect("package name").to_owned())
        .collect();
    let trees: Vec<Tree<'_>> = MDM_CRATES
        .iter()
        .map(|name| Tree {
            name,
            linked: linked_by(name),
            dev: dev_dependencies(package(&packages, name)),
        })
        .collect();
    let host = &trees[2];
    for seen in ["mdm-core", "host-step", "ureq"] {
        assert!(
            host.linked.contains(seen),
            "mdm-host's tree must hold {seen}, or this walk proves nothing: {:?}",
            host.linked
        );
    }
    let found = violations(&trees, &members);
    assert!(found.is_empty(), "{found:#?}");
}

/// Asserts that `mdm-core` denies the `clippy.toml` lists in its `lib.rs`, so the clock,
/// sleep, socket and hashed-collection checks stay on for it.
#[test]
fn the_mdm_renderer_denies_the_clippy_lists() {
    let packages = workspace_packages();
    let lib = target_path(package(&packages, "mdm-core"), "lib", "mdm_core")
        .expect("mdm-core has a library");
    let text = std::fs::read_to_string(&lib).expect("read mdm-core's lib.rs");
    assert!(
        text.lines().any(|l| l.trim() == DENY_ATTR),
        "{lib} must carry `{DENY_ATTR}` on one line"
    );
}

/// Asserts that neither the `kbf` binary's crate nor the server links an MDM crate: the MDM
/// is a binary of its own and is never folded into the farm.
#[test]
fn the_farm_links_no_mdm_crate() {
    for farm in FARM {
        let linked = linked_by(farm);
        assert!(
            linked.contains("kbf-proto"),
            "{farm}'s tree must hold kbf-proto, or this walk proves nothing"
        );
        let mdm: Vec<&&str> = MDM_CRATES.iter().filter(|m| linked.contains(**m)).collect();
        assert!(mdm.is_empty(), "{farm} links {mdm:?}");
    }
}

/// Asserts that `mdm-host` builds its own binary, named `mdm-host`.
#[test]
fn the_mdm_host_crate_builds_its_own_binary() {
    let packages = workspace_packages();
    assert!(
        target_path(package(&packages, "mdm-host"), "bin", "mdm-host").is_some(),
        "mdm-host must build the binary mdm-host"
    );
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn members() -> BTreeSet<String> {
    set(&[
        "mdm-core",
        "host-step",
        "mdm-host",
        "kbf-server",
        "kbf-cli",
        "kbf-testkit",
    ])
}

fn clean() -> Vec<Tree<'static>> {
    vec![
        Tree {
            name: "mdm-core",
            linked: set(&["mdm-core", "sha2"]),
            dev: BTreeSet::new(),
        },
        Tree {
            name: "host-step",
            linked: set(&["host-step", "rustix"]),
            dev: BTreeSet::new(),
        },
        Tree {
            name: "mdm-host",
            linked: set(&["mdm-host", "mdm-core", "host-step", "ureq"]),
            dev: BTreeSet::new(),
        },
    ]
}

/// Catches a rule function that refuses the layer as it is: the trees the crates have today
/// pass.
#[test]
fn trees_that_keep_the_rules_pass() {
    assert_eq!(violations(&clean(), &members()), Vec::<String>::new());
}

/// Catches the MDM host layer linking the server or the `kbf` binary's crate, through its
/// normal tree or a dev-dependency: each is refused by name.
#[test]
fn a_dependency_on_the_server_or_the_kbf_binary_is_refused() {
    let mut trees = clean();
    trees[2].linked.insert("kbf-server".to_owned());
    trees[1].dev.insert("kbf-cli".to_owned());
    assert_eq!(
        violations(&trees, &members()),
        vec![
            "host-step dev-depends on kbf-cli, a crate of the farm".to_owned(),
            "mdm-host links kbf-server, a crate of the farm".to_owned(),
        ]
    );
}

/// Catches an MDM crate taking another workspace crate (the test kit here), or linking a crate
/// of its own layer out of order.
#[test]
fn another_workspace_crate_or_the_wrong_order_is_refused() {
    let mut trees = clean();
    trees[1].dev.insert("kbf-testkit".to_owned());
    trees[0].linked.insert("host-step".to_owned());
    assert_eq!(
        violations(&trees, &members()),
        vec![
            "mdm-core links host-step, another crate of this workspace".to_owned(),
            "host-step dev-depends on kbf-testkit, another crate of this workspace".to_owned(),
        ]
    );
}

/// Catches the pure renderer reaching a network or random-source crate, and an MDM crate named
/// with `kbf`.
#[test]
fn an_impure_renderer_or_a_kbf_name_is_refused() {
    let mut trees = clean();
    trees[0].linked.insert("getrandom".to_owned());
    assert_eq!(
        violations(&trees, &members()),
        vec!["mdm-core links getrandom, which a pure crate may not".to_owned()]
    );
    let named = [Tree {
        name: "kbf-mdm-tool",
        linked: set(&["kbf-mdm-tool", "sha2"]),
        dev: BTreeSet::new(),
    }];
    assert_eq!(
        violations(&named, &members()),
        vec!["kbf-mdm-tool: an MDM crate is named without kbf".to_owned()]
    );
}
