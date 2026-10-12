//! Repository lints over the files git knows about (tracked, plus untracked files that
//! are not ignored, so a planted file is caught before it is committed).
//!
//! Catches:
//! - a workflow that can run on a non-hosted runner, uses `pull_request_target`, uses
//!   an action not pinned by commit SHA or outside the allowed set (`actions/*` and
//!   `tailscale/github-action`), or lacks top-level `permissions: {}`, and an
//!   `action.yml` in any directory with an unpinned step; a local `uses: ./...` that
//!   names no linted action or workflow; a workflow or action file that is not valid
//!   UTF-8 or holds a NUL byte (rules in `workflows/mod.rs`, which parses each file as
//!   YAML; its module docs say what it cannot cover);
//! - a text file carrying a non-documentation IPv4 address, an absolute home path, a
//!   citation of an unpublished design document by its sections (the IETF numbers
//!   pass) or a git conflict marker, and a file holding a NUL byte whose extension is
//!   not on the binary allow list (rules in `kbf_it::hygiene`);
//! - a shell, Python, awk, sed, Perl or Ruby file, a workflow `run:` step with shell
//!   logic, or a production `sh -c` in `crates/*/src/` that `tools/interpreter_budget.tsv`
//!   does not list (by path, never by size), and a row of that file that is stale,
//!   repeated or malformed (rules in `interpreters/mod.rs`);
//! - the darwin-arm64 job of `.github/workflows/artifacts.yml` no longer running
//!   `tools/ci/check-darwin-asset.sh` on the packaged binary after `Package`, or
//!   running it where its failure does not fail the job;
//! - the scans passing vacuously because git listed nothing, no Markdown file was
//!   scanned or the workflows moved;
//! - a drift in the rules that pick which files each lint reads (module `selection`).

mod interpreters;
mod workflows;

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

/// Paths, relative to the root, of tracked and untracked-but-not-ignored files.
fn listed_files(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .output()
        .expect("run git ls-files (the lints need git)");
    assert!(
        out.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut files: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8(p.to_vec()).expect("UTF-8 path"))
        .collect();
    files.sort();
    files.dedup();
    files
}

/// The file's bytes, or `None` for a tracked file deleted in the working tree.
fn read_bytes(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("read {}: {e}", path.display()),
    }
}

/// A workflow file: `.github/workflows/<...>.yml` or `.yaml`, the extension in any case.
fn is_workflow(f: &str) -> bool {
    let lower = f.to_ascii_lowercase();
    f.starts_with(".github/workflows/") && (lower.ends_with(".yml") || lower.ends_with(".yaml"))
}

/// An `action.yml` or `action.yaml` in any directory, any case.
fn is_action_file(f: &str) -> bool {
    let name = f.rsplit('/').next().unwrap_or(f);
    name.eq_ignore_ascii_case("action.yml") || name.eq_ignore_ascii_case("action.yaml")
}

