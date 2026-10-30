//! No darwin `kbf-task` asset or image ships until a Mac confine exists.
//!
//! Where no confine is built, `kbf-task` refuses a Command that is to be denied the network
//! (`crates/kbf-task/src/network.rs`, exit 15), so a darwin `kbf-task` could run only Commands
//! that are allowed the network. Nothing may package one as a release asset or an image until
//! the Mac confine lands (the Mac confine change, M7), which lifts this lint together with the
//! refusal for the platform that has the confine.
//!
//! This is a text tripwire, not the backstop. The backstop is the pin of `release_artifacts`: the
//! asset table and the workflow must agree, so a darwin `kbf-task` asset needs an edit of
//! docs/artifacts.md, which these rules catch. The lint cannot read a build that names nothing
//! (a script, an action) and says so by being a text match.
//!
//! The rules, over `.github/workflows/*.yml` and `docs/artifacts.md`:
//! - no line names `kbf-task` and `darwin` together (`kbf-task-<commit>-darwin-arm64.tar.gz`, a
//!   `--platform darwin-arm64` package line, a release asset list);
//! - no line of a job that runs on a Mac (its name has `darwin`, or `runs-on` or a matrix names
//!   `macos-*`) names `kbf-task`: the build of it, its SBOM, its package or its image;
//! - no line of such a job, or naming an `apple-darwin` target, runs `cargo build`, `test`,
//!   `nextest`, `check` or `clippy` with `--workspace` or `--all` and without `--exclude kbf-task`
//!   (it would build `kbf-task` without naming it);
//! - no line naming `kbf-task` interpolates a matrix, env or input value (a name the text cannot
//!   resolve);
//! - no workflow names the feature `unconfined-for-tests`, which lets a Command run with the
//!   network where no confine exists and is for developers' test runs only.

use std::path::{Path, PathBuf};

/// The change that lifts the lint: the Mac confine.
const LIFTED_BY: &str = "the Mac confine (M7)";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

/// Whether the job runs on a Mac (its name has `darwin`, a `runs-on` or matrix names `macos`).
/// A Linux job that cross-checks for `apple-darwin` is not one.
fn is_mac_runner_job(header: &str, body: &[&str]) -> bool {
    header.contains("darwin")
        || body.iter().any(|l| {
            let l = l.trim();
            (l.starts_with("runs-on:") && l.contains("macos"))
                || (l.contains("macos-")
                    && (l.starts_with("os:") || l.starts_with("- ") || l.starts_with('[')))
        })
}

/// Whether `line` has a `${{ }}` that names a matrix or an env value (`${{ github.sha }}` and the
/// like do not pick a platform).
fn interpolates_a_platform(line: &str) -> bool {
    line.split("${{").skip(1).any(|rest| {
        let inner = rest.split("}}").next().unwrap_or(rest);
        inner.contains("matrix") || inner.contains("env.") || inner.contains("inputs.")
    })
}

/// Whether `line` runs a cargo verb over the whole workspace and does not exclude `kbf-task`.
fn builds_workspace(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    let verb = ["build", "test", "nextest", "check", "clippy", "llvm-cov"];
    words.contains(&"cargo")
        && words.iter().any(|w| verb.contains(w))
        && words.iter().any(|w| *w == "--workspace" || *w == "--all")
        && !line.contains("--exclude kbf-task")
}

/// Every way `files` (name and text) ship a darwin `kbf-task` or turn on the test feature.
fn problems(files: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for (name, text) in files {
        let lines: Vec<&str> = text.lines().collect();
        let workflow = name.starts_with(".github/workflows/");
        for (i, line) in lines.iter().enumerate() {
            let at = format!("{name}:{}", i + 1);
            if line.contains("kbf-task") && line.contains("darwin") {
                out.push(format!(
                    "{at}: names a darwin kbf-task, which cannot ship until {LIFTED_BY}: {}",
                    line.trim()
                ));
            }
            if workflow && line.contains("kbf-task") && interpolates_a_platform(line) {
                out.push(format!(
                    "{at}: a line naming kbf-task has an interpolation the lint cannot resolve: {}",
                    line.trim()
                ));
            }
            if workflow && line.contains("unconfined-for-tests") {
                out.push(format!(
                    "{at}: a workflow names the test-only feature `unconfined-for-tests`: {}",
                    line.trim()
                ));
            }
        }
        if !workflow {
            continue;
        }
        let mut jobs: Vec<(usize, &str)> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let key = line.strip_prefix("  ").filter(|r| !r.starts_with(' '));
            if let Some(key) = key.filter(|k| k.ends_with(':') && !k.starts_with('#')) {
                jobs.push((i, key));
            }
        }
        for (n, &(start, header)) in jobs.iter().enumerate() {
            let end = jobs.get(n + 1).map_or(lines.len(), |j| j.0);
            let body = &lines[start + 1..end];
            if !is_mac_runner_job(header, body) {
                continue;
            }
            for (k, line) in body.iter().enumerate() {
                let to_darwin = line.contains("apple-darwin");
                if builds_workspace(line) && (to_darwin || is_mac_runner_job(header, body)) {
                    out.push(format!(
                        "{name}:{}: the darwin job `{}` builds the whole workspace, kbf-task included, without `--exclude kbf-task`: {}",
                        start + k + 2,
                        header.trim_end_matches(':'),
                        line.trim()
                    ));
                }
                if line.contains("kbf-task")
                    && !line.contains("darwin")
                    && !line.contains("--exclude kbf-task")
                {
                    out.push(format!(
                        "{name}:{}: the darwin job `{}` names kbf-task, which cannot ship until {LIFTED_BY}: {}",
                        start + k + 2,
                        header.trim_end_matches(':'),
                        line.trim()
                    ));
                }
            }
        }
    }
    out
}

