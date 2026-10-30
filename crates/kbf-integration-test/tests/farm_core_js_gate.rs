//! The `farm-core-js` feature of kbf-server stays out of every default feature set.
//!
//! The feature makes `SchedulerCore::under_test()` name the js core, so the farm tests of the
//! packages that reach it run on that core; both cores are in every build and
//! `--scheduler-core` picks the one a server runs. CI runs the fast tests once without the
//! feature and once with it and merges the two coverage runs, which only works while the
//! feature is turned on by a command line alone (`--features kbf-server/farm-core-js`). A
//! default list that carried it would run every test on the js core and leave the legacy
//! one to the tests that name it. This test reads every `Cargo.toml` of the
//! repository and requires:
//!
//! - kbf-server defines the feature (a renamed or removed feature would pass the rest
//!   vacuously);
//! - the only lines of any manifest that name it, comments aside, are the feature's own
//!   definition in kbf-server's manifest and the `kbf` binary's forwarding flag
//!   (`farm-core-js = ["kbf-server/farm-core-js"]`): it is in no `default` list, in no other
//!   feature's list, and in no dependency's `features` (the `kbf` binary's, the
//!   workspace's, a dev-dependency's), so no default or release feature set carries it.

use std::path::{Path, PathBuf};

const FEATURE: &str = "farm-core-js";

/// The manifest that defines the feature, as a path from the repository root.
const OWNER: &str = "crates/kbf-server/Cargo.toml";

/// The manifest of the `kbf` binary, which forwards the feature under the same name so a
/// build can ask for it with `-p kbf-cli --features farm-core-js`.
const FORWARDER: &str = "crates/kbf-cli/Cargo.toml";

/// Every `Cargo.toml` under `dir` as (path from `root`, text), in path order.
fn manifests(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if path.is_dir() {
            if !matches!(name, "target" | ".git" | "node_modules") {
                manifests(root, &path, out);
            }
        } else if name == "Cargo.toml" {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let relative = path.strip_prefix(root).expect("under the root");
            out.push((relative.to_string_lossy().replace('\\', "/"), text));
        }
    }
}

/// What is wrong with how `manifests` (path, text) name the feature; empty when it is as
/// required.
fn problems(manifests: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    let mut defined = false;
    for (path, text) in manifests {
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.starts_with('#') || !line.contains(FEATURE) {
                continue;
            }
            if path == OWNER && line.starts_with(&format!("{FEATURE} =")) {
                defined = true;
            } else if path == FORWARDER && line == format!("{FEATURE} = [\"kbf-server/{FEATURE}\"]")
            {
                // The `kbf` binary's own flag for the feature, off like the feature.
            } else {
                out.push(format!("{path}:{}: {line}", number + 1));
            }
        }
    }
    if !defined {
        out.push(format!("{OWNER} does not define {FEATURE}"));
    }
    out
}

fn is_clean(problems: &[String]) -> bool {
    problems.is_empty()
}

#[test]
fn no_manifest_turns_the_feature_on_for_a_default_or_release_build() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = root.canonicalize().expect("the repository root");
    let mut found = Vec::new();
    manifests(&root, &root, &mut found);
    assert!(
        found.len() > 10,
        "only {} manifests found under {}",
        found.len(),
        root.display()
    );
    let problems = problems(&found);
    assert!(
        is_clean(&problems),
        "{FEATURE} must be enabled by a command line alone:\n{}",
        problems.join("\n")
    );
}

fn manifest(path: &str, text: &str) -> (String, String) {
    (path.to_owned(), text.to_owned())
}

/// Catches the check passing on a tree that enables the feature: in a `default` list of
/// the owner, in another crate's dependency on kbf-server (the `kbf` binary's), in the
/// workspace dependency table, in another feature's list; and one that no longer defines it.
#[test]
fn the_check_refuses_each_way_to_turn_the_feature_on() {
    let owner = "[features]\ntestkit = []\n# farm-core-js is off\nfarm-core-js = []\n";
    let cli = "[dependencies]\nkbf-server.workspace = true\n";
    let clean = [
        manifest(OWNER, owner),
        manifest("crates/kbf-cli/Cargo.toml", cli),
    ];
    assert!(is_clean(&problems(&clean)), "{:?}", problems(&clean));

    let default_on = owner.replace("[features]\n", "[features]\ndefault = [\"farm-core-js\"]\n");
    let bad = [
        manifest(OWNER, &default_on),
        manifest("crates/kbf-cli/Cargo.toml", cli),
    ];
    assert_eq!(problems(&bad).len(), 1, "{:?}", problems(&bad));

    let in_cli =
        "[dependencies]\nkbf-server = { workspace = true, features = [\"farm-core-js\"] }\n";
    let bad = [
        manifest(OWNER, owner),
        manifest("crates/kbf-cli/Cargo.toml", in_cli),
    ];
    assert_eq!(problems(&bad).len(), 1, "{:?}", problems(&bad));

    let in_root = "[workspace.dependencies]\nkbf-server = { path = \"crates/kbf-server\", features = [\"farm-core-js\"] }\n";
    let bad = [manifest(OWNER, owner), manifest("Cargo.toml", in_root)];
    assert_eq!(problems(&bad).len(), 1, "{:?}", problems(&bad));

    let chained = owner.replace("testkit = []", "release = [\"farm-core-js\"]");
    let bad = [manifest(OWNER, &chained)];
    assert_eq!(problems(&bad).len(), 1, "{:?}", problems(&bad));

    let forward = "[features]\nfarm-core-js = [\"kbf-server/farm-core-js\"]\n";
    let ok = [manifest(OWNER, owner), manifest(FORWARDER, forward)];
    assert!(is_clean(&problems(&ok)), "{:?}", problems(&ok));
    let default_forward = format!("{forward}default = [\"farm-core-js\"]\n");
    let bad = [
        manifest(OWNER, owner),
        manifest(FORWARDER, &default_forward),
    ];
    assert_eq!(problems(&bad).len(), 1, "{:?}", problems(&bad));
    let elsewhere = [
        manifest(OWNER, owner),
        manifest("crates/kbf-node/Cargo.toml", forward),
    ];
    assert_eq!(problems(&elsewhere).len(), 1, "{:?}", problems(&elsewhere));

    let gone = [manifest(OWNER, "[features]\ntestkit = []\n")];
    assert_eq!(problems(&gone).len(), 1, "{:?}", problems(&gone));
}