/// The directory of an action file, as a local `uses: ./<dir>` names it (`""` for the
/// root).
fn action_dir(f: &str) -> &str {
    f.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Which rules the workflow lint applies to a file.
#[derive(Debug, Clone, Copy)]
enum Linted {
    Workflow,
    Action,
}

/// The workflow lint's problems with the file `f`, each prefixed with `f`. A file the
/// lint cannot read is a problem, never skipped.
fn lint_workflow_file(
    f: &str,
    bytes: &[u8],
    linted: Linted,
    repo: &workflows::Repo,
) -> Vec<String> {
    let found = match (workflows::decode(bytes), linted) {
        (Ok(text), Linted::Action) => workflows::scan_action(text, repo),
        (Ok(text), Linted::Workflow) => workflows::scan(text, repo),
        (Err(e), _) => vec![e],
    };
    found.into_iter().map(|p| format!("{f}: {p}")).collect()
}

/// What the hygiene scan made of one file.
#[derive(Debug, PartialEq, Eq)]
enum TextFile {
    /// Skipped: its extension is on the binary allow list.
    Binary,
    /// Not read, a problem; the message (prefixed with the path) says why.
    Refused(String),
    /// Decoded and scanned; holds the findings, each prefixed with the path.
    Scanned(Vec<String>),
}

fn check_text_file(f: &str, bytes: &[u8]) -> TextFile {
    match kbf_it::hygiene::decode(f, bytes) {
        Ok(Some(text)) => TextFile::Scanned(
            kbf_it::hygiene::scan(&text)
                .iter()
                .map(|p| format!("{f}: {p}"))
                .collect(),
        ),
        Ok(None) => TextFile::Binary,
        Err(e) => TextFile::Refused(format!("{f}: {e}")),
    }
}

#[test]
fn workflows_are_hosted_pinned_and_least_privilege() {
    let root = repo_root();
    let listed = listed_files(&root);
    let present = |f: &&String| root.join(f.as_str()).exists();
    let workflows: Vec<String> = listed.iter().filter(|f| is_workflow(f)).cloned().collect();
    let actions: Vec<String> = listed
        .iter()
        .filter(|f| is_action_file(f))
        .cloned()
        .collect();
    assert!(
        workflows.iter().any(|f| f == ".github/workflows/ci.yml"),
        "ci.yml not found; the workflow lint would check nothing: {workflows:?}"
    );
    // A local `uses: ./...` may name only a file this test lints below.
    let repo = workflows::Repo {
        action_dirs: actions
            .iter()
            .filter(present)
            .map(|f| action_dir(f).to_owned())
            .collect(),
        workflows: workflows.iter().filter(present).cloned().collect(),
    };
    let mut problems = Vec::new();
    let files = workflows.iter().map(|f| (f, Linted::Workflow));
    for (f, linted) in files.chain(actions.iter().map(|f| (f, Linted::Action))) {
        if let Some(bytes) = read_bytes(&root.join(f)) {
            problems.extend(lint_workflow_file(f, &bytes, linted, &repo));
        }
    }
    assert!(
        problems.is_empty(),
        "workflow lint:\n{}",
        problems.join("\n")
    );
}

#[test]
fn text_files_carry_no_addresses_home_paths_design_citations_or_conflict_markers() {
    let root = repo_root();
    let files = listed_files(&root);
    let mut scanned = 0;
    let mut markdown = 0;
    let mut problems = Vec::new();
    for f in &files {
        let Some(bytes) = read_bytes(&root.join(f)) else {
            continue;
        };
        match check_text_file(f, &bytes) {
            TextFile::Binary => {}
            TextFile::Refused(e) => problems.push(e),
            TextFile::Scanned(found) => {
                scanned += 1;
                markdown += usize::from(f.ends_with(".md"));
                problems.extend(found);
            }
        }
    }
    assert!(
        scanned >= 20 && files.iter().any(|f| f == "Cargo.toml"),
        "only {scanned} text files scanned; git listed too little"
    );
    assert!(
        markdown >= 5,
        "only {markdown} Markdown files scanned; the docs escape the scan"
    );
    assert!(
        problems.is_empty(),
        "public hygiene:\n{}",
        problems.join("\n")
    );
}

/// What the interpreter budget scan read and found in a tree.
#[derive(Debug, Default)]
struct BudgetScan {
    found: Vec<interpreters::Found>,
    /// Files the scan could not clear: a workflow that does not parse, two logic
    /// steps with one key, a Rust source that is not UTF-8 or does not parse.
    problems: Vec<String>,
    /// `run:` steps read, logic or not.
    run_steps: usize,
    /// Rust files under `crates/*/src/` read.
    rust_files: usize,
}

/// The interpreter budget scan of the listed `files`, each opened by `read` (`None`
/// for a listed file deleted in the working tree).
fn interpreter_scan(files: &[String], read: impl Fn(&str) -> Option<Vec<u8>>) -> BudgetScan {
    use interpreters::{Found, Section};
    let mut found = Vec::new();
    let mut problems = Vec::new();
    let mut rust_src = Vec::new();
    let mut run_steps = 0;
    for f in files {
        let Some(bytes) = read(f) else {
            continue;
        };
        if let Some(why) = interpreters::interpreter_file(f, &bytes) {
            found.push(Found {
                section: Section::Files,
                key: f.clone(),
                why,
            });
        }
        if is_workflow(f) || is_action_file(f) {
            let steps = workflows::decode(&bytes).and_then(workflows::run_steps);
            let steps = match steps {
                Ok(steps) => steps,
                Err(e) => {
                    problems.push(format!("{f}: {e}"));
                    continue;
                }
            };
            run_steps += steps.len();
            for s in steps {
                let Some(what) = interpreters::shell_logic(&s.script) else {
                    continue;
                };
                let key = format!("{f}#{}/{}", s.job, s.step);
                if found
                    .iter()
                    .any(|x: &Found| x.section == Section::Workflow && x.key == key)
                {
                    problems.push(format!(
                        "`{key}`: two steps share this key; name them apart"
                    ));
                    continue;
                }
                found.push(Found {
                    section: Section::Workflow,
                    key,
                    why: format!("its `run:` script has {what}"),
                });
            }
        }
        if interpreters::rust::is_crate_src(f) {
            match String::from_utf8(bytes) {
                Ok(text) => rust_src.push((f.clone(), text)),
                Err(e) => problems.push(format!("{f}: not UTF-8 ({e})")),
            }
        }
    }
    let (spawns, unread) = interpreters::rust::shell_c_files(&rust_src);
    problems.extend(unread);
    found.extend(spawns.into_iter().map(|key| Found {
        section: Section::SrcShC,
        key,
        why: "its production code names a shell and a `-c` flag".to_owned(),
    }));
    BudgetScan {
        found,
        problems,
        run_steps,
        rust_files: rust_src.len(),
    }
}

/// The scan's problems and the budget's, for a tree and a budget text.
fn budget_problems(scan: BudgetScan, budget: &str) -> Vec<String> {
    let mut problems = scan.problems;
    problems.extend(interpreters::check(budget, &scan.found));
    problems
}

#[test]
fn every_interpreter_file_workflow_script_and_shell_spawn_is_in_the_budget() {
    // Catches: a new shell, Python, awk, sed, Perl or Ruby file (by extension or by
    // its `#!` line); a row left for a file that is gone; a new workflow `run:` step
    // with shell logic; a new `sh -c` in production Rust; a malformed or duplicate row
    // (module `interpreters`); and the scan passing because it read no workflow steps
    // or no Rust sources. Sizes are not checked: an edit of a listed file passes.
    let root = repo_root();
    let files = listed_files(&root);
    let scan = interpreter_scan(&files, |f| read_bytes(&root.join(f)));
    assert!(
        scan.run_steps >= 10,
        "only {} `run:` steps read; the workflow scan reads too little",
        scan.run_steps
    );
    assert!(
        scan.rust_files >= 50,
        "only {} Rust files under crates/*/src read; the shell scan reads too little",
        scan.rust_files
    );
    let budget = std::fs::read_to_string(root.join(interpreters::BUDGET))
        .unwrap_or_else(|e| panic!("{}: {e}", interpreters::BUDGET));
    let problems = budget_problems(scan, &budget);
    assert!(
        problems.is_empty(),
        "interpreter budget ({}):\n{}",
        interpreters::BUDGET,
        problems.join("\n")
    );
}

/// The whole budget scan over planted trees: each planted defect the rule ships to
/// refuse, against a clean control tree.
mod budget {
    use super::*;
    use std::collections::BTreeMap;

    const CI: &str = ".github/workflows/ci.yml";
    const WORKFLOW: &str = "on: push\njobs:\n  test:\n    steps:\n      - run: cargo test --locked\n      - name: Probe\n        run: |\n          uname -m\n          nproc\n";
    const ACTION: &str = "runs:\n  using: composite\n  steps:\n    - run: a && b\n";
    const KEEPER: &str = "crates/k/src/keeper.rs";
    const KEEPER_RS: &str =
        r#"pub fn watch() { std::process::Command::new("/bin/sh").args(["-c", "x"]); }"#;
    const BUDGET: &str = "[files]\ntools/ci/x.sh\tCI\ntools/probe\tprobe\n\
                          [workflow]\n.github/workflows/ci.yml#test/Probe\tfacts\n\
                          .github/actions/a/action.yml#runs/step 1\tchain\n\
                          [src-sh-c]\ncrates/k/src/keeper.rs\tkeeper\n";

    /// The control tree: one row-matching thing of each kind, plus files the scan
    /// must leave alone.
    fn tree() -> BTreeMap<String, Vec<u8>> {
        [
            ("tools/ci/x.sh", "#!/bin/sh\necho\n"),
            ("tools/probe", "#!/usr/bin/env bash\n"),
            (CI, WORKFLOW),
            (".github/actions/a/action.yml", ACTION),
            (KEEPER, KEEPER_RS),
            ("crates/k/src/lib.rs", "pub mod keeper;\n"),
            ("crates/k/tests/t.rs", r#"fn t() { g("sh", "-c") }"#),
            ("README.md", "run `sh -c`\n"),
        ]
        .into_iter()
        .map(|(p, t)| (p.to_owned(), t.as_bytes().to_vec()))
        .collect()
    }

    fn problems(tree: &BTreeMap<String, Vec<u8>>, budget: &str) -> Vec<String> {
        let files: Vec<String> = tree.keys().cloned().collect();
        budget_problems(interpreter_scan(&files, |f| tree.get(f).cloned()), budget)
    }

    #[test]
    fn the_control_tree_passes() {
        // Catches: the control refused, so every planted defect below would go red
        // for the wrong reason.
        assert_eq!(problems(&tree(), BUDGET), Vec::<String>::new());
    }

    #[test]
    fn an_unlisted_sh_file_is_refused() {
        // Catches (planted defect): a new `x.sh` with no row.
        let mut t = tree();
        t.insert("tools/new.sh".to_owned(), b"echo\n".to_vec());
        let p = problems(&t, BUDGET);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].starts_with("`tools/new.sh`: a `.sh` file"), "{p:?}");
    }

    #[test]
    fn an_unlisted_extensionless_python_file_is_refused() {
        // Catches (planted defect): an extensionless `#!/usr/bin/env python3` with no
        // row, which an extension-only rule would pass.
        let mut t = tree();
        t.insert("tools/gen".to_owned(), b"#!/usr/bin/env python3\n".to_vec());
        let p = problems(&t, BUDGET);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            p[0].starts_with("`tools/gen`: its `#!` line runs `python3`"),
            "{p:?}"
        );
    }

    #[test]
    fn an_edit_of_a_listed_file_step_or_spawn_passes() {
        // Catches: the list tied to sizes again, so editing a listed script, a listed
        // `run:` step or a listed keeper would make the pull request touch the list.
        // The first edit grows each listed thing (a second shell for the keeper), the
        // second shrinks the script; neither adds anything unlisted.
        let step = WORKFLOW.replace("nproc\n", "nproc\n          sw_vers\n");
        let keeper = format!("{KEEPER_RS}\nfn again() {{ g(\"/bin/bash\", \"-c\") }}\n");
        for script in ["#!/bin/sh\necho\necho\necho\n", "#!/bin/sh\n"] {
            let mut t = tree();
            t.insert("tools/ci/x.sh".to_owned(), script.as_bytes().to_vec());
            t.insert(CI.to_owned(), step.clone().into_bytes());
            t.insert(KEEPER.to_owned(), keeper.clone().into_bytes());
            assert_eq!(problems(&t, BUDGET), Vec::<String>::new(), "{script:?}");
        }
    }

    #[test]
    fn a_row_for_a_deleted_file_is_refused() {
        // Catches (planted defect): a stale row, its file deleted.
        let mut t = tree();
        t.remove("tools/probe");
        assert_eq!(
            problems(&t, BUDGET),
            vec![
                "line 3: the [files] row for `tools/probe` names nothing the scan found \
                 (deleted, renamed, or no longer counted); remove the row"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn an_unlisted_multi_line_run_step_is_refused() {
        // Catches (planted defect): a new `run: |` block with no [workflow] row, and
        // a one-line command (the first step) refused.
        let mut t = tree();
        let planted = format!("{WORKFLOW}      - run: |\n          a\n          b\n");
        t.insert(CI.to_owned(), planted.into_bytes());
        let p = problems(&t, BUDGET);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            p[0].starts_with(
                "`.github/workflows/ci.yml#test/step 3`: its `run:` script has more than one line"
            ),
            "{p:?}"
        );
    }

    #[test]
    fn two_logic_steps_with_one_key_are_refused() {
        // Catches: a second step of the same name in one job cleared by the first
        // step's row, so its script is never counted.
        let mut t = tree();
        let planted = format!("{WORKFLOW}      - name: Probe\n        run: a && b\n");
        t.insert(CI.to_owned(), planted.into_bytes());
        assert_eq!(
            problems(&t, BUDGET),
            vec![
                "`.github/workflows/ci.yml#test/Probe`: two steps share this key; name them apart"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn an_unlisted_production_sh_c_is_refused() {
        // Catches (planted defect): `Command::new("sh").arg("-c")` in a src file with
        // no [src-sh-c] row; the same text under `crates/*/tests/` stays allowed.
        let mut t = tree();
        t.insert(
            "crates/k/src/run.rs".to_owned(),
            br#"fn f() { std::process::Command::new("sh").arg("-c"); }"#.to_vec(),
        );
        let p = problems(&t, BUDGET);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            p[0].starts_with("`crates/k/src/run.rs`: its production code names a shell"),
            "{p:?}"
        );
    }

    #[test]
    fn an_unreadable_workflow_or_source_is_refused_not_skipped() {
        // Catches: a workflow or Rust source the scan cannot read dropped from it, so
        // its shell escapes the budget.
        let mut t = tree();
        t.insert(CI.to_owned(), b"on: [\n".to_vec());
        t.insert(KEEPER.to_owned(), b"fn f( {".to_vec());
        let p = problems(&t, BUDGET);
        assert!(p.iter().any(|e| e.starts_with(&format!("{CI}: "))), "{p:?}");
        assert!(
            p.iter()
                .any(|e| e.starts_with(&format!("{KEEPER}: not read as Rust"))),
            "{p:?}"
        );
    }
}

/// The workflow paths `docs/artifacts.md` gives as `--signer-workflow`, relative to the
/// repository root.
fn documented_signer_workflows(doc: &str) -> Vec<&str> {
    const FLAG: &str = "--signer-workflow komira-ai/komira-build-farm/";
    doc.match_indices(FLAG)
        .map(|(at, _)| {
            doc[at + FLAG.len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
        })
        .collect()
}

/// What a workflow must hold to be the signer a node's verification names: an attest
/// step, under a job whose `if:` limits it to a push to `main`.
fn signs_provenance_on_main(workflow: &str) -> bool {
    workflow.contains("uses: actions/attest-build-provenance@")
        && workflow
            .contains("if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}")
}

#[test]
fn the_documented_signer_workflow_signs_provenance_on_main() {
    // Catches: the artifacts workflow renamed, moved or stripped of its attest step
    // while docs/artifacts.md still names it, so every node's verification command
    // names a workflow that signs nothing (and every deployment fails to verify).
    let root = repo_root();
    let doc = std::fs::read_to_string(root.join("docs/artifacts.md")).expect("docs/artifacts.md");
    let signers = documented_signer_workflows(&doc);
    assert!(
        !signers.is_empty(),
        "docs/artifacts.md names no --signer-workflow"
    );
    for path in signers {
        assert!(is_workflow(path), "`{path}` is not a workflow file");
        let text = std::fs::read_to_string(root.join(path))
            .unwrap_or_else(|e| panic!("the documented signer workflow `{path}`: {e}"));
        assert!(
            signs_provenance_on_main(&text),
            "`{path}` has no main-only attest step"
        );
    }
}

/// The workflow that builds the release files, and its macOS job.
const ARTIFACTS_WORKFLOW: &str = ".github/workflows/artifacts.yml";
const DARWIN_JOB: &str = "darwin-arm64";

/// The command that checks the packaged macOS binary's signature and runs it.
const CHECK_DARWIN_ASSET: &str = "bash tools/ci/check-darwin-asset.sh";

/// Whether `job` in `workflow` runs [`CHECK_DARWIN_ASSET`] in a step after the one named
/// `Package`, with no `if:` or `continue-on-error:` on that step; an error says what is
/// missing.
fn checks_the_packaged_darwin_binary(workflow: &str, job: &str) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, job)?;
    let package = steps
        .iter()
        .position(|s| s.name.as_deref() == Some("Package"))
        .ok_or_else(|| format!("the job `{job}` has no step named `Package`"))?;
    let runs_check = |run: &str| {
        run.lines()
            .map(str::trim_start)
            .any(|line| line.starts_with(&format!("{CHECK_DARWIN_ASSET} ")))
    };
    let check = steps[package + 1..]
        .iter()
        .find(|s| s.run.as_deref().is_some_and(runs_check))
        .ok_or_else(|| {
            format!("the job `{job}` does not run `{CHECK_DARWIN_ASSET}` after `Package`")
        })?;
    if check.conditional {
        return Err(format!(
            "the job `{job}` runs `{CHECK_DARWIN_ASSET}` under `if:` or `continue-on-error:`"
        ));
    }
    Ok(())
}

#[test]
fn the_darwin_job_checks_the_binary_it_packaged() {
    // Catches: the step that checks the packaged binary's signature and runs it (the
    // only check of the bytes that ship, docs/artifacts.md) dropped from the darwin
    // job, moved before Package, or made one whose failure does not fail the job.
    let root = repo_root();
    let text = std::fs::read_to_string(root.join(ARTIFACTS_WORKFLOW)).expect(ARTIFACTS_WORKFLOW);
    if let Err(e) = checks_the_packaged_darwin_binary(&text, DARWIN_JOB) {
        panic!("{ARTIFACTS_WORKFLOW}: {e}");
    }
}

/// The rules that pick which files each lint reads. The repository tests above cannot
/// pin them: a selection that misses a file leaves those tests green.
mod selection {
    use super::*;

    #[test]
    fn signer_workflows_are_read_from_the_verify_command() {
        // Catches: a signer path read with the line's trailing `\` or the next flag, so
        // the repository test opens the wrong file; another repository's workflow
        // taken as ours; a flag at the very end of the text read as a path.
        let doc = "gh attestation verify x \\\n  --signer-workflow komira-ai/komira-build-farm/.github/workflows/a.yml \\\n  --source-ref refs/heads/main\nalso --signer-workflow komira-ai/komira-build-farm/.github/workflows/b.yml";
        assert_eq!(
            documented_signer_workflows(doc),
            vec![".github/workflows/a.yml", ".github/workflows/b.yml"]
        );
        assert!(
            documented_signer_workflows("--signer-workflow other/repo/.github/workflows/a.yml")
                .is_empty()
        );
        assert_eq!(
            documented_signer_workflows("--signer-workflow komira-ai/komira-build-farm/"),
            vec![""]
        );
    }

    #[test]
    fn a_signer_workflow_needs_both_the_attest_step_and_the_main_only_job() {
        // Catches: a workflow accepted as the signer with its attest step gone, or with
        // the attest job no longer limited to a push to main.
        let step = "      - uses: actions/attest-build-provenance@0123 # v4\n";
        let cond =
            "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n";
        assert!(signs_provenance_on_main(&format!("{cond}{step}")));
        assert!(!signs_provenance_on_main(step));
        assert!(!signs_provenance_on_main(cond));
    }

    /// A darwin job: Package, then `check` as the next step's lines.
    fn darwin_workflow(check: &str) -> String {
        format!(
            "on: push\npermissions: {{}}\njobs:\n  darwin-arm64:\n    runs-on: macos-15\n    steps:\n      - name: Package\n        run: cargo xtask package --binary a --sbom b --platform darwin-arm64 --commit c --out dist\n{check}"
        )
    }

    #[test]
    fn the_darwin_asset_check_must_follow_package_and_gate_the_job() {
        // Catches: a darwin job without the check accepted (the lint passing a workflow
        // that misses it), the check found only before Package, in another job, in a
        // comment, or under `if:` / `continue-on-error:`.
        let check = "      - name: Check\n        run: |\n          bash tools/ci/check-darwin-asset.sh \\\n            dist/x.tar.gz kbf-daemon\n";
        assert_eq!(
            checks_the_packaged_darwin_binary(&darwin_workflow(check), "darwin-arm64"),
            Ok(())
        );
        let missing = "the job `darwin-arm64` does not run `bash tools/ci/check-darwin-asset.sh` after `Package`";
        let comment =
            "      - run: |\n          # bash tools/ci/check-darwin-asset.sh x y\n          true\n";
        for planted in [
            "",
            comment,
            "      - run: echo tools/ci/check-darwin-asset.sh\n",
        ] {
            assert_eq!(
                checks_the_packaged_darwin_binary(&darwin_workflow(planted), "darwin-arm64"),
                Err(missing.to_owned()),
                "{planted:?}"
            );
        }
        let before = darwin_workflow("").replace(
            "      - name: Package\n",
            &format!("{check}      - name: Package\n"),
        );
        assert_eq!(
            checks_the_packaged_darwin_binary(&before, "darwin-arm64"),
            Err(missing.to_owned())
        );
        assert_eq!(
            checks_the_packaged_darwin_binary(&darwin_workflow(check), "linux-arm64"),
            Err("the workflow has no job `linux-arm64`".to_owned())
        );
        for key in ["        if: false\n", "        continue-on-error: true\n"] {
            let gated = check.replace("        run: |\n", &format!("{key}        run: |\n"));
            assert!(
                checks_the_packaged_darwin_binary(&darwin_workflow(&gated), "darwin-arm64")
                    .is_err_and(|e| e.contains("under `if:` or `continue-on-error:`")),
                "{key:?}"
            );
        }
        let unnamed = darwin_workflow(check).replace("name: Package", "name: Pack");
        assert_eq!(
            checks_the_packaged_darwin_binary(&unnamed, "darwin-arm64"),
            Err("the job `darwin-arm64` has no step named `Package`".to_owned())
        );
    }

    #[test]
    fn action_files_are_selected_by_name_in_any_case() {
        // Catches: an action file matched case-sensitively, so `Action.YAML` (which
        // GitHub runs) escaped the action lint; or a match on a name prefix or suffix.
        for f in [
            "action.yml",
            "action.yaml",
            ".github/actions/t/Action.YAML",
            "tools/x/ACTION.yml",
        ] {
            assert!(is_action_file(f), "{f}");
        }
        for f in [
            "myaction.yml",
            "action.yml.bak",
            "action.json",
            "action.yml/README.md",
            ".github/workflows/ci.yml",
        ] {
            assert!(!is_action_file(f), "{f}");
        }
    }

    #[test]
    fn an_action_file_names_its_directory() {
        // Catches: a wrong directory recorded for an action, so a valid `uses: ./<dir>`
        // is refused, or the root action is keyed by its file name.
        assert_eq!(action_dir("Action.yml"), "");
        assert_eq!(
            action_dir(".github/actions/t/action.yaml"),
            ".github/actions/t"
        );
    }

    #[test]
    fn workflow_files_are_selected_by_extension_in_any_case() {
        // Catches: a workflow extension matched case-sensitively, so `x.YML` escaped
        // the workflow lint; or a file outside `.github/workflows/` linted as one.
        for f in [
            ".github/workflows/ci.yml",
            ".github/workflows/x.YML",
            ".github/workflows/y.Yaml",
        ] {
            assert!(is_workflow(f), "{f}");
        }
        for f in [
            ".github/workflows/README.md",
            ".github/workflows/ci.yml.orig",
            ".github/ci.yml",
            "docs/.github/workflows/ci.yml",
        ] {
            assert!(!is_workflow(f), "{f}");
        }
    }

    #[test]
    fn an_unreadable_workflow_or_action_is_refused_not_skipped() {
        // Catches: a workflow or action file holding a NUL byte dropped from the lint
        // without a problem, so its steps are never checked.
        let repo = workflows::Repo::default();
        let bytes = b"on: push\0\njobs: {}\n";
        for (f, linted) in [
            (".github/workflows/x.YML", Linted::Workflow),
            ("t/Action.yml", Linted::Action),
        ] {
            assert_eq!(
                lint_workflow_file(f, bytes, linted, &repo),
                vec![format!(
                    "{f}: line 0: the file holds a NUL byte, so it is not read"
                )],
                "{f}"
            );
        }
    }

    #[test]
    fn an_action_file_is_linted_as_an_action() {
        // Catches: an action file read by the workflow rules (or the reverse), which
        // refuses every valid action.
        let repo = workflows::Repo::default();
        let action = b"name: a\nruns: {using: node24, main: index.js}\n";
        assert_eq!(
            lint_workflow_file("action.yml", action, Linted::Action, &repo),
            Vec::<String>::new()
        );
        assert!(!lint_workflow_file("action.yml", action, Linted::Workflow, &repo).is_empty());
    }

    #[test]
    fn a_utf16_text_file_is_decoded_and_scanned() {
        // Catches: a UTF-16 file (its ASCII holds NUL bytes) skipped or refused rather
        // than decoded, or scanned as raw bytes; either hides an address from the scan.
        // The address is assembled at run time so this file does not trip the scan.
        let addr = format!("{}.1.2.3", 10);
        let mut bytes = vec![0xFF, 0xFE];
        for u in format!("host {addr}\n").encode_utf16() {
            bytes.extend(u.to_le_bytes());
        }
        assert_eq!(
            check_text_file("notes.txt", &bytes),
            TextFile::Scanned(vec![format!(
                "notes.txt: line 1: private IPv4 address {addr}"
            )])
        );
    }

    #[test]
    fn a_design_citation_is_reported_with_its_file_and_line() {
        // Catches: a citation of the unpublished design document by section passing the
        // repository scan, or reported without the file and line that hold it. The
        // acronym is assembled at run time so this file does not trip the scan.
        let cite = format!("R{} section 5.8", "FC");
        let text = format!("//! ok\n/// The fence ({cite}).\n");
        assert_eq!(
            check_text_file("crates/x/src/lib.rs", text.as_bytes()),
            TextFile::Scanned(vec![format!(
                "crates/x/src/lib.rs: line 2: `{cite})` cites an unpublished design \
                 document; cite a doc under docs/ instead"
            )])
        );
    }

    /// A conflict as git writes it, its markers assembled at run time so this file does
    /// not trip the scan.
    fn conflict(ours: &str, theirs: &str) -> String {
        let (o, s, t) = ("<".repeat(7), "=".repeat(7), ">".repeat(7));
        format!("{o} HEAD\n{ours}\n{s}\n{theirs}\n{t} topic\n")
    }

    /// What the scan reports for each marker of [`conflict`] at line 2 of `f`.
    fn conflict_findings(f: &str) -> Vec<String> {
        [(2, "<<<<<<<"), (4, "======="), (6, ">>>>>>>")]
            .into_iter()
            .map(|(line, m)| {
                format!("{f}: line {line}: git conflict marker `{m}`; finish the merge")
            })
            .collect()
    }

    #[test]
    fn a_conflict_in_rust_or_markdown_is_reported_with_its_file_and_line() {
        // Catches: a conflict left in a source file or a doc passing the repository
        // scan (Markdown exempted for its heading underlines, or only the opening
        // marker checked, or the separator passed as a heading in a text mid-merge),
        // or a marker reported without its file and line.
        for (f, ours, theirs) in [
            ("crates/x/src/lib.rs", "let x = 1;", "let x = 2;"),
            ("docs/design/x.md", "Title", "Heading"),
        ] {
            let text = format!("ok\n{}", conflict(ours, theirs));
            assert_eq!(
                check_text_file(f, text.as_bytes()),
                TextFile::Scanned(conflict_findings(f)),
                "{f}"
            );
        }
    }

    #[test]
    fn a_heading_underline_or_table_rule_is_not_a_conflict() {
        // Catches: a Markdown setext heading or a table rule refused as a conflict
        // marker, which would make every such doc unmergeable.
        let s = "=".repeat(7);
        let text = format!("Title\n{s}\n\n{s} =====\nkey   value\n{s} =====\n");
        for f in ["docs/x.md", "README.md", "notes.txt"] {
            assert_eq!(
                check_text_file(f, text.as_bytes()),
                TextFile::Scanned(vec![]),
                "{f}"
            );
        }
    }

    #[test]
    fn a_markdown_file_is_scanned() {
        // Catches: Markdown skipped by the text scan (no test pinned it before), so a
        // home path in a doc reached the public repository. The path is assembled at
        // run time so this file does not trip the scan.
        let home = format!("/{}/alice", "home");
        let text = format!("# Notes\n\nsee {home}/x\n");
        for f in ["docs/design/x.md", "README.md", "docs/X.MD"] {
            assert_eq!(
                check_text_file(f, text.as_bytes()),
                TextFile::Scanned(vec![format!("{f}: line 3: home path {home}")]),
                "{f}"
            );
        }
    }

    #[test]
    fn an_allow_listed_binary_file_is_skipped() {
        // Catches: an allow-listed binary refused, or scanned as text.
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        for f in ["logo.png", "img/Logo.PNG"] {
            assert_eq!(check_text_file(f, png), TextFile::Binary, "{f}");
        }
    }

    #[test]
    fn a_nul_text_file_off_the_allow_list_is_refused() {
        // Catches: a text file with a planted NUL skipped as binary, so the rest of its
        // text escapes the scan; or an allow-listed directory name or inner extension
        // taken as binary.
        let bytes = b"host text\0\n";
        for f in ["notes.txt", "Makefile", "dir.png/Makefile", "a.png.txt"] {
            let got = check_text_file(f, bytes);
            assert!(
                matches!(&got, TextFile::Refused(e)
                    if e.starts_with(&format!("{f}: ")) && e.contains("NUL byte")),
                "{f}: {got:?}"
            );
        }
    }
}
