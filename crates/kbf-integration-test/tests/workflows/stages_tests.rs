//! Tests of the stage file lint. Each says which defect it catches. The planted
//! defects work on the repository's real files, so a rule that stops matching reality
//! goes red; the unplanted text is the control.

use super::*;

/// The repository's stage file and every workflow file, as the real scan reads them.
fn real() -> (String, Vec<(String, String)>) {
    let root = crate::repo_root();
    let stages = std::fs::read_to_string(root.join(STAGE_FILE)).expect("stage file");
    let dir = root.join(WORKFLOW_DIR);
    let mut workflows: Vec<(String, String)> = std::fs::read_dir(&dir)
        .expect("workflow directory")
        .map(|e| {
            e.expect("entry")
                .file_name()
                .into_string()
                .expect("UTF-8 name")
        })
        .filter(|n| n.ends_with(".yml") || n.ends_with(".yaml"))
        .map(|n| {
            let text = std::fs::read_to_string(dir.join(&n)).expect("workflow text");
            (n, text)
        })
        .collect();
    workflows.sort();
    (stages, workflows)
}

/// The real workflows with `file`'s text replaced after `edit`, which must change it.
fn planted(
    workflows: &[(String, String)],
    file: &str,
    edit: impl Fn(&str) -> String,
) -> Vec<(String, String)> {
    let mut out = workflows.to_vec();
    let (_, text) = out.iter_mut().find(|(f, _)| f == file).expect("workflow");
    let new = edit(text);
    assert_ne!(*text, new, "the plant changed nothing in {file}");
    *text = new;
    out
}

fn refused(found: &[String], want: &str) {
    assert!(
        found.iter().any(|m| m.contains(want)),
        "want `{want}`: {found:#?}"
    );
}

#[test]
fn the_real_stage_file_matches_the_real_workflows() {
    // Catches: a stage file that drifts from the workflows (a job renamed, a trigger or
    // a signing scope added) and a scan that reads nothing: it must see the build and
    // prod jobs and at least four workflows.
    let (stages, workflows) = real();
    assert!(workflows.len() >= 4, "{} workflows read", workflows.len());
    assert!(stages.contains("artifacts.yml#attest-linux"));
    assert_eq!(check(&stages, &workflows), Vec::<String>::new());
}

#[test]
fn a_pull_request_trigger_on_the_release_workflow_is_refused() {
    // Catches: a stage workflow gaining `pull_request`, which the prod stage does not
    // allow. Control: the real text above.
    let (stages, workflows) = real();
    let bad = planted(&workflows, "release.yml", |t| {
        t.replacen(
            "on:\n  workflow_run:",
            "on:\n  pull_request:\n  workflow_run:",
            1,
        )
    });
    refused(
        &check(&stages, &bad),
        "release.yml: the trigger `pull_request` is allowed by no stage",
    );
}

#[test]
fn id_token_in_a_non_signing_job_is_refused() {
    // Catches: a push-only job that is not an attest job gaining `id-token: write`
    // (here verify-linux, also in a stage workflow), and the same in a workflow the
    // stage file does not list. Control: the attest jobs hold both scopes in the real text.
    let (stages, workflows) = real();
    let bad = planted(&workflows, "artifacts.yml", |t| {
        t.replacen(
            "      attestations: read\n",
            "      attestations: read\n      id-token: write\n",
            1,
        )
    });
    refused(
        &check(&stages, &bad),
        "job `verify-linux` can write `id-token` or `attestations`",
    );
    let bad = planted(&workflows, "ci.yml", |t| {
        t.replacen(
            "      contents: read\n",
            "      contents: read\n      attestations: write\n",
            1,
        )
    });
    refused(&check(&stages, &bad), "ci.yml: line");
}

