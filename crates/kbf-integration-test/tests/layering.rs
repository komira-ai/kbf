//! Workspace layering: the pure crates stay pure.
//!
//! The pure crates hold the farm's decision logic and must replay a simulation seed
//! exactly, so they may not reach an async runtime, the network or a random source.
//! This test reads the resolved dependency graph from `cargo metadata` and walks each
//! pure crate's normal (non-dev, non-build) dependencies transitively.
//!
//! Catches:
//! - a pure crate that depends, directly or through another crate, on a forbidden
//!   crate (tokio, tonic, hyper, reqwest, the rand family, getrandom, mio, socket2);
//! - a pure crate that depends on a kbf crate outside the pure set;
//! - a pure crate that reaches a crate taken from a git repository other than the ones
//!   listed in `PURE_GIT_CRATES` (today `js-sizing`, `js-caps`, `js-types` and `js-sim`, which
//!   `kbf-estimator`, `kbf-caps`, `kbf-types` and `kbf-sim` re-export; `js-sched`, which `kbf-sched` re-exports,
//!   and `js-whole`, which `js-sched` depends on);
//! - a pure crate whose `lib.rs` no longer denies the `clippy.toml` lists, which would
//!   silently switch off the clock, sleep, network and `HashMap` checks for it;
//! - a pure crate renamed or removed, which would otherwise make every check above
//!   pass vacuously;
//! - a pure module of an impure crate (`PURE_MODULES`) whose file no longer denies
//!   the `clippy.toml` lists, or that was moved or removed;
//! - the server and the worker daemon linked together outside the one crate meant to
//!   (`JOINERS`): `kbf-server` and `kbf-node` are libraries so a combined binary can
//!   run both in one process, and any other crate that links both, or either one into
//!   the other's side of the farm, makes the split meaningless;
//! - a `main.rs` of those two that grows past `THIN_MAIN_LINES` code lines, which
//!   means assembly that a combined binary could not reuse is back in a binary;
//! - the storage crate (`kbf-storage`) depending, directly or through another crate, on
//!   a frontend crate (`STORAGE_MUST_NOT_REACH`: the scheduler and authorization), which
//!   would make a storage process link the frontend; and the storage crate renamed or
//!   removed, which would make that check pass vacuously;
//! - the routing library (`kbf-route`) depending, directly or through another crate, on a
//!   frontend crate, the server, or the daemon's side (`ROUTE_MUST_NOT_REACH`): the
//!   frontend and the daemon both link it, and neither may pull in the other's code
//!   through it;
//! - a daemon crate (`DAEMON_ONLY`: `kbf-node`, the `kbf-daemon` binary's library, and
//!   `kbf-daemon` beneath it) depending, directly or through another crate, on a crate that
//!   holds the cold store behind its cache, or on a standalone HTTP client one could be
//!   written over (`COLD_STORE_CLIENT`); and a walk that cannot see those crates (renamed,
//!   or the server no longer reaching the front), which would make that check pass
//!   vacuously. A daemon-only node holds no cold-store credential and reaches bytes
//!   through storage (docs/design/architecture-v2.md, section 9). The one exception is
//!   `--pack-read` (`kbf_daemon::packed`): a daemon reads the packs of the farm's object
//!   store with a key that may read `cas/` and write nothing, in a file named by a flag,
//!   so the daemon crates now reach the object store client (`kbf-objstore`) and the HTTP
//!   client it is written over (`reqwest`), and the check no longer refuses them. What this
//!   pins is the daemon's code: the daemon crates cannot build the cold store of a
//!   frontend or a storage node. The deployed `kbf daemon` is the `kbf` binary
//!   (`kbf-cli`), which links `kbf-server` for its other modes (`JOINERS`). For that
//!   binary the protection is that it is given no credential in its environment: its unit
//!   loads no environment file or systemd credential (kbf-deploy's `linux/modes_tests.rs`),
//!   and no daemon flag names a store or reads an environment variable, but the seven
//!   `--pack-read` flags, which take a key file and never a secret (kbf-cli's tests);
//! - the front-door proxy library (`kbf-proxy`) depending, directly or through another crate,
//!   on a child's, the installer's or the runtime's crates (`PROXY_MUST_NOT_REACH`); and any
//!   file of `kbf-installer/src` outside `MAY_NAME_THE_PROXY`, the root helper first of all,
//!   naming `kbf_proxy` or `proxy_verb`: the root half shares the installer's binary with the proxy verb and
//!   nothing else;
//! - any file of the setup library (`kbf-setup/src`, which `kbf-installer` re-exports) outside
//!   `SETUP_MAY_NAME_THE_PROXY` naming `kbf_proxy`;
//! - the `kbf` binary (`kbf-cli`) linking the proxy: `kbf setup` is built on the setup library
//!   without its `proxy` feature (`kbf-installer` turns it on), so the tree of
//!   `cargo build -p kbf-cli` holds `kbf-setup` and no `kbf-proxy`;
//! - the packing client (`kbf-pack`) depending, directly or through another crate, on the
//!   frontend, the server, the storage node or the daemon (`PACK_MUST_NOT_REACH`); and a walk
//!   that cannot see `kbf-objstore` beneath it, which would make that check pass vacuously;
//! - the pack adoption tool (`kbf-pack-adopt`) depending, directly or through another crate, on
//!   a crate of `PACK_MUST_NOT_REACH`, so that the operator binary never links the frontend, the
//!   server or the daemon; and a walk that cannot see `kbf-pack` beneath it;
//! - the state store (`kbf-state`, a re-export of `js-state`) depending on any other crate of
//!   this workspace (with its test-database feature too, which brings `js-testkit` and not
//!   `kbf-testkit`), which would stop it from being extracted, and the pack index (`kbf-pack-index`) depending on any but the state store;
//!   each with a walk that cannot see what it should, which would make the check vacuous.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::process::Command;

