//! No darwin `kbf-task` asset or image ships, and no darwin `kbf-task` is built for shipping.
//!
//! `kbf-task` has a macOS confine (`crates/kbf-task/src/network.rs`: the Command runs under
//! `sandbox-exec`), and a Mac runner job tests it. Nothing yet builds, packages or releases a
//! darwin `kbf-task`: no asset, package line, image, or `cargo build` of it for a Mac target. This
//! lint holds that until a release of it is designed and the lint is changed with it.
//!
//! This is a text tripwire, not the backstop. The backstop is the pin of `release_artifacts`: the
//! asset table and the workflow must agree, so a darwin `kbf-task` asset needs an edit of
//! docs/artifacts.md, which these rules catch. The lint cannot read a build that names nothing
//! (a script, an action) and says so by being a text match. A line is read as words split on
//! whitespace: `cargo` and the word after it (a `+toolchain` between them is skipped) name the verb.
//!
//! The rules, over `.github/workflows/*.yml` and `docs/artifacts.md`:
//! - no line names `kbf-task` and `darwin` together (`kbf-task-<commit>-darwin-arm64.tar.gz`, a
//!   `--platform darwin-arm64` package line, a release asset list), except a line whose every `cargo`
//!   runs `check`, `clippy` or `test`: compiling or testing it for darwin without shipping it is wanted;
//! - in any job, no `cargo build`, `b` or `install` for an `apple-darwin` target over `--workspace` or
//!   `--all` without `--exclude kbf-task` (that exact name, not `kbf-task-pack`), or naming the
//!   package (`-p kbf-task`, `-pkbf-task`, `--package kbf-task`, `--package=kbf-task`,
//!   `--path crates/kbf-task`);
//! - in a job that runs on a Mac (its name has `darwin`, or `runs-on` or a matrix names `macos-*`),
//!   no line names `kbf-task` (its build, SBOM, package or image) except a `cargo test`, `check` or
//!   `clippy` line, and no `cargo build`, `b`, `install`, `test`, `nextest`, `check`, `clippy` or
//!   `llvm-cov` line runs over `--workspace` or `--all` without `--exclude kbf-task`;
//! - no line naming `kbf-task` interpolates a matrix, env or input value (a name the text cannot
//!   resolve); `${{ github.sha }}` and the like pick no platform and pass;
//! - no workflow names the feature `unconfined-for-tests`, which lets a Command run with the
//!   network where no confine exists (neither Linux nor macOS) and is for developers' test runs only.

use std::path::{Path, PathBuf};

/// What has to change before a darwin `kbf-task` ships.
const LIFTED_BY: &str = "a darwin kbf-task release is designed and this lint is changed with it";

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

/// The word after each `cargo` in `words`, skipping a `+toolchain` between them: the verb.
fn cargo_verbs<'a>(words: &[&'a str]) -> Vec<&'a str> {
    words
        .iter()
        .enumerate()
        .filter(|(_, w)| **w == "cargo")
        .filter_map(|(i, _)| words[i + 1..].iter().find(|w| !w.starts_with('+')).copied())
        .collect()
}

/// Whether `line` runs cargo and every `cargo` on it has the verb `check`, `clippy` or `test`: it
/// compiles or tests a crate for a target and ships nothing, which is wanted for `kbf-task` and darwin.
/// The verb is the word right after `cargo`, so `cargo xtask pkg check` is not one.
fn is_cross_check(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    let verbs = cargo_verbs(&words);
    !verbs.is_empty()
        && verbs
            .iter()
            .all(|v| ["check", "clippy", "test"].contains(v))
}

/// The cargo verbs that produce a binary: `build`, its alias `b`, and `install`.
const BUILD_VERBS: [&str; 3] = ["build", "b", "install"];

/// Whether `words` name the package `kbf-task` itself (not `kbf-task-pack`): `-p kbf-task`,
/// `-pkbf-task`, `--package kbf-task`, `--package=kbf-task`, or the path of its crate.
fn names_task_package(words: &[&str]) -> bool {
    let path = |p: &str| p.trim_end_matches('/').ends_with("crates/kbf-task");
    words.iter().any(|w| {
        *w == "-pkbf-task"
            || *w == "--package=kbf-task"
            || w.strip_prefix("--path=").is_some_and(path)
    }) || words.windows(2).any(|w| {
        ((w[0] == "-p" || w[0] == "--package") && w[1] == "kbf-task")
            || (w[0] == "--path" && path(w[1]))
    })
}

