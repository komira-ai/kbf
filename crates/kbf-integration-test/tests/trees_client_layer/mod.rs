//! The tierstore trees client (`kbf-trees-client`) in the layering of `tests/layering.rs`: a
//! leaf over tierstore's `tierstore-trees-api`, the generated wire contract of a build node's
//! trees interface. The design (`docs/design/trees-for-action-inputs.md`, section 8) makes the
//! frontend's executor its one user; until that stage lands no crate of the workspace depends
//! on it, and `USERS` names the crate in the change that adds it. The client reaches no kbf
//! crate and no tierstore crate but the API, and every crate of the tierstore repository comes
//! from one revision: two would build two copies of the wire types, and the pin that names the
//! tierstore the farm runs would not be the one its trees client speaks to.

use std::collections::BTreeSet;
use std::process::Command;

use serde_json::Value;

use super::{Graph, load_graph, normal_closure};

/// The client's package name.
const CLIENT: &str = "kbf-trees-client";

/// The crates that may depend on the client (with its fake as a dev-dependency too). None
/// yet: the executor's stage that creates the input trees adds `kbf-server` here.
const USERS: [&str; 0] = [];

/// The tierstore crate the client may reach: the generated messages and stubs.
const API: &str = "tierstore-trees-api";

/// The repository every tierstore crate is taken from, by one revision.
const REPOSITORY: &str = "git+https://github.com/komira-ai/tierstore?rev=";

/// The crates the workspace takes from the tierstore repository: each must be among them.
const TIERSTORE_CRATES: [&str; 3] = ["objstore-s3", "tierstore", API];

/// Whether `source` is the tierstore repository.
fn from_tierstore(source: Option<&str>) -> bool {
    source.is_some_and(|source| source.starts_with(REPOSITORY))
}

/// The chains, from `root_id`, to every crate its normal closure reaches that it may not: a
/// kbf crate, or a crate of the tierstore repository other than the API. Sorted.
fn violations(g: &Graph, root_id: &str) -> Vec<String> {
    let mut found: Vec<String> = normal_closure(g, root_id)
        .into_iter()
        .filter(|(dep, _)| {
            let name = g.names[dep].as_str();
            let tierstore = from_tierstore(g.sources[dep].as_deref()) && name != API;
            name.starts_with("kbf-") || tierstore
        })
        .map(|(_, chain)| chain)
        .collect();
    found.sort();
    found
}

/// What is wrong with the tierstore crates of `g`: a crate of the tierstore repository at
/// another source than the first, a crate of [`TIERSTORE_CRATES`] missing or taken from
/// somewhere else; empty when they are as required.
fn tierstore_problems(g: &Graph) -> Vec<String> {
    let mut out = Vec::new();
    let mut sources = BTreeSet::new();
    for (id, name) in &g.names {
        let source = g.sources[id].as_deref();
        let named = TIERSTORE_CRATES.contains(&name.as_str());
        if from_tierstore(source) {
            sources.insert(source.unwrap_or_default());
        } else if named {
            out.push(format!(
                "{name} comes from {source:?}, not {REPOSITORY}<rev>"
            ));
        }
    }
    for name in TIERSTORE_CRATES {
        if !g.names.values().any(|n| n == name) {
            out.push(format!("{name} is not in the graph"));
        }
    }
    if sources.len() > 1 {
        out.push(format!("the tierstore crates come from {sources:?}"));
    }
    out
}

/// Asserts that no crate of the workspace but the [`USERS`] depends on the trees client, in
/// any kind of dependency, and that the client's own manifest names itself once, as the
/// dev-dependency of its tests (so the walk reads the dependencies it checks).
#[test]
fn only_the_users_depend_on_the_trees_client() {
    let cargo = std::env::var("CARGO").expect("CARGO is set when cargo runs a test");
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(out.status.success(), "cargo metadata failed");
    let meta: Value = serde_json::from_slice(&out.stdout).expect("cargo metadata emits JSON");
    let packages = meta["packages"].as_array().expect("packages");
    let itself = packages
        .iter()
        .find(|p| p["name"] == CLIENT)
        .expect("kbf-trees-client is a workspace member")["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .filter(|d| d["name"] == CLIENT && d["kind"] == "dev")
        .count();
    assert_eq!(itself, 1, "the client's tests take it with the fake");
    let found = dependents(packages);
    assert!(
        found.is_empty(),
        "a crate that is not one of the users depends on the trees client: {found:?}"
    );
}

/// Each dependency on the client among `packages` (of `cargo metadata --no-deps`), as
/// `<package> (<kind>)`, except the client's own dev-dependency on itself and the
/// dependencies of the [`USERS`].
fn dependents(packages: &[Value]) -> Vec<String> {
    let mut found = Vec::new();
    for package in packages {
        let name = package["name"].as_str().expect("package name");
        for dep in package["dependencies"].as_array().expect("dependencies") {
            if dep["name"] != CLIENT {
                continue;
            }
            let kind = dep["kind"].as_str().unwrap_or("normal");
            let own_fake = name == CLIENT && kind == "dev";
            if !own_fake && !USERS.contains(&name) {
                found.push(format!("{name} ({kind})"));
            }
        }
    }
    found
}

/// Asserts that a dependency on the client is named whatever its kind, and the client's own
/// dev-dependency is not, while a normal dependency of the client on itself would be.
#[test]
fn a_crate_that_depends_on_the_trees_client_is_named() {
    let packages: Vec<Value> = serde_json::from_str(
        r#"[
            {"name": "kbf-trees-client", "dependencies": [
                {"name": "kbf-trees-client", "kind": "dev"}, {"name": "tonic", "kind": null}]},
            {"name": "kbf-node", "dependencies": [{"name": "kbf-trees-client", "kind": null}]},
            {"name": "kbf-front", "dependencies": [{"name": "kbf-trees-client", "kind": "dev"}]},
            {"name": "kbf-cli", "dependencies": [{"name": "kbf-trees-client", "kind": "build"}]}
        ]"#,
    )
    .unwrap();
    assert_eq!(
        dependents(&packages),
        ["kbf-node (normal)", "kbf-front (dev)", "kbf-cli (build)"]
    );
    assert!(dependents(&packages[..1]).is_empty());
    let itself_normal: Vec<Value> = serde_json::from_str(
        r#"[{"name": "kbf-trees-client", "dependencies": [{"name": "kbf-trees-client", "kind": null}]}]"#,
    )
    .unwrap();
    assert_eq!(dependents(&itself_normal), ["kbf-trees-client (normal)"]);
}