use serde_json::Value;

const PURE: &[&str] = &[
    "kbf-types",
    "kbf-caps",
    "kbf-segments",
    "kbf-meta",
    "kbf-sched",
    "kbf-estimator",
    "kbf-raft",
    "kbf-invocation",
    "kbf-map",
];

/// The simulation kernel. It is not `PURE`: it reaches `rand_chacha` (seeded, never
/// seeded from the system), which the purity check refuses by name. Pure crates take it
/// as a dev-dependency only, so nothing in `PURE` reaches the `js-sim` it re-exports; the
/// check of `PURE_GIT_CRATES` walks it as well, so that its git crates are listed too.
const SIM_KERNEL: &[&str] = &["kbf-sim"];

/// Crates that do a little file I/O (so they are not `PURE`) but are shared by every CI/CD
/// stage and by xtask: they may reach no `FORBIDDEN` crate and no other kbf crate.
const LEAF: &[&str] = &["kbf-gate"];

const FORBIDDEN: &[&str] = &[
    "tokio",
    "tonic",
    "hyper",
    "reqwest",
    "rand",
    "rand_core",
    "rand_chacha",
    "getrandom",
    "mio",
    "socket2",
];

/// Modules of impure crates that hold state every server of a replicated farm must
/// hold alike, so they are kept pure as the pure crates are. Paths are from the
/// workspace's `crates` directory.
const PURE_MODULES: &[&str] = &["kbf-server/src/machine.rs"];

const DENY_ATTR: &str = "#![deny(clippy::disallowed_methods, clippy::disallowed_types)]";

/// The crates of other repositories (taken by `git` dependency, `source` beginning
/// `git+`) that a pure crate may reach, with the one repository each may come from.
/// `kbf-estimator` re-exports `js-sizing`, `kbf-caps` re-exports `js-caps`, `kbf-types`
/// re-exports `js-types` (its `sched` module), `kbf-sim` re-exports `js-sim` and `kbf-sched`
/// re-exports `js-sched`; `js-sched` depends on `js-whole`, the pure whole-node rules; they depend on
/// one another and on crates.io crates only. A
/// further git crate in a pure crate's tree is a decision, so it is listed here by name.
const PURE_GIT_CRATES: &[(&str, &str)] = &[
    (
        "js-sched",
        "git+https://github.com/komira-ai/jobscheduler?rev=",
    ),
    (
        "js-caps",
        "git+https://github.com/komira-ai/jobscheduler?rev=",
    ),
    (
        "js-sim",
        "git+https://github.com/komira-ai/jobscheduler?rev=",
    ),
    (
        "js-sizing",
        "git+https://github.com/komira-ai/jobscheduler?rev=",
    ),
    (
        "js-types",
        "git+https://github.com/komira-ai/jobscheduler?rev=",
    ),
    (
        "js-whole",
        "git+https://github.com/komira-ai/jobscheduler?rev=",
    ),
];

struct Graph {
    /// Package id to package name.
    names: BTreeMap<String, String>,
    /// Package id to the package's `source` (`None` for a workspace path crate).
    sources: BTreeMap<String, Option<String>>,
    /// Package id to the ids of its normal dependencies.
    normal: BTreeMap<String, Vec<String>>,
    /// Workspace member name to (package id, path of its library root).
    members: BTreeMap<String, (String, Option<String>)>,
}

fn load_graph() -> Graph {
    let cargo = std::env::var("CARGO").expect("CARGO is set when cargo runs a test");
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: Value = serde_json::from_slice(&out.stdout).expect("cargo metadata emits JSON");

    let mut names = BTreeMap::new();
    let mut sources = BTreeMap::new();
    for pkg in meta["packages"].as_array().expect("packages") {
        let id = pkg["id"].as_str().expect("package id").to_owned();
        sources.insert(id.clone(), pkg["source"].as_str().map(str::to_owned));
        names.insert(id, pkg["name"].as_str().expect("package name").to_owned());
    }

    let mut members = BTreeMap::new();
    let member_ids: BTreeSet<&str> = meta["workspace_members"]
        .as_array()
        .expect("workspace_members")
        .iter()
        .map(|v| v.as_str().expect("member id"))
        .collect();
    for pkg in meta["packages"].as_array().expect("packages") {
        let id = pkg["id"].as_str().expect("package id");
        if !member_ids.contains(id) {
            continue;
        }
        let lib = pkg["targets"]
            .as_array()
            .expect("targets")
            .iter()
            .find(|t| {
                t["kind"]
                    .as_array()
                    .is_some_and(|k| k.iter().any(|k| k == "lib"))
            })
            .map(|t| t["src_path"].as_str().expect("src_path").to_owned());
        let name = pkg["name"].as_str().expect("package name").to_owned();
        members.insert(name, (id.to_owned(), lib));
    }

    let mut normal = BTreeMap::new();
    for node in meta["resolve"]["nodes"].as_array().expect("resolve.nodes") {
        let deps = node["deps"]
            .as_array()
            .expect("deps")
            .iter()
            .filter(|d| {
                // A null kind is a normal dependency; "dev" and "build" are not linked
                // into the crate's library.
                d["dep_kinds"]
                    .as_array()
                    .expect("dep_kinds")
                    .iter()
                    .any(|k| k["kind"].is_null())
            })
            .map(|d| d["pkg"].as_str().expect("dep pkg").to_owned())
            .collect();
        normal.insert(node["id"].as_str().expect("node id").to_owned(), deps);
    }

    Graph {
        names,
        sources,
        normal,
        members,
    }
}