fn committed() -> Vec<(String, String)> {
    let root = repo_root();
    let mut files = Vec::new();
    let dir = root.join(".github/workflows");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .expect("workflows")
        .map(|e| {
            e.expect("entry")
                .file_name()
                .into_string()
                .expect("utf-8 name")
        })
        .filter(|n| {
            Path::new(n)
                .extension()
                .is_some_and(|e| e == "yml" || e == "yaml")
        })
        .collect();
    names.sort();
    for n in names {
        let text = std::fs::read_to_string(dir.join(&n)).expect("workflow");
        files.push((format!(".github/workflows/{n}"), text));
    }
    let docs = std::fs::read_to_string(root.join("docs/artifacts.md")).expect("docs/artifacts.md");
    files.push(("docs/artifacts.md".to_owned(), docs));
    files
}

fn with(name: &str, text: &str) -> Vec<(String, String)> {
    let mut files = committed();
    files.push((name.to_owned(), text.to_owned()));
    files
}

/// Asserts that the workflows and docs of the repository package no darwin `kbf-task` and name
/// no test-only feature: the guard that holds until the Mac confine exists.
#[test]
fn no_workflow_or_doc_ships_a_darwin_kbf_task() {
    assert_eq!(problems(&committed()), Vec::<String>::new());
}

/// The guard's plants, each a file added to the committed ones: an asset name, a package line in
/// a darwin job, a build line in a job that runs on a Mac, the feature in a workflow, and clean
/// files that must not be named.
#[test]
fn the_guard_names_each_way_a_darwin_kbf_task_would_ship() {
    let asset = problems(&with(
        "docs/artifacts.md",
        "| `kbf-task-<commit>-darwin-arm64.tar.gz` | `macos-15` |\n",
    ));
    assert_eq!(asset.len(), 1, "{asset:?}");
    assert!(
        asset[0].contains("cannot ship until the Mac confine (M7)"),
        "{asset:?}"
    );

    let package = "name: x\njobs:\n  darwin-arm64:\n    runs-on: macos-15\n    steps:\n      - run: cargo xtask package --binary target/t/release/kbf-task --platform arm64\n";
    let got = problems(&with(".github/workflows/artifacts.yml", package));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].contains("the darwin job `darwin-arm64` names kbf-task"),
        "{got:?}"
    );

    let mac = "jobs:\n  build-mac:\n    runs-on: macos-15\n    steps:\n      - run: cargo build -p kbf-task\n";
    let got = problems(&with(".github/workflows/other.yml", mac));
    assert_eq!(got.len(), 1, "{got:?}");

    let feature = "jobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo build --release -p kbf-task --features unconfined-for-tests\n";
    let got = problems(&with(".github/workflows/other.yml", feature));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains("unconfined-for-tests"), "{got:?}");

    let linux = "jobs:\n  linux-x86_64:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo build --release -p kbf-task --bin kbf-task\n";
    assert_eq!(
        problems(&with(".github/workflows/other.yml", linux)),
        Vec::<String>::new()
    );

    let matrix = "jobs:\n  t:\n    strategy:\n      matrix:\n        os: [ubuntu-24.04, macos-15]\n    runs-on: ${{ matrix.os }}\n    steps:\n      - run: cargo build --release --workspace\n";
    let got = problems(&with(".github/workflows/other.yml", matrix));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains("builds the whole workspace"), "{got:?}");
    let excluded = matrix.replace("--workspace", "--workspace --exclude kbf-task");
    assert_eq!(
        problems(&with(".github/workflows/other.yml", &excluded)),
        Vec::<String>::new()
    );

    let interpolated = "jobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cp dist/kbf-task-${{ matrix.os }}.tar.gz out\n";
    let got = problems(&with(".github/workflows/other.yml", interpolated));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains("interpolation"), "{got:?}");
}
