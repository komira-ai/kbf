//! The only darwin `kbf-task` that ships is the bundle, and it is made by one job.
//!
//! `kbf-task` has a macOS confine (`crates/kbf-task/src/network.rs`: the Command runs under
//! `sandbox-exec`), and a Mac runner job tests it. The darwin job of `artifacts.yml` builds it with the
//! confine, signs it, packs it with the pinned `js-bundle` and ships the OCI layout as
//! `kbf-task-bundle-<commit>-darwin-arm64.tar.gz` (docs/artifacts.md, "The macOS `kbf-task` bundle";
//! its steps are pinned by `release_artifacts/task_bundle.rs`). This lint allows that bundle and
//! refuses every other way a darwin `kbf-task` could ship, so it cannot happen by a stray line.
//!
//! This is a text tripwire, not the backstop. The backstop is the pin of `release_artifacts`: the
//! asset table and the workflow must agree, so another darwin `kbf-task` asset needs an edit of
//! docs/artifacts.md and of that table. The lint cannot read a build that names nothing (a script, an
//! action) and says so by being a text match. A line is read as words split on whitespace: `cargo` and the
//! word after it (a `+toolchain` between them is skipped) name the verb. Comment lines of a workflow
//! are not read (a comment ships nothing). The bundle job is the job `darwin-arm64` of
//! `.github/workflows/artifacts.yml`.
//!
//! The rules, over `.github/workflows/*.yml` and `docs/artifacts.md`:
//! - **A bare asset.** No word names a darwin `kbf-task` other than the bundle: a word whose last path
//!   segment begins `kbf-task` and which has `darwin` in it, unless the segment begins
//!   `kbf-task-bundle-` (`kbf-task-<commit>-darwin-arm64.tar.gz`, `target/aarch64-apple-darwin/release/kbf-task`),
//!   except on a line whose every `cargo` runs `check`, `clippy` or `test` (compiling or testing it for darwin
//!   without shipping it is wanted); and no line runs `cargo xtask package` on the binary `kbf-task` in a
//!   job on a Mac or for a darwin platform, nor `cargo xtask darwin-asset` on one;
//! - **A build outside the bundle job.** In any job but the bundle job, no `cargo build`, `b` or `install`
//!   for an `apple-darwin` target over `--workspace` or `--all` without `--exclude kbf-task` (that exact name,
//!   not `kbf-task-pack`), or naming the package (`-p kbf-task`, `-pkbf-task`, `--package kbf-task`,
//!   `--package=kbf-task`, `--path crates/kbf-task`); and in a job that runs on a Mac (its name has `darwin`,
//!   or `runs-on` or a matrix names `macos-*`), no line names `kbf-task` (its build, SBOM, package or
//!   image) except a `cargo test`, `check` or `clippy` line. In every job, the bundle job included, no
//!   `cargo build`, `b`, `install`, `test`, `nextest`, `check`, `clippy` or `llvm-cov` line on a Mac runs over
//!   `--workspace` or `--all` without `--exclude kbf-task`;
//!   (A reference to the bundle asset, `kbf-task-bundle-<commit>-darwin-arm64` and its SBOM and provenance bundle, in the
//!   attest, sums, verify and publish jobs is not a mention of `kbf-task`: only other mentions are refused there.)
//! - **The bundle from another job.** No job but the bundle job runs `cargo xtask package-task-bundle` or
//!   the `js-bundle` that packs one;
//! - no line naming `kbf-task` interpolates a matrix, env or input value (a name the text cannot
//!   resolve); `${{ github.sha }}` and the like pick no platform and pass;
//! - **The test-only feature.** No workflow names `unconfined-for-tests`, which lets a Command run with the
//!   network where no confine exists (neither Linux nor macOS) and is for developers' test runs only, in
//!   the bundle job or any other.

use std::path::{Path, PathBuf};

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

/// The word after each `cargo` in `words`, skipping a `+toolchain` or a `-flag` between them: the
/// verb. (A flag that takes its value as a separate word, `-Z x`, makes that value the verb.)
fn cargo_verbs<'a>(words: &[&'a str]) -> Vec<&'a str> {
    words
        .iter()
        .enumerate()
        .filter(|(_, w)| **w == "cargo")
        .filter_map(|(i, _)| {
            words[i + 1..]
                .iter()
                .find(|w| !w.starts_with(['+', '-']))
                .copied()
        })
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