/// Every package reachable from `root` through normal edges, with the chain of names
/// that reaches it (for the failure message).
fn normal_closure(g: &Graph, root: &str) -> BTreeMap<String, String> {
    let mut seen = BTreeMap::new();
    let mut queue = VecDeque::from([(root.to_owned(), g.names[root].clone())]);
    while let Some((id, chain)) = queue.pop_front() {
        for dep in g.normal.get(&id).into_iter().flatten() {
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

#[test]
fn pure_crates_have_no_impure_dependencies() {
    let g = load_graph();
    let mut violations = Vec::new();
    for &pure in PURE {
        let (id, _) = g
            .members
            .get(pure)
            .unwrap_or_else(|| panic!("{pure} is not a workspace member"));
        for (dep, chain) in normal_closure(&g, id) {
            let name = g.names[&dep].as_str();
            let impure_kbf = g.members.contains_key(name) && !PURE.contains(&name);
            if FORBIDDEN.contains(&name) || impure_kbf {
                violations.push(chain);
            }
        }
    }
    assert!(
        violations.is_empty(),
        "pure crates reach impure crates:\n{}",
        violations.join("\n")
    );
}

#[test]
fn pure_crates_take_git_crates_only_from_the_list() {
    let g = load_graph();
    let mut violations = Vec::new();
    let mut seen = BTreeSet::new();
    for &pure in PURE.iter().chain(SIM_KERNEL) {
        let (id, _) = g
            .members
            .get(pure)
            .unwrap_or_else(|| panic!("{pure} is not a workspace member"));
        for (dep, chain) in normal_closure(&g, id) {
            let Some(source) = g.sources[&dep].as_deref().filter(|s| s.starts_with("git+")) else {
                continue;
            };
            let name = g.names[&dep].as_str();
            seen.insert(name.to_owned());
            let allowed = PURE_GIT_CRATES
                .iter()
                .any(|(n, prefix)| *n == name && source.starts_with(prefix));
            if !allowed {
                violations.push(format!("{chain} ({source})"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "pure crates reach git crates outside PURE_GIT_CRATES:\n{}",
        violations.join("\n")
    );
    // The list is not vacuous: each entry is reached through a shim crate.
    for (name, _) in PURE_GIT_CRATES {
        assert!(
            seen.contains(*name),
            "{name} is in PURE_GIT_CRATES but no pure crate reaches it"
        );
    }
}

#[test]
fn leaf_crates_reach_no_async_stack_and_no_other_kbf_crate() {
    let g = load_graph();
    let mut violations = Vec::new();
    for &leaf in LEAF {
        let (id, _) = g
            .members
            .get(leaf)
            .unwrap_or_else(|| panic!("{leaf} is not a workspace member"));
        for (dep, chain) in normal_closure(&g, id) {
            let name = g.names[&dep].as_str();
            if name != leaf && (FORBIDDEN.contains(&name) || g.members.contains_key(name)) {
                violations.push(chain);
            }
        }
    }
    assert!(
        violations.is_empty(),
        "leaf crates reach the async stack or other kbf crates:\n{}",
        violations.join("\n")
    );
}

#[test]
fn pure_crates_deny_the_clippy_lists() {
    let g = load_graph();
    for &pure in PURE {
        let (_, lib) = g
            .members
            .get(pure)
            .unwrap_or_else(|| panic!("{pure} is not a workspace member"));
        let lib = lib
            .as_ref()
            .unwrap_or_else(|| panic!("{pure} has no library target"));
        let src = std::fs::read_to_string(lib).unwrap_or_else(|e| panic!("read {lib}: {e}"));
        assert!(
            src.lines().any(|l| l.trim() == DENY_ATTR),
            "{pure} ({lib}) must carry `{DENY_ATTR}`"
        );
    }
}

#[test]
fn pure_modules_deny_the_clippy_lists() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for &module in PURE_MODULES {
        let path = crates.join(module);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            src.lines().any(|l| l.trim() == DENY_ATTR),
            "{module} must carry `{DENY_ATTR}`"
        );
    }
}

/// The crates allowed to depend on both the server and the node library: the combined
/// binary and the one `kbf` binary.
const JOINERS: &[&str] = &["kbf-storage-and-daemon", "kbf-cli"];

/// What the server library's normal dependencies must not reach: the daemon side. The
/// server stores and schedules; it never moves blobs as a client.
const DAEMON_SIDE: &[&str] = &[
    "kbf-node",
    "kbf-daemon",
    "kbf-transfer",
    "kbf-driver-container",
    "kbf-driver-native",
];

/// What the node library's normal dependencies must not reach: the server side.
const SERVER_SIDE: &[&str] = &["kbf-server", "kbf-front"];

/// Names in `names` that `root`'s normal closure contains, with the chain reaching each.
fn reaches(g: &Graph, root: &str, names: &[&str]) -> Vec<String> {
    let (id, _) = g
        .members
        .get(root)
        .unwrap_or_else(|| panic!("{root} is not a workspace member"));
    normal_closure(g, id)
        .into_iter()
        .filter(|(dep, _)| names.contains(&g.names[dep].as_str()))
        .map(|(_, chain)| chain)
        .collect()
}

#[test]
fn server_and_node_libraries_stay_apart() {
    let g = load_graph();
    let mut violations = Vec::new();
    for lib in ["kbf-server", "kbf-node"] {
        let (_, root) = &g.members[lib];
        assert!(root.is_some(), "{lib} must have a library target");
    }
    violations.extend(reaches(&g, "kbf-server", DAEMON_SIDE));
    violations.extend(reaches(&g, "kbf-node", SERVER_SIDE));
    for (member, (id, _)) in &g.members {
        if JOINERS.contains(&member.as_str()) {
            continue;
        }
        let linked: BTreeSet<&str> = normal_closure(&g, id)
            .keys()
            .map(|dep| g.names[dep].as_str())
            .collect();
        if linked.contains("kbf-server") && linked.contains("kbf-node") {
            violations.push(format!("{member} links both kbf-server and kbf-node"));
        }
    }
    assert!(
        violations.is_empty(),
        "the server and the node library are linked together outside {JOINERS:?}:\n{}",
        violations.join("\n")
    );
}

/// The most lines of code (not blank, not comment) a binary's `main.rs` may have.
const THIN_MAIN_LINES: usize = 40;

#[test]
fn the_server_and_daemon_binaries_are_thin() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for main in [
        "kbf-server/src/main.rs",
        "kbf-node/src/main.rs",
        "kbf-cli/src/main.rs",
    ] {
        let path = crates.join(main);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let code = src
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .count();
        assert!(
            code <= THIN_MAIN_LINES,
            "{main} has {code} code lines (at most {THIN_MAIN_LINES}): the assembly belongs \
             in the library, where a combined binary can call it"
        );
    }
}

/// What the storage crate's normal dependencies must not reach: the frontend's scheduler
/// and authorization (docs/design/architecture-v2.md, section 3). A storage process
/// stores; it neither schedules nor decides who may call.
const STORAGE_MUST_NOT_REACH: &[&str] = &["kbf-sched", "kbf-auth"];

#[test]
fn storage_reaches_no_frontend_crate() {
    let g = load_graph();
    let (_, root) = g
        .members
        .get("kbf-storage")
        .expect("kbf-storage is a workspace member");
    assert!(root.is_some(), "kbf-storage must have a library target");
    let violations = reaches(&g, "kbf-storage", STORAGE_MUST_NOT_REACH);
    assert!(
        violations.is_empty(),
        "kbf-storage reaches a frontend crate:\n{}",
        violations.join("\n")
    );
}

/// The crates that make or hold certificate authority: the farm CA (`kbf-ca`: the signer
/// and the issuing of every certificate) and the frontend (`kbf-server`, which holds the CA
/// and runs the enroll listener).
const CA_SIDE: &[&str] = &["kbf-ca", "kbf-server"];

/// The update coordinator and a host's updater take their certificates from the farm CA
/// and never hold it: the coordinator's certificate is made for it, and an updater makes its
/// key on the host and has a certificate signed for it (`docs/design/updates.md`, section
/// 5). Neither crate's normal dependencies may reach the CA or the frontend.
#[test]
fn the_coordinator_and_the_updater_hold_no_ca_code() {
    let g = load_graph();
    for member in ["kbf-coordinator", "kbf-updater", "kbf-enroll"] {
        let (_, root) = g.members.get(member).expect("a workspace member");
        assert!(root.is_some(), "{member} must have a library target");
        let violations = reaches(&g, member, CA_SIDE);
        assert!(
            violations.is_empty(),
            "{member} reaches the CA side:\n{}",
            violations.join("\n")
        );
    }
    assert!(
        !reaches(&g, "kbf-server", &["kbf-ca"]).is_empty(),
        "the frontend's closure must reach kbf-ca, or this walk proves nothing"
    );
}

/// A host's updater has no tie to the front-door proxy (`kbf-proxy`, which is being retired):
/// the frontend serves the updater's calls itself (`kbf_server::node_self`), and the roll no
/// longer drains a pool. The crate's normal closure holds no `kbf-proxy`, its manifest names
/// none (the dev-dependencies the closure walk does not see), and no source or test file of it
/// names the crate or its identity headers.
#[test]
fn the_updater_has_no_tie_to_the_proxy() {
    let g = load_graph();
    let (_, root) = g.members.get("kbf-updater").expect("a workspace member");
    assert!(root.is_some(), "kbf-updater must have a library target");
    let violations = reaches(&g, "kbf-updater", &["kbf-proxy"]);
    assert!(
        violations.is_empty(),
        "kbf-updater reaches the proxy:\n{}",
        violations.join("\n")
    );
    // The walk proves something: a crate that does link the proxy is seen to reach it.
    assert!(
        !reaches(&g, "kbf-installer", &["kbf-proxy"]).is_empty(),
        "the installer's closure must reach kbf-proxy, or this walk proves nothing"
    );

    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kbf-updater");
    let manifest = std::fs::read_to_string(base.join("Cargo.toml")).expect("the manifest");
    assert!(
        !manifest.contains("kbf-proxy") && !manifest.contains("kbf_proxy"),
        "kbf-updater's Cargo.toml names the proxy"
    );
    let mut files = Vec::new();
    rust_files(&base, &base.join("src"), &mut files);
    rust_files(&base, &base.join("tests"), &mut files);
    assert!(
        files.iter().any(|f| f == "src/agent/build.rs") && files.iter().any(|f| f == "src/lib.rs"),
        "kbf-updater's files were not found under {}",
        base.display()
    );
    for file in &files {
        let text =
            std::fs::read_to_string(base.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        for word in ["kbf-proxy", "kbf_proxy"] {
            assert!(!text.contains(word), "{file} names {word}");
        }
    }
}

/// The daemon crates: the daemon's code, which the `kbf-daemon` binary is and which
/// `kbf daemon` runs inside the `kbf` binary, with the drivers, which `kbf-node` names
/// and so are in its closure.
const DAEMON_ONLY: &[&str] = &["kbf-node", "kbf-daemon"];

/// What a daemon crate's normal dependencies must not reach: the crates that hold the cold
/// store behind their cache and build it from a cold-store URI (`kbf-front`, `kbf-server`),
/// and a standalone HTTP client an S3 client could be written over (`ureq`). The object
/// store client (`kbf-objstore`) and the `reqwest` it is written over are not listed since
/// `--pack-read`: a daemon reads packs with it (the module docs). `hyper-util` is not listed
/// either: tonic's channel, which the daemon dials the server with, is built on its client.
const COLD_STORE_CLIENT: &[&str] = &["kbf-front", "kbf-server", "ureq"];

#[test]
fn a_daemon_cannot_build_a_cold_store_client() {
    let g = load_graph();
    // Every name the check looks for is a package of the graph, and the walk sees the
    // crate where it is linked: the server holds the front, which builds the cold store.
    // Without these, a renamed or misspelled crate would pass the check below.
    for name in COLD_STORE_CLIENT {
        assert!(
            g.names.values().any(|n| n == *name),
            "{name} is not a package of the workspace's graph"
        );
    }
    assert!(
        !reaches(&g, "kbf-server", &["kbf-front"]).is_empty(),
        "the server's closure must reach kbf-front, or this walk proves nothing"
    );
    let violations: Vec<String> = DAEMON_ONLY
        .iter()
        .flat_map(|daemon| reaches(&g, daemon, COLD_STORE_CLIENT))
        .collect();
    assert!(
        violations.is_empty(),
        "a daemon crate reaches a crate that builds the cold store or a standalone HTTP \
         client (a daemon-only node holds no cold-store credential, \
         docs/design/architecture-v2.md section 9):\n{}",
        violations.join("\n")
    );
}

/// The daemon bins each lease's usage with the jobscheduler's standalone `js-agent-usage`
/// crate, which depends on nothing. The crates that read the same files and post them
/// (`js-agent`, `js-client`) are in no package of the workspace's graph: a daemon that took
/// `js-agent` would reach `ureq`, and `a_daemon_cannot_build_a_cold_store_client` would say
/// only that; this one says why.
#[test]
fn a_daemon_takes_the_usage_binner_from_js_agent_usage_alone() {
    let g = load_graph();
    assert!(
        !reaches(&g, "kbf-daemon", &["js-agent-usage"]).is_empty(),
        "kbf-daemon's closure must hold js-agent-usage"
    );
    for name in ["js-agent", "js-client"] {
        assert!(
            !g.names.values().any(|n| n == name),
            "{name} is a package of the workspace's graph"
        );
    }
}

/// What the routing library's normal dependencies must not reach: the frontend's scheduler
/// and authorization, the server, and the daemon's side (the transfer engine and the node).
/// The frontend and the daemon both use it (docs/design/architecture-v2.md, section 7); a
/// path to either side's code would make the other link it.
const ROUTE_MUST_NOT_REACH: &[&str] = &[
    "kbf-sched",
    "kbf-auth",
    "kbf-front",
    "kbf-server",
    "kbf-transfer",
    "kbf-node",
    "kbf-daemon",
];

#[test]
fn route_reaches_neither_the_frontend_nor_the_daemon() {
    let g = load_graph();
    let (_, root) = g
        .members
        .get("kbf-route")
        .expect("kbf-route is a workspace member");
    assert!(root.is_some(), "kbf-route must have a library target");
    let violations = reaches(&g, "kbf-route", ROUTE_MUST_NOT_REACH);
    assert!(
        violations.is_empty(),
        "kbf-route reaches a frontend or daemon crate:\n{}",
        violations.join("\n")
    );
}

/// What the packing client's normal dependencies must not reach: the frontend's scheduler and
/// authorization, the server and the storage node, and the daemon's side. A frontend and a
/// daemon (`--pack-read`) use the packing client; a path to either side's code would make the
/// other link it. Wiring it into the daemon changed `COLD_STORE_CLIENT`, as `kbf-pack` is
/// written over `kbf-objstore`.
const PACK_MUST_NOT_REACH: &[&str] = &[
    "kbf-sched",
    "kbf-auth",
    "kbf-front",
    "kbf-server",
    "kbf-storage",
    "kbf-transfer",
    "kbf-node",
    "kbf-daemon",
    "kbf-cli",
];

/// Asserts that the packing client (`kbf-pack`) reaches neither the frontend nor the daemon,
/// and that the walk sees the object store client it is written over, so the check does not
/// pass on an empty walk.
#[test]
fn pack_reaches_neither_the_frontend_nor_the_daemon() {
    let g = load_graph();
    let (_, root) = g
        .members
        .get("kbf-pack")
        .expect("kbf-pack is a workspace member");
    assert!(root.is_some(), "kbf-pack must have a library target");
    assert!(
        !reaches(&g, "kbf-pack", &["kbf-objstore"]).is_empty(),
        "kbf-pack's closure must reach kbf-objstore, or this walk proves nothing"
    );
    let violations = reaches(&g, "kbf-pack", PACK_MUST_NOT_REACH);
    assert!(
        violations.is_empty(),
        "kbf-pack reaches a frontend or daemon crate:\n{}",
        violations.join("\n")
    );
}

/// Asserts that the pack adoption tool (`kbf-pack-adopt`), an operator binary of its own,
/// reaches none of the crates the packing client may not (`PACK_MUST_NOT_REACH`), and that the
/// walk sees the packing client beneath it, so the check does not pass on an empty walk.
#[test]
fn pack_adopt_reaches_neither_the_frontend_nor_the_daemon() {
    let g = load_graph();
    let (_, root) = g
        .members
        .get("kbf-pack-adopt")
        .expect("kbf-pack-adopt is a workspace member");
    assert!(root.is_some(), "kbf-pack-adopt must have a library target");
    assert!(
        !reaches(&g, "kbf-pack-adopt", &["kbf-pack"]).is_empty(),
        "kbf-pack-adopt's closure must reach kbf-pack, or this walk proves nothing"
    );
    let violations = reaches(&g, "kbf-pack-adopt", PACK_MUST_NOT_REACH);
    assert!(
        violations.is_empty(),
        "kbf-pack-adopt reaches a frontend or daemon crate:\n{}",
        violations.join("\n")
    );
}

/// The workspace crates among `linked`, other than those in `allowed`.
fn workspace_crates_beyond<'a>(
    g: &'a Graph,
    linked: &BTreeSet<String>,
    allowed: &[&str],
) -> Vec<&'a str> {
    g.members
        .keys()
        .map(String::as_str)
        .filter(|name| linked.contains(*name) && !allowed.contains(name))
        .collect()
}

/// Asserts that the state store (`kbf-state`) links no other crate of this workspace, with
/// the Postgres backend on, and with its test-database feature on (which brings the test
/// server's crate, `js-testkit`, from the jobscheduler repository, and not `kbf-testkit`),
/// and that the walk sees its dependencies (it reaches `postgres`), so the check does not
/// pass on an empty walk. The store is generic: what is specific to a kbf
/// build (the pack index, `kbf-pack-index`) depends on it and never the other way, which is
/// what lets the store be extracted. It reads the tree of the crate built alone.
#[test]
fn the_state_store_links_no_build_specific_crate() {
    let g = load_graph();
    let (_, root) = g
        .members
        .get("kbf-state")
        .expect("kbf-state is a workspace member");
    assert!(root.is_some(), "kbf-state must have a library target");
    let alone = linked_with("kbf-state", "postgres");
    assert!(
        alone.contains("postgres"),
        "kbf-state's tree must hold postgres, or this walk proves nothing: {alone:?}"
    );
    let violations = workspace_crates_beyond(&g, &alone, &["kbf-state"]);
    assert!(
        violations.is_empty(),
        "kbf-state links another crate of this workspace: {violations:?}"
    );
    let testing = linked_with("kbf-state", "test-database");
    assert!(
        testing.contains("js-testkit"),
        "test-database must bring the test server's crate: {testing:?}"
    );
    let violations = workspace_crates_beyond(&g, &testing, &["kbf-state"]);
    assert!(
        violations.is_empty(),
        "kbf-state with test-database links another crate of this workspace: {violations:?}"
    );
}

/// Asserts that the pack index (`kbf-pack-index`) links no workspace crate but the state
/// store it keeps its tables in, with its Postgres feature on, so the frontend, the daemon,
/// the packing client and the adoption tool can all depend on it without a cycle, and that
/// the walk sees the state store beneath it, so the check does not pass on an empty walk.
#[test]
fn the_pack_index_links_only_the_state_store() {
    let g = load_graph();
    let (_, root) = g
        .members
        .get("kbf-pack-index")
        .expect("kbf-pack-index is a workspace member");
    assert!(root.is_some(), "kbf-pack-index must have a library target");
    let linked = linked_with("kbf-pack-index", "postgres");
    assert!(
        linked.contains("kbf-state"),
        "kbf-pack-index's tree must hold kbf-state, or this walk proves nothing: {linked:?}"
    );
    let violations = workspace_crates_beyond(&g, &linked, &["kbf-pack-index", "kbf-state"]);
    assert!(
        violations.is_empty(),
        "kbf-pack-index links another crate of this workspace: {violations:?}"
    );
}

/// What the front-door proxy library's normal dependencies must not reach
/// (docs/design/kbf-proxy-api.md, sections 1 and 11). The proxy routes bytes: it never
/// decodes a child's protocol (`kbf-proto`), runs none of the frontend, storage or node code
/// (`kbf-server`, `kbf-front`, `kbf-storage`, `kbf-node`, `kbf-daemon`, the binaries
/// `kbf-cli` and `kbf-storage-and-daemon`), and does not know the installer, its host plan or its
/// update types (`kbf-installer`, `kbf-setup`, `kbf-host`, `kbf-update`, `kbf-deploy`): the
/// admin calls to a child are the installer's, through a trait that lives there (the proxy
/// makes none), and the installer and the setup library depend on the proxy, not the other way
/// round.
const PROXY_MUST_NOT_REACH: &[&str] = &[
    "kbf-installer",
    "kbf-setup",
    "kbf-host",
    "kbf-update",
    "kbf-deploy",
    "kbf-server",
    "kbf-front",
    "kbf-storage",
    "kbf-node",
    "kbf-daemon",
    "kbf-cli",
    "kbf-storage-and-daemon",
    "kbf-proto",
];

#[test]
fn the_proxy_reaches_no_child_installer_or_runtime_crate() {
    let g = load_graph();
    // Every name is a package of the graph, and the walk sees the edge it guards: the
    // installer's closure reaches the proxy. A renamed crate, or a walk that cannot see the
    // proxy, would otherwise pass the check below with nothing in it.
    for name in PROXY_MUST_NOT_REACH {
        assert!(
            g.names.values().any(|n| n == *name),
            "{name} is not a package of the workspace's graph"
        );
    }
    let (_, root) = g
        .members
        .get("kbf-proxy")
        .expect("kbf-proxy is a workspace member");
    assert!(root.is_some(), "kbf-proxy must have a library target");
    assert!(
        !reaches(&g, "kbf-installer", &["kbf-proxy"]).is_empty(),
        "the installer's closure must reach kbf-proxy, or this walk proves nothing"
    );
    let violations = reaches(&g, "kbf-proxy", PROXY_MUST_NOT_REACH);
    assert!(
        violations.is_empty(),
        "kbf-proxy reaches a child, installer or runtime crate:\n{}",
        violations.join("\n")
    );
}

/// The files of `kbf-installer/src` that may name the proxy: the verb that hosts it and the
/// verb's pool mode (`proxy_pool.rs`, which binds and runs the proxy over frontends that run on
/// their own). Everything else, the root helper (`helper/`, `run.rs`) first of all, runs as root
/// and links a network stack it never runs if it names a proxy type. The settings file whose
/// `[proxy]` table the verb reads is in `kbf-setup` (`SETUP_MAY_NAME_THE_PROXY`).
const MAY_NAME_THE_PROXY: &[&str] = &["proxy_pool.rs", "proxy_verb.rs"];

/// The files of `kbf-setup/src` that may name the proxy: the settings file whose `[proxy]`
/// table the proxy verb reads and `setup` keeps as it is (`installer_toml.rs`). `setup` runs as
/// root; the rest of the library names no proxy type, the proxy's flags (`setup/proxy.rs`, taken
/// only to be refused) included. The table is behind the library's `proxy` feature, which `kbf
/// setup` (the `kbf` binary) does not turn on.
const SETUP_MAY_NAME_THE_PROXY: &[&str] = &["installer_toml.rs"];

/// The files that may name the verb's module (`proxy_verb`): the dispatch and its tests, and the
/// verb's pool mode (`proxy_pool.rs`, which shares the verb's stop flag and signal handling). A
/// path into the verb from anywhere else, `crate::proxy_verb::FIRST_FD` in the root helper for
/// one, reaches the proxy without ever writing `kbf_proxy`.
const MAY_NAME_THE_VERB: &[&str] = &[
    "app.rs",
    "app_tests.rs",
    "lib.rs",
    "proxy_pool.rs",
    "proxy_verb.rs",
];

/// Source files under `dir`, as paths relative to `base`, with `/` as the separator.
fn rust_files(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            rust_files(base, &path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let relative = path.strip_prefix(base).expect("under base");
            out.push(relative.to_string_lossy().into_owned());
        }
    }
}

#[test]
fn the_root_half_of_the_installer_names_nothing_of_the_proxy() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kbf-installer/src");
    let mut files = Vec::new();
    rust_files(&src, &src, &mut files);
    // The scan sees the root helper, and the allowed files still name the proxy: a moved
    // directory or a renamed verb would otherwise make the check pass with nothing in it.
    assert!(
        files.iter().any(|f| f.starts_with("helper/")) && files.iter().any(|f| f == "run.rs"),
        "the root helper's files were not found under {}",
        src.display()
    );
    for allowed in MAY_NAME_THE_PROXY {
        let text = std::fs::read_to_string(src.join(allowed))
            .unwrap_or_else(|e| panic!("read {allowed}: {e}"));
        assert!(
            text.contains("kbf_proxy"),
            "{allowed} no longer names kbf_proxy"
        );
    }
    for allowed in MAY_NAME_THE_VERB {
        let text = std::fs::read_to_string(src.join(allowed))
            .unwrap_or_else(|e| panic!("read {allowed}: {e}"));
        assert!(
            text.contains("proxy_verb"),
            "{allowed} no longer names proxy_verb"
        );
    }
    let reaches_verb: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .filter(|f| !MAY_NAME_THE_VERB.contains(f))
        .filter(|f| {
            std::fs::read_to_string(src.join(f))
                .unwrap_or_else(|e| panic!("read {f}: {e}"))
                .contains("proxy_verb")
        })
        .collect();
    assert!(
        reaches_verb.is_empty(),
        "these installer files name proxy_verb, but only {MAY_NAME_THE_VERB:?} may (a path into \
         the verb is a path to the proxy): {reaches_verb:?}"
    );
    let violations: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .filter(|f| !MAY_NAME_THE_PROXY.contains(f))
        .filter(|f| {
            std::fs::read_to_string(src.join(f))
                .unwrap_or_else(|e| panic!("read {f}: {e}"))
                .contains("kbf_proxy")
        })
        .collect();
    assert!(
        violations.is_empty(),
        "these installer files name kbf_proxy, but only {MAY_NAME_THE_PROXY:?} may (the root \
         half shares the binary and nothing else): {violations:?}"
    );
}

/// Asserts that, of the files of `kbf-setup/src`, only those in `SETUP_MAY_NAME_THE_PROXY` name
/// `kbf_proxy`, that each of those still does, and that the scan found the library's files.
#[test]
fn the_setup_library_names_the_proxy_only_where_it_keeps_the_table() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kbf-setup/src");
    let mut files = Vec::new();
    rust_files(&src, &src, &mut files);
    // The scan sees the library, and the allowed files still name the proxy: a moved
    // directory or a renamed module would otherwise make the check pass with nothing in it.
    assert!(
        files.iter().any(|f| f == "setup/mod.rs"),
        "the setup library's files were not found under {}",
        src.display()
    );
    for allowed in SETUP_MAY_NAME_THE_PROXY {
        let text = std::fs::read_to_string(src.join(allowed))
            .unwrap_or_else(|e| panic!("read {allowed}: {e}"));
        assert!(
            text.contains("kbf_proxy"),
            "{allowed} no longer names kbf_proxy"
        );
    }
    let violations: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .filter(|f| !SETUP_MAY_NAME_THE_PROXY.contains(f))
        .filter(|f| {
            std::fs::read_to_string(src.join(f))
                .unwrap_or_else(|e| panic!("read {f}: {e}"))
                .contains("kbf_proxy")
        })
        .collect();
    assert!(
        violations.is_empty(),
        "these setup files name kbf_proxy, but only {SETUP_MAY_NAME_THE_PROXY:?} may: \
         {violations:?}"
    );
}