/// Asserts that the trees client's normal tree reaches `tierstore-trees-api` and `tonic`, and
/// no kbf crate and no other crate of the tierstore repository; and that every crate of the
/// tierstore repository in the graph, the client's API among them, comes from one revision.
#[test]
fn the_trees_client_reaches_only_the_trees_api_at_the_one_tierstore_revision() {
    let g = load_graph();
    let (id, root) = &g.members[CLIENT];
    assert!(
        root.is_some(),
        "kbf-trees-client must have a library target"
    );
    let closure = normal_closure(&g, id);
    let reached: Vec<&str> = closure.keys().map(|dep| g.names[dep].as_str()).collect();
    assert!(
        reached.contains(&API) && reached.contains(&"tonic"),
        "the walk must see {API} and tonic, or it proves nothing: {reached:?}"
    );
    let found = violations(&g, id);
    assert!(found.is_empty(), "the trees client reaches {found:?}");
    let problems = tierstore_problems(&g);
    assert!(
        problems.is_empty(),
        "tierstore crates:\n{}",
        problems.join("\n")
    );
}

/// A graph of `packages` (id, name, source) where `edges` are the normal dependencies.
fn graph(packages: &[(&str, &str, Option<&str>)], edges: &[(&str, &[&str])]) -> Graph {
    Graph {
        names: packages
            .iter()
            .map(|(id, name, _)| ((*id).to_owned(), (*name).to_owned()))
            .collect(),
        sources: packages
            .iter()
            .map(|(id, _, source)| ((*id).to_owned(), source.map(str::to_owned)))
            .collect(),
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

const REV_A: &str = "git+https://github.com/komira-ai/tierstore?rev=aaaa#aaaa";
const REV_B: &str = "git+https://github.com/komira-ai/tierstore?rev=bbbb#bbbb";

/// Asserts that the evaluator names the chain to a kbf crate and to a tierstore crate other
/// than the API (here the whole `tierstore`, reached through the API), and nothing for the
/// API and a crate of crates.io.
#[test]
fn a_trees_client_that_reaches_kbf_or_tierstore_crates_names_both_chains() {
    let g = graph(
        &[
            ("c", CLIENT, None),
            ("a", API, Some(REV_A)),
            ("t", "tierstore", Some(REV_A)),
            ("k", "kbf-types", None),
            (
                "p",
                "prost",
                Some("registry+https://github.com/rust-lang/crates.io-index"),
            ),
        ],
        &[("c", &["a", "k", "p"]), ("a", &["t", "p"])],
    );
    assert_eq!(
        violations(&g, "c"),
        [
            "kbf-trees-client -> kbf-types",
            "kbf-trees-client -> tierstore-trees-api -> tierstore"
        ]
    );
    assert!(violations(&g, "a").len() == 1);
    assert!(violations(&g, "p").is_empty());
}

/// Asserts the one-revision check passes one source for every tierstore crate, and names a
/// second revision, a tierstore crate taken from elsewhere (a registry, or a path), and one
/// missing from the graph.
#[test]
fn a_second_tierstore_revision_or_source_is_named() {
    let crates: [(&str, &str, Option<&str>); 3] = [
        ("o", "objstore-s3", Some(REV_A)),
        ("t", "tierstore", Some(REV_A)),
        ("a", API, Some(REV_A)),
    ];
    assert_eq!(
        tierstore_problems(&graph(&crates, &[])),
        Vec::<String>::new()
    );
    let mut two = crates;
    two[2].2 = Some(REV_B);
    let problems = tierstore_problems(&graph(&two, &[]));
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("the tierstore crates come from"),
        "{problems:?}"
    );
    let mut elsewhere = crates;
    elsewhere[0].2 = Some("registry+https://github.com/rust-lang/crates.io-index");
    elsewhere[1].2 = None;
    let problems = tierstore_problems(&graph(&elsewhere, &[]));
    assert_eq!(
        problems,
        [
            "objstore-s3 comes from Some(\"registry+https://github.com/rust-lang/crates.io-index\"), not git+https://github.com/komira-ai/tierstore?rev=<rev>",
            "tierstore comes from None, not git+https://github.com/komira-ai/tierstore?rev=<rev>"
        ]
    );
    let missing = tierstore_problems(&graph(&crates[..2], &[]));
    assert_eq!(missing, ["tierstore-trees-api is not in the graph"]);
}