/// The workflow and the job that make the bundle.
const BUNDLE_WORKFLOW: &str = ".github/workflows/artifacts.yml";
const BUNDLE_JOB: &str = "darwin-arm64";

/// Whether `word` (a path or a file name) names a darwin `kbf-task` that is not the bundle: its last path
/// segment begins `kbf-task` and the word has `darwin` in it, and the segment does not begin
/// `kbf-task-bundle-`.
fn is_bare_darwin_task(word: &str) -> bool {
    let word = word.trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '|' || c == ',');
    let segment = word.rsplit('/').next().unwrap_or(word);
    segment.starts_with("kbf-task")
        && word.contains("darwin")
        && !segment.starts_with("kbf-task-bundle-")
}

/// Whether some word of `words` is the binary `kbf-task` (its last path segment, quotes dropped).
fn names_task_binary(words: &[&str]) -> bool {
    words.iter().any(|w| {
        let w = w.trim_matches(|c| c == '"' || c == '\'');
        w.rsplit('/').next() == Some("kbf-task")
    })
}

/// Every way `files` (name and text) ship a darwin `kbf-task` other than the bundle, make the bundle
/// outside its job, or turn on the test feature.
fn problems(files: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for (name, text) in files {
        let lines: Vec<&str> = text.lines().collect();
        let workflow = name.starts_with(".github/workflows/");
        for (i, line) in lines.iter().enumerate() {
            let at = format!("{name}:{}", i + 1);
            let comment = workflow && line.trim_start().starts_with('#');
            // `${{ x }}` is one word, whatever its spaces.
            let joined = line.replace("${{ ", "${{").replace(" }}", "}}");
            if !comment
                && !is_cross_check(line)
                && joined.split_whitespace().any(is_bare_darwin_task)
            {
                out.push(format!(
                    "{at}: names a darwin kbf-task other than the kbf-task bundle, the only one that ships (`kbf-task-bundle-<commit>-darwin-arm64`): {}",
                    line.trim()
                ));
            }
            if workflow && !comment && line.contains("kbf-task") && interpolates_a_platform(line) {
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
            let bundle_job = name == BUNDLE_WORKFLOW && job == BUNDLE_JOB;
            for (k, line) in body.iter().enumerate() {
                if line.trim_start().starts_with('#') {
                    continue;
                }
                let at = format!("{name}:{}", start + k + 2);
                let words: Vec<&str> = line.split_whitespace().collect();
                let runs = |verb: &str| words.windows(3).any(|w| w == ["cargo", "xtask", verb]);
                if builds_task_for_darwin(line) && !bundle_job {
                    out.push(format!(
                        "{at}: the job `{job}` builds kbf-task for an apple-darwin target outside the bundle job (`{BUNDLE_JOB}` of {BUNDLE_WORKFLOW}): {}",
                        line.trim()
                    ));
                } else if mac && builds_workspace(line) {
                    out.push(format!(
                        "{at}: the darwin job `{job}` builds the whole workspace, kbf-task included, without `--exclude kbf-task`: {}",
                        line.trim()
                    ));
                }
                if mac
                    && !bundle_job
                    && line
                        .replace("kbf-task-bundle", "kbf-bundle")
                        .contains("kbf-task")
                    && !excludes_task(&words)
                    && !is_cross_check(line)
                {
                    out.push(format!(
                        "{at}: the darwin job `{job}` names kbf-task outside the bundle job (`{BUNDLE_JOB}` of {BUNDLE_WORKFLOW}): {}",
                        line.trim()
                    ));
                }
                let bundle_made = runs("package-task-bundle") || line.contains("js-bundle pack ");
                if bundle_made && !bundle_job {
                    out.push(format!(
                        "{at}: the job `{job}` makes the kbf-task bundle, which only the bundle job (`{BUNDLE_JOB}` of {BUNDLE_WORKFLOW}) does: {}",
                        line.trim()
                    ));
                }
                let packs = runs("package") || runs("darwin-asset");
                if packs && names_task_binary(&words) && (mac || line.contains("darwin")) {
                    out.push(format!(
                        "{at}: the job `{job}` packages or checks a bare kbf-task binary for a Mac, which ships only in the bundle: {}",
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

/// Asserts that the workflows and docs of the repository ship no darwin `kbf-task` but the bundle, make the
/// bundle in its job only, and name no test-only feature.
#[test]
fn no_workflow_or_doc_ships_a_darwin_kbf_task_but_the_bundle() {
    assert_eq!(problems(&committed()), Vec::<String>::new());
}

/// A job of `artifacts.yml` named `job`, on `runner`, with `steps` (each a `run:` line).
fn artifacts(job: &str, runner: &str, steps: &[&str]) -> Vec<(String, String)> {
    let mut text = format!("jobs:\n  {job}:\n    runs-on: {runner}\n    steps:\n");
    for step in steps {
        text.push_str(&format!("      - run: {step}\n"));
    }
    with(BUNDLE_WORKFLOW, &text)
}

fn other(job: &str, runner: &str, steps: &[&str]) -> Vec<(String, String)> {
    let mut text = format!("jobs:\n  {job}:\n    runs-on: {runner}\n    steps:\n");
    for step in steps {
        text.push_str(&format!("      - run: {step}\n"));
    }
    with(".github/workflows/other.yml", &text)
}

/// The steps of the bundle job, as `artifacts.yml` has them, in the one job that may have them.
const BUNDLE_STEPS: [&str; 6] = [
    "cargo build --locked --release --target \"$TARGET\" -p kbf-task --bin kbf-task",
    "cargo xtask sign-darwin \"target/$TARGET/release/kbf-task\" --minos 14.0",
    "mkdir -p \"$RUNNER_TEMP/task-tree\" && cp \"target/$TARGET/release/kbf-task\" \"$RUNNER_TEMP/task-tree/kbf-task\"",
    "$RUNNER_TEMP/tools/js-bundle pack --out \"$RUNNER_TEMP/task-bundle\" --name kbf-task --entrypoint 'kbf-task=${tree:task}/kbf-task'",
    "cargo xtask package-task-bundle --layout \"$RUNNER_TEMP/task-bundle\" --sbom crates/kbf-task/kbf-task_bin.cdx.json --commit \"$GITHUB_SHA\" --out dist",
    "cp dist/kbf-task-bundle-${{ github.sha }}-darwin-arm64.tar.gz out",
];

/// What the bundle job may do passes, and each way a darwin `kbf-task` could ship otherwise, or the
/// bundle be made outside its job, is named by its rule's text.
#[test]
fn the_guard_allows_the_bundle_and_names_each_other_way_a_darwin_kbf_task_would_ship() {
    // The bundle job, as it is: allowed.
    assert_eq!(
        problems(&artifacts(BUNDLE_JOB, "macos-15", &BUNDLE_STEPS)),
        Vec::<String>::new()
    );
    // The bundle's asset names, in a doc and in a workflow, pass; the same names with the bundle left
    // out of them do not.
    assert_eq!(
        problems(&with(
            "docs/artifacts.md",
            "| `kbf-task-bundle-<commit>-darwin-arm64.tar.gz` | `macos-15` |\n"
        )),
        Vec::<String>::new()
    );

    // Rule: a bare asset. An asset name, a path of the binary built for a Mac target, in a doc and in a
    // workflow; the bundle's job does not make them pass.
    const BARE: &str = "names a darwin kbf-task other than the kbf-task bundle";
    for line in [
        "| `kbf-task-<commit>-darwin-arm64.tar.gz` | `macos-15` |",
        "gh release upload v1 dist/kbf-task-c0ffee-darwin-arm64.tar.gz",
        "cp target/aarch64-apple-darwin/release/kbf-task dist/",
        "docker build -t kbf-task-image-x-darwin-arm64 .",
    ] {
        let got = problems(&with("docs/artifacts.md", &format!("{line}\n")));
        assert_eq!(got.len(), 1, "{line}: {got:?}");
        assert!(got[0].contains(BARE), "{line}: {got:?}");
        let text =
            format!("jobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: {line}\n");
        let got = problems(&with(".github/workflows/other.yml", &text));
        assert!(got.iter().any(|p| p.contains(BARE)), "{line}: {got:?}");
    }
    // ... in the bundle job too.
    let got = problems(&artifacts(
        BUNDLE_JOB,
        "macos-15",
        &["cp dist/kbf-task-c0ffee-darwin-arm64.tar.gz out"],
    ));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains(BARE), "{got:?}");
    // A comment of a workflow is not a line that ships anything.
    assert_eq!(
        problems(&other(
            "t",
            "ubuntu-24.04",
            &["true # kbf-task-x-darwin-arm64"]
        ))
        .len(),
        1,
        "a trailing comment is part of the line"
    );
    assert_eq!(
        problems(&with(
            ".github/workflows/other.yml",
            "# the old kbf-task-x-darwin-arm64.tar.gz is gone\njobs: {}\n"
        )),
        Vec::<String>::new()
    );

    // Rule: a bare binary packaged or checked for a Mac.
    const PACKAGED: &str = "packages or checks a bare kbf-task binary for a Mac";
    for step in [
        "cargo xtask package --binary target/$TARGET/release/kbf-task --sbom s --platform darwin-arm64 --commit $GITHUB_SHA --out dist",
        "cargo xtask darwin-asset dist/x.tar.gz kbf-task",
    ] {
        let got = problems(&artifacts(BUNDLE_JOB, "macos-15", &[step]));
        assert!(got.iter().any(|p| p.contains(PACKAGED)), "{step}: {got:?}");
    }
    let linux_for_mac = "jobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo xtask package --binary target/x/release/kbf-task --sbom s --platform darwin-arm64 --commit c --out dist\n";
    let got = problems(&with(".github/workflows/other.yml", linux_for_mac));
    assert!(got.iter().any(|p| p.contains(PACKAGED)), "{got:?}");
    // The Linux package line is not one.
    let linux = "jobs:\n  linux-x86_64:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo xtask package --binary target/x/release/kbf-task --sbom s --platform linux-x86_64 --commit c --out dist\n      - run: cargo build --release -p kbf-task --bin kbf-task\n";
    assert_eq!(
        problems(&with(".github/workflows/other.yml", linux)),
        Vec::<String>::new()
    );

    // Rule: the test-only feature, in any job, the bundle job included.
    const FEATURE: &str = "names the test-only feature `unconfined-for-tests`";
    let mut steps = BUNDLE_STEPS.to_vec();
    steps[0] = "cargo build --locked --release --target \"$TARGET\" -p kbf-task --features unconfined-for-tests --bin kbf-task";
    let got = problems(&artifacts(BUNDLE_JOB, "macos-15", &steps));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains(FEATURE), "{got:?}");
    let got = problems(&other(
        "t",
        "ubuntu-24.04",
        &["cargo build --release -p kbf-task --features unconfined-for-tests"],
    ));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains(FEATURE), "{got:?}");

    // Rule: a build outside the bundle job. By package name or over the workspace, for an apple-darwin
    // target, in another workflow, in another job of artifacts.yml, and on a Linux runner.
    const OUTSIDE: &str = "builds kbf-task for an apple-darwin target outside the bundle job";
    for step in [
        "cargo build --release --workspace --target aarch64-apple-darwin",
        "cargo b --release -p kbf-task --target aarch64-apple-darwin",
        "cargo install --path crates/kbf-task --target aarch64-apple-darwin",
    ] {
        for (files, what) in [
            (other("t", "ubuntu-24.04", &[step]), "other workflow, Linux"),
            (
                artifacts("darwin-x", "ubuntu-24.04", &[step]),
                "other job of artifacts.yml",
            ),
            (
                with(
                    ".github/workflows/other.yml",
                    &format!(
                        "jobs:\n  {BUNDLE_JOB}:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: {step}\n"
                    ),
                ),
                "the bundle job's name in another workflow",
            ),
        ] {
            let got = problems(&files);
            assert!(
                got.iter().any(|p| p.contains(OUTSIDE)),
                "{step} ({what}): {got:?}"
            );
        }
    }
    // `--exclude kbf-task` (in both spellings) and kbf-task-pack leave it as it was: a build that excludes it passes,
    // one that excludes only the pack crate does not.
    let darwin = "--target aarch64-apple-darwin";
    for exclude in ["--exclude kbf-task", "--exclude=kbf-task"] {
        let line = format!("cargo build --workspace {exclude} {darwin}");
        assert!(!builds_task_for_darwin(&line), "{line}");
    }
    assert!(builds_task_for_darwin(&format!(
        "cargo build --workspace --exclude kbf-task-pack {darwin}"
    )));
    for flag in [
        "-p kbf-task",
        "-pkbf-task",
        "--package kbf-task",
        "--package=kbf-task",
        "--path crates/kbf-task",
        "--path=crates/kbf-task",
    ] {
        assert!(
            builds_task_for_darwin(&format!("cargo install {darwin} {flag}")),
            "{flag}"
        );
    }
    for flag in [
        "-p kbf-task-pack",
        "-pkbf-task-pack",
        "--package=kbf-task-pack",
        "--path crates/kbf-task-pack",
    ] {
        assert!(
            !builds_task_for_darwin(&format!("cargo build {darwin} {flag}")),
            "{flag}"
        );
    }
    assert!(!builds_task_for_darwin(&format!(
        "cargo fetch --workspace {darwin}"
    )));
    // A job on a Mac that builds, packages or names kbf-task outside the bundle job.
    const MAC_NAMES: &str = "names kbf-task outside the bundle job";
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
    for (step, rules) in [
        (
            "cargo build --locked --release -p kbf-task",
            vec![MAC_NAMES],
        ),
        ("cargo b -p kbf-task", vec![MAC_NAMES]),
        ("cargo install --path crates/kbf-task", vec![MAC_NAMES]),
        (
            "cargo xtask package --binary target/release/kbf-task",
            vec![MAC_NAMES, PACKAGED],
        ),
        (
            "cargo test -p kbf-task --features unconfined-for-tests",
            vec![FEATURE],
        ),
    ] {
        let got = problems(&with(".github/workflows/other.yml", &mac_job(step)));
        for rule in rules {
            assert!(
                got.iter().any(|p| p.contains(rule)),
                "{step}: wanted {rule:?}: {got:?}"
            );
        }
    }
    let got = problems(&with(
        ".github/workflows/other.yml",
        &mac_job("cargo build --locked --release -p kbf-task"),
    ));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains(MAC_NAMES), "{got:?}");
    // The same line in the bundle job is the bundle's build.
    assert_eq!(
        problems(&artifacts(
            BUNDLE_JOB,
            "macos-15",
            &["cargo build --locked --release -p kbf-task"]
        )),
        Vec::<String>::new()
    );

    // The jobs after the build refer to the bundle asset by name: allowed; any other darwin kbf-task mention
    // in them is refused by the rules above.
    for job in [
        "attest-darwin-arm64",
        "sums-darwin-arm64",
        "verify-darwin-arm64",
        "publish-darwin-arm64",
    ] {
        let jobs = |lines: &[&str]| {
            let mut text = format!("jobs:\n  {job}:\n    runs-on: ubuntu-24.04\n    steps:\n");
            for l in lines {
                text.push_str(&format!("      {l}\n"));
            }
            with(BUNDLE_WORKFLOW, &text)
        };
        assert_eq!(
            problems(&jobs(&[
                "- uses: actions/attest-sbom@v1",
                "  with:",
                "    subject-path: dist/kbf-task-bundle-${{ github.sha }}-darwin-arm64.tar.gz",
                "    sbom-path: dist/kbf-task-bundle-${{ github.sha }}-darwin-arm64.cdx.json",
                "- id: provenance-kbf-task-bundle-darwin-arm64",
                "  run: cp \"$B\" \"dist/kbf-task-bundle-${GITHUB_SHA}-darwin-arm64.tar.gz.sigstore.json\"",
            ])),
            Vec::<String>::new(),
            "{job}"
        );
        let got = problems(&jobs(&[
            "- uses: actions/attest-sbom@v1",
            "  with:",
            "    subject-path: dist/kbf-task-${{ github.sha }}-darwin-arm64.tar.gz",
        ]));
        assert!(got.iter().any(|p| p.contains(BARE)), "{job}: {got:?}");
        assert!(got.iter().any(|p| p.contains(MAC_NAMES)), "{job}: {got:?}");
        let got = problems(&jobs(&["- run: cp target/release/kbf-task dist/"]));
        assert!(got.iter().any(|p| p.contains(MAC_NAMES)), "{job}: {got:?}");
    }

    // Rule: the whole workspace on a Mac, kbf-task included (the bundle job too).
    const WORKSPACE: &str = "builds the whole workspace";
    let matrix = "jobs:\n  t:\n    strategy:\n      matrix:\n        os: [ubuntu-24.04, macos-15]\n    runs-on: ${{ matrix.os }}\n    steps:\n      - run: cargo build --release --workspace\n";
    let got = problems(&with(".github/workflows/other.yml", matrix));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains(WORKSPACE), "{got:?}");
    for exclude in ["--exclude kbf-task", "--exclude=kbf-task"] {
        let excluded = matrix.replace("--workspace", &format!("--workspace {exclude}"));
        assert_eq!(
            problems(&with(".github/workflows/other.yml", &excluded)),
            Vec::<String>::new(),
            "{exclude}"
        );
    }
    let pack_excluded = matrix.replace("--workspace", "--workspace --exclude kbf-task-pack");
    let got = problems(&with(".github/workflows/other.yml", &pack_excluded));
    assert!(got.iter().any(|p| p.contains(WORKSPACE)), "{got:?}");
    let got = problems(&artifacts(
        BUNDLE_JOB,
        "macos-15",
        &["cargo build --release --workspace"],
    ));
    assert!(got.iter().any(|p| p.contains(WORKSPACE)), "{got:?}");

    // Rule: the bundle made outside its job.
    const MADE: &str = "makes the kbf-task bundle, which only the bundle job";
    for step in [
        "cargo xtask package-task-bundle --layout l --sbom s --commit c --out dist",
        "$RUNNER_TEMP/tools/js-bundle pack --out x --name kbf-task",
    ] {
        for files in [
            other("t", "macos-15", &[step]),
            other("t", "ubuntu-24.04", &[step]),
            artifacts("darwin-other", "macos-15", &[step]),
            artifacts("linux-x86_64", "ubuntu-24.04", &[step]),
        ] {
            let got = problems(&files);
            assert!(got.iter().any(|p| p.contains(MADE)), "{step}: {got:?}");
        }
        // In the bundle job it is allowed.
        assert!(
            !problems(&artifacts(BUNDLE_JOB, "macos-15", &[step]))
                .iter()
                .any(|p| p.contains(MADE)),
            "{step}"
        );
    }

    // Rule: an interpolation the lint cannot resolve in a line naming kbf-task.
    let interpolated = "jobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cp dist/kbf-task-${{ matrix.os }}.tar.gz out\n";
    let got = problems(&with(".github/workflows/other.yml", interpolated));
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains("interpolation"), "{got:?}");
    let sha = "jobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cp dist/kbf-task-${{ github.sha }}-linux-x86_64.tar.gz out\n";
    assert_eq!(
        problems(&with(".github/workflows/other.yml", sha)),
        Vec::<String>::new()
    );

    // Cross checks: `cargo check`, `clippy` and `test` for a Mac target ship nothing, and a `check` as a later
    // word or after another cargo subcommand is not one.
    let cross = "jobs:\n  t:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo check --locked --target aarch64-apple-darwin -p kbf-task\n      - run: cargo clippy --workspace --target aarch64-apple-darwin\n";
    assert_eq!(
        problems(&with(".github/workflows/other.yml", cross)),
        Vec::<String>::new()
    );
    for line in [
        "cargo xtask pkg check --binary target/aarch64-apple-darwin/release/kbf-task",
        "cargo check -p kbf-task --target aarch64-apple-darwin && cargo xtask package --binary target/aarch64-apple-darwin/release/kbf-task",
    ] {
        let got = problems(&with("docs/artifacts.md", &format!("{line}\n")));
        assert_eq!(got.len(), 1, "{line}: {got:?}");
        assert!(got[0].contains(BARE), "{line}: {got:?}");
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

    // A Linux job that builds kbf-task for its own target.
    let linux_build = "jobs:\n  linux-x86_64:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo build --release -p kbf-task --bin kbf-task\n";
    assert_eq!(
        problems(&with(".github/workflows/other.yml", linux_build)),
        Vec::<String>::new()
    );
}