/// The names of the crates `cargo build -p <package>` links, from `cargo tree` over the normal
/// edges. Unlike the `cargo metadata` graph above, whose features are those of the whole
/// workspace (the installer turns the setup library's `proxy` feature on for everyone), this
/// resolves the features of the one package, as building it alone does.
fn linked_by(package: &str) -> BTreeSet<String> {
    tree(package, &[])
}

/// As [`linked_by`], with these features of the package on (`cargo tree --features`).
fn linked_with(package: &str, features: &str) -> BTreeSet<String> {
    tree(package, &["--features", features])
}

/// The names `cargo tree` prints for the normal edges of `package`, with these extra arguments.
fn tree(package: &str, extra: &[&str]) -> BTreeSet<String> {
    let cargo = std::env::var("CARGO").expect("CARGO is set when cargo runs a test");
    let out = Command::new(cargo)
        .args(["tree", "--locked", "--edges", "normal", "--prefix", "none"])
        .args(["--format", "{p}", "--package", package])
        .args(extra)
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

/// Asserts that the tree of `cargo build -p kbf-cli` holds the setup library (`kbf setup` is
/// built on it) and neither the proxy nor the installer: the `kbf` binary takes the library
/// without its `proxy` feature. The installer's tree does hold the proxy, so the walk can see
/// the edge it guards.
#[test]
fn the_kbf_binary_links_the_setup_library_and_not_the_proxy() {
    let kbf = linked_by("kbf-cli");
    assert!(
        kbf.contains("kbf-setup"),
        "kbf-cli's tree must hold kbf-setup, or this walk proves nothing: {kbf:?}"
    );
    let installer = linked_by("kbf-installer");
    for name in ["kbf-setup", "kbf-proxy"] {
        assert!(
            installer.contains(name),
            "the installer's tree must hold {name}: {installer:?}"
        );
    }
    let reached: Vec<&str> = ["kbf-proxy", "kbf-installer"]
        .into_iter()
        .filter(|name| kbf.contains(*name))
        .collect();
    assert!(
        reached.is_empty(),
        "the kbf binary links {reached:?}: kbf-cli must depend on kbf-setup without its \
         `proxy` feature, and on no crate that turns it on"
    );
}

/// Asserts that the release workflow builds the `kbf` binary in a `cargo build` of its own: no
/// build line that names `kbf-cli` also names `kbf-installer` or `kbf-deploy` (which depends on
/// it). Built in one command with either, cargo would build the setup library once for both,
/// with the installer's `proxy` feature, and `kbf` would link the proxy whatever its own tree
/// says. At least one line builds `kbf-cli`, or the check proves nothing.
#[test]
fn the_release_workflow_builds_the_kbf_binary_apart_from_the_installer() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.github/workflows/artifacts.yml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let builds_kbf: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter(|line| line.contains("cargo build"))
        .filter(|line| line.contains("-p kbf-cli"))
        .collect();
    assert!(!builds_kbf.is_empty(), "no line of {path:?} builds kbf-cli");
    let together: Vec<&&str> = builds_kbf
        .iter()
        .filter(|line| {
            ["kbf-installer", "kbf-deploy"]
                .iter()
                .any(|n| line.contains(n))
        })
        .collect();
    assert!(
        together.is_empty(),
        "kbf-cli is built with the installer, which turns the setup library's `proxy` feature \
         on for it: {together:?}"
    );
}

