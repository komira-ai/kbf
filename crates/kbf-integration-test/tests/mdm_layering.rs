//! The MDM host layer (`mdm-core`, `host-step`, `mdm-host`) stays apart from the farm, as the
//! `[mdm]` group of the root `layers.toml` declares.
//!
//! The MDM installer is its own binary (`mdm-host`), beside the update coordinator and never
//! folded into the `kbf` binary, and it will leave this repository with the coordinator. So
//! its crates link no other crate of this workspace, and nothing outside the group links them.
//! This test reads the group from `layers.toml`, `cargo tree` for each of its crates with all
//! features on (so a dependency behind a feature is in the walk), and `cargo metadata` for the
//! direct dependencies of every kind, optional ones included.
//!
//! Catches:
//! - a crate of the group that links a crate its `farm` row names (`kbf-server`, the `kbf`
//!   binary's crate `kbf-cli`), directly or through another crate (behind a feature too), or
//!   declares one as a dependency of any kind, optional or not (the failure names them);
//! - a crate of the group that links, or takes as a dev- or build-dependency, a crate of this
//!   workspace its `may_link` row does not name (`kbf-testkit` included), which is how the
//!   order inside the layer is kept (only `mdm-host` links the other two);
//! - a crate of the workspace outside the group that depends on a crate of it (a crate added
//!   to the layer and not registered in `layers.toml`, or the farm linking the MDM);
//! - a `layers.toml` that is missing, has no `[mdm]` group, names in `may_link` or `pure` a crate
//!   the group does not hold, puts a farm crate in the group, or names a crate that is not a
//!   member of the workspace;
//! - a crate the group lists as `pure` reaching an async runtime, the network or a random source
//!   (the crates `FORBIDDEN` names), or its `lib.rs` no longer denying the `clippy.toml` lists;
//! - `kbf-cli` or `kbf-server` linking a crate of the group through any chain;
//! - `mdm-host` no longer building its own binary named `mdm-host`;
//! - a crate of the group with `kbf` in its name, and a walk that cannot see the crates it should
//!   (`mdm-host` reaching `mdm-core`, `host-step` and `ureq`), which would make every check above
//!   pass on an empty tree.
//!
//! The rules are the functions [`group`], [`violations`] and [`unregistered`], which the tests at
//! the end run on made-up declarations and trees that break each rule.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use serde_json::Value;

/// The `[mdm]` group of `layers.toml`.
#[derive(Debug)]
struct Group {
    /// Crates refused by name, in either direction.
    farm: BTreeSet<String>,
    /// Crates of the group that must stay pure.
    pure: BTreeSet<String>,
    /// Every crate of the group, with the crates of the group it may link.
    may_link: BTreeMap<String, BTreeSet<String>>,
}

impl Group {
    fn holds(&self, name: &str) -> bool {
        self.may_link.contains_key(name)
    }
}

/// The text of a TOML array of strings, or why it is not one.
fn names(table: &toml::Table, key: &str) -> Result<BTreeSet<String>, String> {
    let Some(value) = table.get(key) else {
        return Ok(BTreeSet::new());
    };
    value
        .as_array()
        .ok_or(format!("{key} is not a list"))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or(format!("{key} holds a non-string"))
        })
        .collect()
}

/// The `[mdm]` group declared in `text` (a `layers.toml`), or why it is refused.
fn group(text: &str) -> Result<Group, String> {
    let doc: toml::Table = text.parse().map_err(|e| format!("layers.toml: {e}"))?;
    let mdm = doc
        .get("mdm")
        .and_then(toml::Value::as_table)
        .ok_or("layers.toml has no [mdm] group")?;
    let rows = mdm
        .get("may_link")
        .and_then(toml::Value::as_table)
        .ok_or("[mdm] has no may_link table")?;
    let mut may_link = BTreeMap::new();
    for name in rows.keys() {
        may_link.insert(name.clone(), names(rows, name)?);
    }
    let g = Group {
        farm: names(mdm, "farm")?,
        pure: names(mdm, "pure")?,
        may_link,
    };
    for (name, allowed) in &g.may_link {
        if let Some(stray) = allowed.iter().find(|a| !g.holds(a)) {
            return Err(format!(
                "{name} may link {stray}, which the group does not hold"
            ));
        }
    }
    if let Some(stray) = g.pure.iter().find(|p| !g.holds(p)) {
        return Err(format!("pure names {stray}, which the group does not hold"));
    }
    if let Some(both) = g.farm.iter().find(|f| g.holds(f)) {
        return Err(format!("{both} is both in the group and in its farm"));
    }
    Ok(g)
}