#[test]
fn a_stage_whose_only_job_is_deleted_is_refused() {
    // Catches: a stage left empty. Delete every prod job from the stage file (the rows
    // go, the workflow keeps its jobs).
    let (stages, workflows) = real();
    let cut = stages
        .split("  - job: release.yml#plan")
        .next()
        .expect("text");
    let found = check(cut, &workflows);
    refused(&found, "stage `prod` is an `actions` stage with no job");
    // The release's gamma job holds `id-token: write`, which only a signing row may: with its
    // row gone it is named too, and nothing else is.
    refused(&found, "job `gamma` can write `id-token` or `attestations`");
    assert_eq!(found.len(), 2, "{found:#?}");
}

#[test]
fn a_job_naming_an_unknown_stage_is_refused() {
    // Catches: a row naming a stage the file does not list (a typo, or a stage deleted
    // while its jobs stay).
    let (stages, workflows) = real();
    let bad = stages.replacen("    stage: prod", "    stage: production", 1);
    refused(
        &check(&bad, &workflows),
        "names the unknown stage `production`",
    );
}

/// Two stages, one workflow with a signing job.
const MINI_STAGES: &str = "stages:\n  - name: build\n    runs: actions\n    after: []\n    result: r\n    events: [push]\n  - name: beta\n    runs: actions\n    planned: true\n    after: [build]\n    result: b\n  - name: ops\n    runs: operator\n    after: [beta]\n    result: o\njobs:\n  - job: w.yml#a\n    stage: build\n  - job: w.yml#sign\n    stage: build\n    signing: true\n";

fn mini_workflow() -> String {
    "on:\n  push:\njobs:\n  a:\n    runs-on: x\n  sign:\n    permissions:\n      id-token: write\n"
        .to_owned()
}

fn mini(stages: &str) -> Vec<String> {
    check(stages, &[("w.yml".to_owned(), mini_workflow())])
}

#[test]
fn the_mini_files_are_clean() {
    // The control for the tests below.
    assert_eq!(mini(MINI_STAGES), Vec::<String>::new());
}

#[test]
fn a_planned_stage_with_a_job_is_refused() {
    // Catches: `planned` outliving the gap it marks.
    let bad = MINI_STAGES.replacen(
        "    stage: build\n    signing",
        "    stage: beta\n    signing",
        1,
    );
    refused(
        &mini(&bad),
        "stage `beta` is marked `planned` but has a job",
    );
}

#[test]
fn an_actions_stage_without_a_job_or_plan_is_refused() {
    // Catches: removing `planned` before a job exists.
    let bad = MINI_STAGES.replacen("    planned: true\n", "", 1);
    refused(
        &mini(&bad),
        "stage `beta` is an `actions` stage with no job",
    );
}

#[test]
fn an_operator_stage_with_a_job_is_refused() {
    // Catches: an operator stage (outside Actions) given a workflow job.
    let bad = MINI_STAGES.replacen(
        "  - job: w.yml#a\n    stage: build",
        "  - job: w.yml#a\n    stage: ops",
        1,
    );
    refused(&mini(&bad), "stage `ops` is an `operator` stage");
}

#[test]
fn an_unlisted_job_and_a_missing_job_are_refused() {
    // Catches: a job added to a stage workflow without a row, and a row whose job is
    // gone (renamed).
    refused(
        &check(
            MINI_STAGES,
            &[(
                "w.yml".to_owned(),
                format!("{}  extra:\n    runs-on: x\n", mini_workflow()),
            )],
        ),
        "job `extra` is in no stage",
    );
    let gone = mini_workflow().replace("  a:\n", "  b:\n");
    let found = check(MINI_STAGES, &[("w.yml".to_owned(), gone)]);
    refused(&found, "`w.yml` has no job `a`");
    refused(
        &check(MINI_STAGES, &[]),
        "names a workflow file that is not in",
    );
}

#[test]
fn a_signing_row_that_does_not_sign_is_refused() {
    // Catches: a stale allowance (the job lost its scopes but stays listed).
    let text = mini_workflow().replace("id-token: write", "id-token: read");
    let found = check(MINI_STAGES, &[("w.yml".to_owned(), text)]);
    refused(&found, "job `sign` is a signing row");
}