/// What the lease server client's normal dependencies must not reach: another HTTP stack
/// (`reqwest`, which the object store uses, and `ureq`, which the daemons must not link), and
/// the jobscheduler's own agent and client crates, whose names the workspace refuses. The
/// client is written over hyper and tokio-rustls alone, with the one CA it is given as its
/// only trust anchor; a second stack beside it would be a second place the token could go
/// through.
const LEASE_CLIENT_MUST_NOT_REACH: &[&str] = &["reqwest", "ureq", "js-agent", "js-client"];

/// The crates that must never reach the lease server client: the daemon side
/// (`DAEMON_SIDE`), the VM driver and the guest agent (which run untrusted builds), the
/// Mac session, and the updater and installer that run on nodes. `kbf-storage-and-daemon` is
/// not here: it links the server library, which is where the executor that holds the token
/// lives, and the server's own closure is what the lease client is for.
const LEASE_CLIENT_FORBIDDEN_ROOTS: &[&str] = &[
    "kbf-node",
    "kbf-daemon",
    "kbf-transfer",
    "kbf-driver-container",
    "kbf-driver-native",
    "kbf-driver-vm",
    "kbf-guest",
    "kbf-mac-session",
    "kbf-updater",
    "kbf-installer",
];

/// Asserts that no node-side crate reaches the lease server client: the executor that
/// leases hosts runs in the server, and a daemon, a VM guest or an installer holding the
/// lease token or the means to call the lease server would let a compromised node or a build
/// lease and run work on other hosts. The roots are `LEASE_CLIENT_FORBIDDEN_ROOTS`, and the
/// walk is shown to see the client's own dependencies, so the check does not pass on an
/// empty walk.
#[test]
fn a_daemon_cannot_reach_the_lease_client() {
    let g = load_graph();
    assert!(
        g.names.values().any(|n| n == "kbf-lease-client"),
        "kbf-lease-client is not a package of the workspace's graph"
    );
    assert!(
        !reaches(&g, "kbf-lease-client", &["hyper", "tokio-rustls"]).is_empty(),
        "the lease client's closure must reach hyper and tokio-rustls, or this walk proves nothing"
    );
    let violations: Vec<String> = LEASE_CLIENT_FORBIDDEN_ROOTS
        .iter()
        .flat_map(|root| reaches(&g, root, &["kbf-lease-client"]))
        .collect();
    assert!(
        violations.is_empty(),
        "a node-side crate reaches the lease server client:\n{}",
        violations.join("\n")
    );
}

/// Asserts that the lease server client depends on no other kbf crate and on no second HTTP
/// stack (`LEASE_CLIENT_MUST_NOT_REACH`), so it can be linked anywhere the server can and
/// brings nothing else with it.
#[test]
fn the_lease_client_is_a_leaf_over_hyper_and_tokio_rustls() {
    let g = load_graph();
    let (id, root) = &g.members["kbf-lease-client"];
    assert!(
        root.is_some(),
        "kbf-lease-client must have a library target"
    );
    let kbf_deps: Vec<String> = normal_closure(&g, id)
        .into_iter()
        .filter(|(dep, _)| g.names[dep].starts_with("kbf-"))
        .map(|(_, chain)| chain)
        .collect();
    assert!(
        kbf_deps.is_empty(),
        "the lease client depends on a kbf crate:\n{}",
        kbf_deps.join("\n")
    );
    let violations = reaches(&g, "kbf-lease-client", LEASE_CLIENT_MUST_NOT_REACH);
    assert!(
        violations.is_empty(),
        "the lease client reaches another HTTP stack or a jobscheduler agent crate:\n{}",
        violations.join("\n")
    );
}