/// The group declared in the repository's `layers.toml`.
fn declared() -> Group {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../layers.toml");
    let text = std::fs::read_to_string(path).expect("read layers.toml at the repository root");
    group(&text).unwrap_or_else(|e| panic!("{e}"))
}

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

/// What one MDM crate holds: the names `cargo tree` prints for its normal edges with every
/// feature on, the names of its direct normal dependencies as declared (optional ones too),
/// and those of its direct dev- and build-dependencies.
struct Tree<'a> {
    name: &'a str,
    linked: BTreeSet<String>,
    declared: BTreeSet<String>,
    dev: BTreeSet<String>,
    build: BTreeSet<String>,
}

/// Every rule the group's crates break, given their trees and the workspace's member names.
/// Empty when they keep them all.
fn violations(g: &Group, trees: &[Tree<'_>], members: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let none = BTreeSet::new();
    for tree in trees {
        let name = tree.name;
        if name.contains("kbf") {
            out.push(format!("{name}: an MDM crate is named without kbf"));
        }
        let allowed = g.may_link.get(name).unwrap_or(&none);
        for (how, set) in [
            ("links", &tree.linked),
            ("declares", &tree.declared),
            ("dev-depends on", &tree.dev),
            ("build-depends on", &tree.build),
        ] {
            for dep in set {
                if dep == name {
                    continue;
                }
                if g.farm.contains(dep) {
                    out.push(format!("{name} {how} {dep}, a crate of the farm"));
                } else if members.contains(dep) && !allowed.contains(dep) {
                    out.push(format!(
                        "{name} {how} {dep}, another crate of this workspace"
                    ));
                }
            }
        }
        if g.pure.contains(name) {
            for dep in FORBIDDEN {
                if tree.linked.contains(*dep) {
                    out.push(format!("{name} links {dep}, which a pure crate may not"));
                }
            }
        }
    }
    out
}

/// Every crate outside the group that depends (directly, any kind) on a crate of it, given each
/// workspace member's direct dependencies; and every crate of the group that is not a member.
fn unregistered(g: &Group, direct: &BTreeMap<String, BTreeSet<String>>) -> Vec<String> {
    let mut out = Vec::new();
    for name in g.may_link.keys() {
        if !direct.contains_key(name) {
            out.push(format!(
                "{name} is in the group but not a member of the workspace"
            ));
        }
    }
    for (member, deps) in direct {
        if g.holds(member) {
            continue;
        }
        for dep in deps.iter().filter(|d| g.holds(d)) {
            out.push(format!(
                "{member} depends on {dep}, a crate of the group, and is not registered in it"
            ));
        }
    }
    out
}

/// The names `cargo tree` prints for the normal edges of `package` built alone.
fn linked_by(package: &str) -> BTreeSet<String> {
    let cargo = std::env::var("CARGO").expect("CARGO is set when cargo runs a test");
    let out = Command::new(cargo)
        .args(["tree", "--locked", "--edges", "normal", "--prefix", "none"])
        .args(["--format", "{p}", "--all-features", "--package", package])
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

/// The direct dependencies of a package of this kind (`dev`, `build`, or `None` for normal),
/// optional ones included.
fn dependencies_of_kind(pkg: &Value, kind: Option<&str>) -> BTreeSet<String> {
    pkg["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .filter(|d| d["kind"].as_str() == kind)
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

/// Asserts that the group's crates link no crate of the farm and no crate of this workspace
/// their `may_link` row does not name, by any kind of dependency, and that the walk sees what
/// it should, so the check cannot pass on an empty tree.
#[test]
fn the_mdm_crates_link_no_farm_crate_and_no_other_workspace_crate() {
    let g = declared();
    let packages = workspace_packages();
    let members: BTreeSet<String> = packages
        .iter()
        .map(|p| p["name"].as_str().expect("package name").to_owned())
        .collect();
    let trees: Vec<Tree<'_>> = g
        .may_link
        .keys()
        .map(|name| Tree {
            name,
            linked: linked_by(name),
            declared: dependencies_of_kind(package(&packages, name), None),
            dev: dependencies_of_kind(package(&packages, name), Some("dev")),
            build: dependencies_of_kind(package(&packages, name), Some("build")),
        })
        .collect();
    let host = trees
        .iter()
        .find(|t| t.name == "mdm-host")
        .expect("layers.toml registers mdm-host");
    for seen in ["mdm-core", "host-step", "ureq"] {
        assert!(
            host.linked.contains(seen),
            "mdm-host's tree must hold {seen}, or this walk proves nothing: {:?}",
            host.linked
        );
    }
    let found = violations(&g, &trees, &members);
    assert!(found.is_empty(), "{found:#?}");
}

/// Asserts that every crate of the group is a member of the workspace and that no member
/// outside the group depends on one of its crates: a crate that joins the layer is registered
/// in `layers.toml`.
#[test]
fn every_crate_that_depends_on_the_group_is_registered_in_layers_toml() {
    let g = declared();
    let direct: BTreeMap<String, BTreeSet<String>> = workspace_packages()
        .iter()
        .map(|p| {
            let deps = p["dependencies"]
                .as_array()
                .expect("dependencies")
                .iter()
                .filter_map(|d| d["name"].as_str().map(str::to_owned))
                .collect();
            (p["name"].as_str().expect("package name").to_owned(), deps)
        })
        .collect();
    assert!(
        direct["mdm-host"].contains("mdm-core"),
        "mdm-host must depend on mdm-core, or this check proves nothing"
    );
    let found = unregistered(&g, &direct);
    assert!(found.is_empty(), "{found:#?}");
}

/// Asserts that every crate the group lists as pure (`mdm-core`) denies the `clippy.toml` lists
/// in its `lib.rs`, so the clock, sleep, socket and hashed-collection checks stay on for it.
#[test]
fn the_mdm_renderer_denies_the_clippy_lists() {
    let g = declared();
    assert!(
        g.pure.contains("mdm-core"),
        "layers.toml keeps mdm-core pure"
    );
    let packages = workspace_packages();
    for name in &g.pure {
        let lib = target_path(package(&packages, name), "lib", &name.replace('-', "_"))
            .unwrap_or_else(|| panic!("{name} has a library"));
        let text = std::fs::read_to_string(&lib).expect("read the library root");
        assert!(
            text.lines().any(|l| l.trim() == DENY_ATTR),
            "{lib} must carry `{DENY_ATTR}` on one line"
        );
    }
}

/// Asserts that neither the `kbf` binary's crate nor the server links an MDM crate: the MDM
/// is a binary of its own and is never folded into the farm.
#[test]
fn the_farm_links_no_mdm_crate() {
    let g = declared();
    assert!(!g.farm.is_empty(), "layers.toml names the farm's crates");
    for farm in &g.farm {
        let linked = linked_by(farm);
        assert!(
            linked.contains("kbf-proto"),
            "{farm}'s tree must hold kbf-proto, or this walk proves nothing"
        );
        let mdm: Vec<&String> = g.may_link.keys().filter(|m| linked.contains(*m)).collect();
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

/// The repository's declaration, as a made-up `layers.toml` the rule tests use.
const LAYERS: &str = r#"
[mdm]
farm = ["kbf-server", "kbf-cli"]
pure = ["mdm-core"]

[mdm.may_link]
mdm-core = []
host-step = []
mdm-host = ["mdm-core", "host-step"]
"#;

fn layers() -> Group {
    group(LAYERS).expect("the made-up declaration parses")
}

fn clean() -> Vec<Tree<'static>> {
    vec![
        Tree {
            name: "mdm-core",
            linked: set(&["mdm-core", "sha2"]),
            declared: BTreeSet::new(),
            dev: BTreeSet::new(),
            build: BTreeSet::new(),
        },
        Tree {
            name: "host-step",
            linked: set(&["host-step", "rustix"]),
            declared: BTreeSet::new(),
            dev: BTreeSet::new(),
            build: BTreeSet::new(),
        },
        Tree {
            name: "mdm-host",
            linked: set(&["mdm-host", "mdm-core", "host-step", "ureq"]),
            declared: BTreeSet::new(),
            dev: BTreeSet::new(),
            build: BTreeSet::new(),
        },
    ]
}

/// Catches a rule function that refuses the layer as it is: the trees the crates have today
/// pass.
#[test]
fn trees_that_keep_the_rules_pass() {
    assert_eq!(
        violations(&layers(), &clean(), &members()),
        Vec::<String>::new()
    );
}

/// Catches the MDM host layer linking the server or the `kbf` binary's crate, through its
/// normal tree or a dev-dependency: each is refused by name.
#[test]
fn a_dependency_on_the_server_or_the_kbf_binary_is_refused() {
    let mut trees = clean();
    trees[2].linked.insert("kbf-server".to_owned());
    trees[1].dev.insert("kbf-cli".to_owned());
    assert_eq!(
        violations(&layers(), &trees, &members()),
        vec![
            "host-step dev-depends on kbf-cli, a crate of the farm".to_owned(),
            "mdm-host links kbf-server, a crate of the farm".to_owned(),
        ]
    );
}

/// Catches a build script of an MDM crate that depends on the server: a build-dependency is
/// refused by name like the other edges.
#[test]
fn a_build_dependency_on_the_server_is_refused() {
    let mut trees = clean();
    trees[0].build.insert("kbf-server".to_owned());
    assert_eq!(
        violations(&layers(), &trees, &members()),
        vec!["mdm-core build-depends on kbf-server, a crate of the farm".to_owned()]
    );
}

/// Catches a dependency on the `kbf` binary's crate declared optional, behind a feature no
/// build turns on by default: the declaration alone is refused.
#[test]
fn an_optional_dependency_on_the_kbf_binary_is_refused() {
    let mut trees = clean();
    trees[2].declared.insert("kbf-cli".to_owned());
    assert_eq!(
        violations(&layers(), &trees, &members()),
        vec!["mdm-host declares kbf-cli, a crate of the farm".to_owned()]
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
        violations(&layers(), &trees, &members()),
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
        violations(&layers(), &trees, &members()),
        vec!["mdm-core links getrandom, which a pure crate may not".to_owned()]
    );
    let named = [Tree {
        name: "kbf-mdm-tool",
        linked: set(&["kbf-mdm-tool", "sha2"]),
        declared: BTreeSet::new(),
        dev: BTreeSet::new(),
        build: BTreeSet::new(),
    }];
    assert_eq!(
        violations(&layers(), &named, &members()),
        vec!["kbf-mdm-tool: an MDM crate is named without kbf".to_owned()]
    );
}

/// Catches a declaration that does not parse into the group, or one whose rows contradict each
/// other: each is refused with its reason, and the repository's own shape parses.
#[test]
fn a_broken_layers_toml_is_refused() {
    let g = layers();
    assert_eq!(
        g.may_link.keys().map(String::as_str).collect::<Vec<_>>(),
        ["host-step", "mdm-core", "mdm-host"]
    );
    let refused = |text: &str| group(text).expect_err("refused");
    assert!(refused("[mdm").starts_with("layers.toml: "));
    assert_eq!(refused("[other]\n"), "layers.toml has no [mdm] group");
    assert_eq!(refused("[mdm]\nfarm = []\n"), "[mdm] has no may_link table");
    assert_eq!(
        refused("[mdm.may_link]\nmdm-host = [\"kbf-proto\"]\n"),
        "mdm-host may link kbf-proto, which the group does not hold"
    );
    assert_eq!(
        refused("[mdm]\npure = [\"x\"]\n[mdm.may_link]\nmdm-core = []\n"),
        "pure names x, which the group does not hold"
    );
    assert_eq!(
        refused("[mdm]\nfarm = [\"mdm-core\"]\n[mdm.may_link]\nmdm-core = []\n"),
        "mdm-core is both in the group and in its farm"
    );
    assert_eq!(
        refused("[mdm]\nfarm = \"kbf-cli\"\n[mdm.may_link]\n"),
        "farm is not a list"
    );
    assert_eq!(
        refused("[mdm]\nfarm = [1]\n[mdm.may_link]\n"),
        "farm holds a non-string"
    );
}

/// Catches a crate that joins the layer without being registered: with `mdm-host` left out of
/// the declaration, its dependency on `mdm-core` is refused; the farm depending on a crate of the
/// group is refused the same way; and a registered crate that is not a workspace member is named.
#[test]
fn a_crate_missing_from_layers_toml_is_refused() {
    let direct: BTreeMap<String, BTreeSet<String>> = [
        ("mdm-core", set(&["sha2"])),
        ("host-step", set(&["rustix"])),
        ("mdm-host", set(&["mdm-core", "host-step"])),
        ("kbf-cli", set(&["kbf-server"])),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    assert_eq!(unregistered(&layers(), &direct), Vec::<String>::new());

    let without_host =
        group("[mdm]\nfarm = [\"kbf-cli\"]\n[mdm.may_link]\nmdm-core = []\nhost-step = []\n")
            .expect("parses");
    assert_eq!(
        unregistered(&without_host, &direct),
        vec![
            "mdm-host depends on host-step, a crate of the group, and is not registered in it"
                .to_owned(),
            "mdm-host depends on mdm-core, a crate of the group, and is not registered in it"
                .to_owned(),
        ]
    );

    let mut farm_links = direct.clone();
    farm_links
        .get_mut("kbf-cli")
        .expect("kbf-cli")
        .insert("mdm-host".to_owned());
    farm_links.remove("host-step");
    assert_eq!(
        unregistered(&layers(), &farm_links),
        vec![
            "host-step is in the group but not a member of the workspace".to_owned(),
            "kbf-cli depends on mdm-host, a crate of the group, and is not registered in it"
                .to_owned(),
        ]
    );
}