#[test]
fn write_all_counts_as_signing() {
    // Catches: `permissions: write-all` carrying the scopes unnamed.
    let text = mini_workflow().replace(
        "    permissions:\n      id-token: write\n",
        "    permissions: write-all\n",
    );
    assert_eq!(
        check(MINI_STAGES, &[("w.yml".to_owned(), text.clone())]),
        Vec::<String>::new()
    );
    let text = text.replacen(
        "  a:\n    runs-on: x\n",
        "  a:\n    permissions: write-all\n",
        1,
    );
    refused(
        &check(MINI_STAGES, &[("w.yml".to_owned(), text)]),
        "job `a` can write",
    );
}

#[test]
fn a_stage_file_that_cannot_be_read_is_refused() {
    // Catches: a scan that passes because the file is empty, broken or the wrong shape.
    let w = [("w.yml".to_owned(), mini_workflow())];
    refused(&check("", &w), "no YAML document");
    refused(&check("a: [", &w), "not valid YAML");
    refused(&check("- 1\n", &w), "not a mapping");
    refused(
        &check("stages: []\njobs: []\n", &w),
        "`stages` is not a non-empty list",
    );
    refused(
        &check("stages: 1\n", &w),
        "`stages` is not a non-empty list",
    );
    refused(&check("jobs: []\n", &w), "no `stages` list");
    refused(
        &check(&format!("{MINI_STAGES}extra: 1\n"), &w),
        "an unknown key",
    );
    let broken = [("w.yml".to_owned(), "a: [".to_owned())];
    refused(&check(MINI_STAGES, &broken), "w.yml: ");
}

#[test]
fn malformed_stage_and_job_entries_are_refused() {
    // Catches: each shape error the parser reads: a stage entry that is not a mapping,
    // a repeated or unnamed stage, a bad `runs`, a missing or forward `after`, a bad
    // flag, a job row that is not `<file>#<id>` or repeats.
    let w = [("w.yml".to_owned(), mini_workflow())];
    let f = |from: &str, to: &str| mini(&MINI_STAGES.replacen(from, to, 1));
    refused(
        &f("  - name: build\n", "  - 3\n  - name: build\n"),
        "an entry is not a mapping",
    );
    refused(
        &f("  - name: ops\n", "  - name: build\n"),
        "stage `build` is listed twice",
    );
    refused(&f("    result: r\n", ""), "needs a `name` and a `result`");
    refused(&f("runs: operator", "runs: laptop"), "is not one of");
    refused(&f("    after: []\n", ""), "needs an `after:` list");
    refused(
        &f("after: []", "after: [beta]"),
        "comes after `beta`, which is not listed before it",
    );
    refused(
        &f("planned: true", "planned: soon"),
        "`planned` is not `true` or `false`",
    );
    assert_eq!(
        f("planned: true", "planned: false")
            .iter()
            .filter(|m| m.contains("planned"))
            .count(),
        0
    );
    refused(&f("w.yml#a", "w.yml"), "is not `<workflow file>#<job id>`");
    refused(
        &f("w.yml#a", "sub/w.yml#a"),
        "is not `<workflow file>#<job id>`",
    );
    refused(&f("w.yml#a", "w.yml#sign"), "is listed twice");
    refused(
        &check(&MINI_STAGES.replacen("jobs:\n", "jobs:\n  - 3\n", 1), &w),
        "an entry is not a mapping",
    );
    refused(
        &f("stage: build\n    signing", "stage: build\n    signin"),
        "an unknown key",
    );
}

#[test]
fn a_workflow_that_cannot_be_read_as_a_mapping_is_refused() {
    // Catches: a workflow with no jobs, or not a mapping, skipped silently.
    refused(
        &check(
            MINI_STAGES,
            &[("w.yml".to_owned(), "on: push\n".to_owned())],
        ),
        "no `jobs` mapping",
    );
    refused(
        &check(MINI_STAGES, &[("w.yml".to_owned(), "- 1\n".to_owned())]),
        "not a mapping",
    );
    refused(
        &check(
            MINI_STAGES,
            &[("w.yml".to_owned(), "jobs:\n  a: 1\n".to_owned())],
        ),
        "no `on:`",
    );
}