/// Whether `words` exclude `kbf-task` itself: `--exclude kbf-task` or `--exclude=kbf-task`, not
/// `--exclude kbf-task-pack`.
fn excludes_task(words: &[&str]) -> bool {
    words.contains(&"--exclude=kbf-task")
        || words
            .windows(2)
            .any(|w| w[0] == "--exclude" && w[1] == "kbf-task")
}

/// Whether `line` is a `cargo build` (or `b`, or `install`) for an `apple-darwin` target that would
/// produce `kbf-task`: over the workspace without `--exclude kbf-task`, or naming its package. Any
/// job, any runner.
fn builds_task_for_darwin(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    cargo_verbs(&words).iter().any(|v| BUILD_VERBS.contains(v))
        && line.contains("apple-darwin")
        && (builds_workspace(line) || names_task_package(&words))
}

/// Whether `line` runs a cargo verb over the whole workspace and does not exclude `kbf-task`.
fn builds_workspace(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    let verbs = ["test", "nextest", "check", "clippy", "llvm-cov"];
    cargo_verbs(&words)
        .iter()
        .any(|v| BUILD_VERBS.contains(v) || verbs.contains(v))
        && words.iter().any(|w| *w == "--workspace" || *w == "--all")
        && !excludes_task(&words)
}

/// Every way `files` (name and text) ship a darwin `kbf-task` or turn on the test feature.
fn problems(files: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for (name, text) in files {
        let lines: Vec<&str> = text.lines().collect();
        let workflow = name.starts_with(".github/workflows/");
        for (i, line) in lines.iter().enumerate() {
            let at = format!("{name}:{}", i + 1);
            if line.contains("kbf-task") && line.contains("darwin") && !is_cross_check(line) {
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
            let mac = is_mac_runner_job(header, body);
            let job = header.trim_end_matches(':');
            for (k, line) in body.iter().enumerate() {
                let at = format!("{name}:{}", start + k + 2);
                if builds_task_for_darwin(line) {
                    out.push(format!(
                        "{at}: the job `{job}` builds kbf-task for an apple-darwin target, which cannot ship until {LIFTED_BY}: {}",
                        line.trim()
                    ));
                } else if mac && builds_workspace(line) {
                    out.push(format!(
                        "{at}: the darwin job `{job}` builds the whole workspace, kbf-task included, without `--exclude kbf-task`: {}",
                        line.trim()
                    ));
                }
                if mac
                    && line.contains("kbf-task")
                    && !line.contains("darwin")
                    && !excludes_task(&line.split_whitespace().collect::<Vec<_>>())
                    && !is_cross_check(line)
                {
                    out.push(format!(
                        "{at}: the darwin job `{job}` names kbf-task, which cannot ship until {LIFTED_BY}: {}",
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
/// no test-only feature.
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
        asset[0].contains("cannot ship until a darwin kbf-task release is designed"),
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

    let linux_build = "jobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo build --release --workspace --target aarch64-apple-darwin\n";
    let got = problems(&with(".github/workflows/other.yml", linux_build));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].contains("builds kbf-task for an apple-darwin target"),
        "{got:?}"
    );
    let named = linux_build.replace("--workspace", "-p kbf-task");
    let got = problems(&with(".github/workflows/other.yml", &named));
    assert!(!got.is_empty(), "{got:?}");

    let cross = "jobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo check --locked --target aarch64-apple-darwin -p kbf-task\n      - run: cargo clippy --workspace --target aarch64-apple-darwin\n";
    assert_eq!(
        problems(&with(".github/workflows/other.yml", cross)),
        Vec::<String>::new()
    );

    // `cargo b` and `cargo install` are builds, as `cargo build` is: the build rule reads the verb.
    let darwin = "--target aarch64-apple-darwin";
    for verb in ["b", "install", "build"] {
        let line = format!("cargo {verb} --release {darwin} -p kbf-task");
        assert!(builds_task_for_darwin(&line), "{line}");
        let text =
            format!("jobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: {line}\n");
        let got = problems(&with(".github/workflows/other.yml", &text));
        assert!(
            got.iter()
                .any(|p| p.contains("builds kbf-task for an apple-darwin target")),
            "cargo {verb}: {got:?}"
        );
    }
    assert!(builds_task_for_darwin(&format!(
        "cargo b --workspace {darwin}"
    )));
    assert!(!builds_task_for_darwin(&format!(
        "cargo fetch --workspace {darwin}"
    )));

    // Every spelling of the package flag, and the crate's path, names `kbf-task`; `kbf-task-pack` is
    // another package.
    for flag in [
        "-p kbf-task",
        "-pkbf-task",
        "--package kbf-task",
        "--package=kbf-task",
        "--path crates/kbf-task",
        "--path=crates/kbf-task",
    ] {
        let line = format!("cargo install {darwin} {flag}");
        assert!(builds_task_for_darwin(&line), "{line}");
    }
    for flag in [
        "-p kbf-task-pack",
        "-pkbf-task-pack",
        "--package=kbf-task-pack",
        "--path crates/kbf-task-pack",
    ] {
        let line = format!("cargo build {darwin} {flag}");
        assert!(!builds_task_for_darwin(&line), "{line}");
    }

    // Excluding `kbf-task-pack` does not exclude `kbf-task`; excluding `kbf-task` does, in both spellings.
    let pack_excluded = matrix.replace("--workspace", "--workspace --exclude kbf-task-pack");
    let got = problems(&with(".github/workflows/other.yml", &pack_excluded));
    assert!(
        got.iter().any(|p| p.contains("builds the whole workspace")),
        "{got:?}"
    );
    let line = format!("cargo build --workspace --exclude kbf-task-pack {darwin}");
    assert!(builds_task_for_darwin(&line), "{line}");
    for exclude in ["--exclude kbf-task", "--exclude=kbf-task"] {
        let line = format!("cargo build --workspace {exclude} {darwin}");
        assert!(!builds_task_for_darwin(&line), "{line}");
        let excluded = matrix.replace("--workspace", &format!("--workspace {exclude}"));
        assert_eq!(
            problems(&with(".github/workflows/other.yml", &excluded)),
            Vec::<String>::new(),
            "{exclude}"
        );
    }

    // Only a line whose `cargo` is followed by check, clippy or test is a cross check: `check` as a
    // later word, or after another cargo subcommand, is not.
    for line in [
        "cargo xtask pkg check --binary target/aarch64-apple-darwin/release/kbf-task --platform darwin-arm64",
        "cargo xtask clippy kbf-task darwin-arm64",
        "cargo check -p kbf-task --target aarch64-apple-darwin && cargo xtask package --binary kbf-task --platform darwin-arm64",
    ] {
        let got = problems(&with("docs/artifacts.md", &format!("{line}\n")));
        assert_eq!(got.len(), 1, "{line}: {got:?}");
        assert!(
            got[0].contains("names a darwin kbf-task"),
            "{line}: {got:?}"
        );
    }
    for line in [
        "cargo test --locked --target aarch64-apple-darwin -p kbf-task",
        "cargo +nightly clippy --target aarch64-apple-darwin -p kbf-task",
    ] {
        assert_eq!(
            problems(&with("docs/artifacts.md", &format!("{line}\n"))),
            Vec::<String>::new(),
            "{line}"
        );
    }

    // A Mac job may test, check and lint `kbf-task`, and may not build, package or ship it.
    let mac_job = |step: &str| {
        format!(
            "jobs:\n  task-macos:\n    runs-on: macos-latest\n    steps:\n      - run: {step}\n"
        )
    };
    for step in [
        "cargo test -p kbf-task --locked",
        "cargo check --locked -p kbf-task --tests",
        "cargo clippy --locked -p kbf-task",
    ] {
        assert_eq!(
            problems(&with(".github/workflows/other.yml", &mac_job(step))),
            Vec::<String>::new(),
            "{step}"
        );
    }
    for step in [
        "cargo build --locked --release -p kbf-task",
        "cargo b -p kbf-task",
        "cargo install --path crates/kbf-task",
        "cargo xtask package --binary target/release/kbf-task",
        "cargo test -p kbf-task --features unconfined-for-tests",
    ] {
        let got = problems(&with(".github/workflows/other.yml", &mac_job(step)));
        assert_eq!(got.len(), 1, "{step}: {got:?}");
    }

    let sha = "jobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cp dist/kbf-task-${{ github.sha }}-linux-x86_64.tar.gz out\n";
    assert_eq!(
        problems(&with(".github/workflows/other.yml", sha)),
        Vec::<String>::new()
    );
}
