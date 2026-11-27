//! Every crate of the tierstore repository in the resolved graph comes from one revision, in
//! the layering of `tests/layering.rs`. Two revisions would build two copies of tierstore's
//! types, and the pin that names the tierstore the farm runs would not be the one kbf's code
//! is built against. The crates the workspace pins directly ([`TIERSTORE_CRATES`]) must be
//! taken from that repository and be in the graph, so the check cannot pass vacuously.

use std::collections::BTreeSet;

use super::{Graph, load_graph};

/// The repository every tierstore crate is taken from, by one revision.
const REPOSITORY: &str = "git+https://github.com/komira-ai/tierstore?rev=";

/// The crates the root `Cargo.toml` takes from the tierstore repository: each must be in the
/// graph and come from it.
const TIERSTORE_CRATES: [&str; 3] = ["objstore-s3", "tierstore", "tierstore-list"];

/// Whether `source` is the tierstore repository.
fn from_tierstore(source: Option<&str>) -> bool {
    source.is_some_and(|source| source.starts_with(REPOSITORY))
}

/// What is wrong with the tierstore crates of `g`: crates of the tierstore repository at more
/// than one source, a crate of [`TIERSTORE_CRATES`] missing or taken from somewhere else;
/// empty when they are as required.
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

/// Asserts that every crate of the tierstore repository in the workspace's resolved graph
/// comes from one revision, and that each crate the workspace pins from it is there.
#[test]
fn every_tierstore_crate_comes_from_one_revision() {
    let problems = tierstore_problems(&load_graph());
    assert!(
        problems.is_empty(),
        "tierstore crates:\n{}",
        problems.join("\n")
    );
}

/// A graph of `packages` (id, name, source), with no edges.
fn graph(packages: &[(&str, &str, Option<&str>)]) -> Graph {
    Graph {
        names: packages
            .iter()
            .map(|(id, name, _)| ((*id).to_owned(), (*name).to_owned()))
            .collect(),
        sources: packages
            .iter()
            .map(|(id, _, source)| ((*id).to_owned(), source.map(str::to_owned)))
            .collect(),
        normal: std::collections::BTreeMap::new(),
        members: std::collections::BTreeMap::new(),
    }
}

const REV_A: &str = "git+https://github.com/komira-ai/tierstore?rev=aaaa#aaaa";
const REV_B: &str = "git+https://github.com/komira-ai/tierstore?rev=bbbb#bbbb";

/// Asserts the one-revision check passes one source for every tierstore crate (a crate of
/// the repository the workspace does not pin, here `tierstore-fetch-api`, included), and names
/// a second revision, even of a crate the workspace does not pin, a pinned crate taken from
/// elsewhere (a registry, or a path), and a pinned crate missing from the graph.
#[test]
fn a_second_tierstore_revision_or_source_is_named() {
    let crates: [(&str, &str, Option<&str>); 4] = [
        ("o", "objstore-s3", Some(REV_A)),
        ("t", "tierstore", Some(REV_A)),
        ("l", "tierstore-list", Some(REV_A)),
        ("f", "tierstore-fetch-api", Some(REV_A)),
    ];
    assert_eq!(tierstore_problems(&graph(&crates)), Vec::<String>::new());
    let mut two = crates;
    two[3].2 = Some(REV_B);
    let problems = tierstore_problems(&graph(&two));
    assert_eq!(
        problems,
        [format!(
            "the tierstore crates come from {{{REV_A:?}, {REV_B:?}}}"
        )]
    );
    let mut elsewhere = crates;
    elsewhere[0].2 = Some("registry+https://github.com/rust-lang/crates.io-index");
    elsewhere[1].2 = None;
    let problems = tierstore_problems(&graph(&elsewhere));
    assert_eq!(
        problems,
        [
            "objstore-s3 comes from Some(\"registry+https://github.com/rust-lang/crates.io-index\"), not git+https://github.com/komira-ai/tierstore?rev=<rev>",
            "tierstore comes from None, not git+https://github.com/komira-ai/tierstore?rev=<rev>"
        ]
    );
    let missing = tierstore_problems(&graph(&crates[1..]));
    assert_eq!(missing, ["objstore-s3 is not in the graph"]);
}
