//! No other family of the state store is read by a prefix that reaches the `exec/` rows
//! (`docs/design/n-frontends.md`, section 4.2), so a release that does not know them never
//! sees them (rule B8).
//!
//! The key families of the repository are the string constants that name a prefix, in the
//! crates that read the state store: a family is read by scanning its constant. A constant
//! that is a prefix of `exec/` (`e`, `ex`, `exec/`, ...) would make a scan return `exec/`
//! rows to a reader that cannot decode them.

use std::fs;
use std::path::{Path, PathBuf};

use kbf_server::executor::rows::{LIVE_PREFIX, OP_PREFIX, ROOT};

/// The crates that scan the state store (`kbf-state` in their manifests).
const CRATES: [&str; 15] = [
    "kbf-cli",
    "kbf-deploy",
    "kbf-enroll-registry",
    "kbf-front",
    "kbf-history",
    "kbf-installer",
    "kbf-pack",
    "kbf-pack-adopt",
    "kbf-pack-index",
    "kbf-server",
    "kbf-setup",
    "kbf-state-copy",
    "kbf-usage",
    "kbf-integration-test",
    "kbf-state",
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The text of every `const NAME: &str = "text";` of a file.
fn string_constants(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some((head, tail)) = line.split_once(": &str = \"") else {
            continue;
        };
        if !head.contains("const ") {
            continue;
        }
        if let Some((literal, _)) = tail.split_once("\";") {
            found.push(literal.to_owned());
        }
    }
    found
}

#[test]
fn no_scan_prefix_of_any_family_is_a_prefix_of_exec() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for name in CRATES {
        sources(&root.join(name).join("src"), &mut files);
    }
    let mut families = Vec::new();
    for file in files
        .iter()
        .filter(|f| !f.ends_with("kbf-server/src/executor/rows.rs"))
    {
        let text = fs::read_to_string(file).expect("source");
        families.extend(string_constants(&text));
    }
    for known in ["member/", "deny/", "unfilled/"] {
        assert!(
            families.iter().any(|f| f == known),
            "the constants of the families were not found: no {known:?} in {} constants",
            families.len()
        );
    }
    let reaching: Vec<_> = families
        .iter()
        .filter(|family| !family.is_empty() && ROOT.starts_with(family.as_str()))
        .collect();
    assert!(
        reaching.is_empty(),
        "a scan prefix reaches exec/: {reaching:?}"
    );
}

#[test]
fn the_row_families_lie_under_the_root() {
    for prefix in [LIVE_PREFIX, OP_PREFIX] {
        assert!(
            prefix.starts_with(ROOT) && prefix.ends_with('/'),
            "{prefix}"
        );
    }
    assert_eq!(ROOT, "exec/");
}
