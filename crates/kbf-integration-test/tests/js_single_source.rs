//! The workspace takes every `js-*` crate from one place.
//!
//! The scheduler libraries live in the jobscheduler repository and are taken by one git
//! revision (`[workspace.dependencies]` in the root `Cargo.toml`). Two sources for them
//! would build two copies of a shared type, and a lease id of one would not be the lease
//! id of the other. This test reads `Cargo.lock` and requires:
//!
//! - at least one `js-*` package (a renamed prefix or an empty lock would otherwise pass);
//! - each `js-*` package once, from a git source (not a path, not a registry);
//! - the same source, revision included, for all of them.

use std::collections::BTreeMap;

const REPOSITORY: &str = "git+https://github.com/komira-ai/jobscheduler?rev=";

/// Crates of crates.io whose names begin `js-` but that are not the scheduler's.
const NOT_OURS: &[&str] = &["js-sys"];

/// The `js-*` packages of a `Cargo.lock` as (name, source) pairs, in file order. A package
/// without a `source` line (a path crate) has the source `<none>`.
fn js_packages(lock: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for block in lock.split("[[package]]").skip(1) {
        let field = |key: &str| {
            let prefix = format!("{key} = \"");
            block
                .lines()
                .find_map(|l| l.strip_prefix(&prefix)?.strip_suffix('"'))
        };
        let Some(name) = field("name").filter(|n| n.starts_with("js-") && !NOT_OURS.contains(n))
        else {
            continue;
        };
        out.push((
            name.to_owned(),
            field("source").unwrap_or("<none>").to_owned(),
        ));
    }
    out
}

/// What is wrong with the `js-*` packages of `lock`; empty when it is as required.
fn problems(lock: &str) -> Vec<String> {
    let packages = js_packages(lock);
    let mut out = Vec::new();
    if packages.is_empty() {
        out.push("Cargo.lock holds no js-* package".to_owned());
    }
    let mut count: BTreeMap<&str, usize> = BTreeMap::new();
    for (name, source) in &packages {
        *count.entry(name).or_default() += 1;
        if !source.starts_with(REPOSITORY) {
            out.push(format!("{name} comes from {source}, not {REPOSITORY}<rev>"));
        }
    }
    for (name, n) in count.iter().filter(|(_, n)| **n > 1) {
        out.push(format!("{name} appears {n} times"));
    }
    let first = packages.first().map(|(_, source)| source);
    for (name, source) in &packages {
        if Some(source) != first {
            out.push(format!("{name} comes from {source}, others from {first:?}"));
        }
    }
    out
}

#[test]
fn every_js_crate_has_one_source_at_one_revision() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let problems = problems(&lock);
    assert!(problems.is_empty(), "js-* crates:\n{}", problems.join("\n"));
}

const GOOD: &str = r#"
[[package]]
name = "js-sizing"
version = "0.1.0"
source = "git+https://github.com/komira-ai/jobscheduler?rev=aaaa#aaaa"

[[package]]
name = "js-types"
version = "0.1.0"
source = "git+https://github.com/komira-ai/jobscheduler?rev=aaaa#aaaa"

[[package]]
name = "js-sys"
version = "0.3.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

#[test]
fn the_check_accepts_one_source_and_refuses_the_ways_to_have_two() {
    assert_eq!(problems(GOOD), Vec::<String>::new());

    let other_rev = GOOD.replacen("rev=aaaa#aaaa", "rev=bbbb#bbbb", 1);
    assert_eq!(problems(&other_rev).len(), 1, "{:?}", problems(&other_rev));

    let twice = format!(
        "{GOOD}\n[[package]]\nname = \"js-sizing\"\nversion = \"0.2.0\"\nsource = \"git+https://github.com/komira-ai/jobscheduler?rev=aaaa#aaaa\"\n"
    );
    assert!(
        problems(&twice)
            .iter()
            .any(|p| p.contains("appears 2 times"))
    );

    let path_crate = GOOD.replacen(
        "source = \"git+https://github.com/komira-ai/jobscheduler?rev=aaaa#aaaa\"\n",
        "",
        1,
    );
    assert!(problems(&path_crate).iter().any(|p| p.contains("<none>")));

    let elsewhere = GOOD.replace("komira-ai/jobscheduler", "someone/else");
    assert!(!problems(&elsewhere).is_empty());

    assert_eq!(problems("[[package]]\nname = \"serde\"\n").len(), 1);
}
