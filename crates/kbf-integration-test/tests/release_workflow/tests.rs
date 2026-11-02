//! Tests of the release workflow lint. The real `release.yml` is the control (no problem);
//! each other test plants one defect on its text and names the rule it catches. A plant must
//! change the text, and it is made inside the one job it names, so a rule whose anchor text
//! moved, or that only read another job, cannot go quiet.

use super::*;

const PATH: &str = ".github/workflows/release.yml";

fn real() -> String {
    std::fs::read_to_string(crate::repo_root().join(PATH)).expect(PATH)
}

/// `text` with `from` replaced by `to` inside the job `job` only (its header line to the next
/// job's, comments included), which must hold `from` exactly once.
fn in_job(text: &str, job: &str, from: &str, to: &str) -> String {
    let header = format!("  {job}:");
    let all: Vec<&str> = text.split_inclusive('\n').collect();
    let start = all
        .iter()
        .position(|l| l.trim_end() == header)
        .unwrap_or_else(|| panic!("no job `{job}`"));
    let end = all[start + 1..]
        .iter()
        .position(|l| l.starts_with("  ") && !l.starts_with("   ") && l.trim_end().ends_with(':'))
        .map_or(all.len(), |n| start + 1 + n);
    let region = all[start..end].concat();
    assert_eq!(
        region.matches(from).count(),
        1,
        "job `{job}` holds `{from}` {} times",
        region.matches(from).count()
    );
    [
        all[..start].concat(),
        region.replace(from, to),
        all[end..].concat(),
    ]
    .concat()
}

/// The problems of the real workflow with `from` replaced by `to` in the job `job`.
fn plant(job: &str, from: &str, to: &str) -> Vec<String> {
    problems(&in_job(&real(), job, from, to)).expect("the planted workflow is read")
}

/// The problems of the real workflow with `from` replaced by `to` once, anywhere.
fn plant_top(from: &str, to: &str) -> Vec<String> {
    let text = real();
    assert_eq!(text.matches(from).count(), 1, "`{from}` is not unique");
    problems(&text.replace(from, to)).unwrap_or_else(|e| vec![e])
}

fn red(found: &[String], want: &str) {
    assert!(
        found.iter().any(|p| p.contains(want)),
        "want a problem with `{want}`: {found:#?}"
    );
}

#[test]
fn the_real_release_workflow_meets_every_rule() {
    // Catches: release.yml drifting from the automatic release (a missing gate, a write scope
    // outside publish, a token in the wrong job), and the control for every plant below.
    let found = problems(&real()).expect("release.yml is read");
    assert!(found.is_empty(), "{PATH}:\n{}", found.join("\n"));
}

#[test]
fn only_the_completion_of_a_pipeline_run_for_main_starts_the_workflow() {
    // Catches: a release started by a pull request, a schedule, a tag, a push, a dispatch,
    // by the completion of another workflow, by a run that is not yet complete, or by a
    // pipeline run of another branch.
    let on = "on:\n  workflow_run:\n    workflows: [pipeline]\n    types: [completed]\n    branches: [main]\n";
    red(
        &plant_top(
            on,
            "on:\n  push:\n    branches: [main]\n  workflow_run:\n    workflows: [pipeline]\n    types: [completed]\n    branches: [main]\n",
        ),
        "runs on `push`",
    );
    red(
        &plant_top(
            on,
            "on:\n  workflow_dispatch:\n  workflow_run:\n    workflows: [pipeline]\n    types: [completed]\n    branches: [main]\n",
        ),
        "runs on `workflow_dispatch`",
    );
    red(
        &plant_top(
            on,
            "on:\n  pull_request:\n  workflow_run:\n    workflows: [pipeline]\n    types: [completed]\n    branches: [main]\n",
        ),
        "runs on `pull_request`",
    );
    red(
        &plant_top(
            on,
            "on:\n  schedule:\n    - cron: '0 0 * * *'\n  workflow_run:\n    workflows: [pipeline]\n    types: [completed]\n    branches: [main]\n",
        ),
        "runs on `schedule`",
    );
    red(
        &plant_top(on, "on:\n  workflow_dispatch:\n"),
        "does not run on `workflow_run`",
    );
    for (from, to) in [
        ("workflows: [pipeline]", "workflows: [ci]"),
        ("workflows: [pipeline]", "workflows: [pipeline, ci]"),
        ("types: [completed]", "types: [requested]"),
        ("types: [completed]", "types: [completed, requested]"),
        ("branches: [main]", "branches: ['**']"),
        ("    branches: [main]\n", ""),
    ] {
        red(&plant_top(from, to), "is filtered by");
    }
}

#[test]
fn a_workflow_run_trigger_needs_the_whole_chain_of_jobs() {
    // Catches: the trigger present while a stage of the chain is missing, so a pipeline run
    // would tag or publish without it. Each of plan, verify, tag, publish and bump is renamed
    // away in turn (each must name the missing job, as the trigger is present). `triggers` is
    // read alone: the later rules stop at a missing job with an error, and the chain rule must
    // stand on its own when they do not. The list itself is pinned here, not read back from
    // `CHAIN`: shrinking `CHAIN` must not shrink this test with it.
    assert_eq!(
        CHAIN,
        ["plan", "verify", "tag", "publish", "bump"],
        "the jobs a release needs"
    );
    let text = real();
    let mut control = Vec::new();
    triggers(&text, &mut control).expect("triggers read");
    assert!(control.is_empty(), "{control:?}");
    for job in CHAIN {
        let from = format!("\n  {job}:\n");
        assert_eq!(text.matches(&from).count(), 1, "`{job}` is not unique");
        let mut found = Vec::new();
        triggers(
            &text.replace(&from, &format!("\n  {job}-gone:\n")),
            &mut found,
        )
        .expect("triggers read");
        red(
            &found,
            &format!("runs on `workflow_run` but has no job `{job}`"),
        );
    }
}

#[test]
fn no_gates_job_exists_and_nothing_polls_for_other_runs() {
    // Catches: the retired wait on separate push runs of ci, beta and artifacts coming back:
    // a job `gates`, its verb, or a search for a run by workflow and commit.
    let text = real();
    assert!(!JOBS.contains(&"gates"));
    assert!(!text.contains("release gates"), "the retired verb");
    assert!(!code(&text).contains("  gates:"), "a job named gates");
    assert!(!code(&text).contains("needs.gates"), "a need of gates");
    red(
        &plant_top(
            "  plan:\n",
            "  gates:\n    runs-on: ubuntu-24.04\n    steps: []\n\n  plan:\n",
        ),
        "the jobs are",
    );
}

#[test]
fn the_job_list_is_pinned() {
    // Catches: the chain changed without its rule. In particular: the job `publish` renamed; a job
    // renamed; a job added.
    red(
        &plant_top("  publish:\n", "  publish-release:\n"),
        "no job `publish`",
    );
    red(&plant_top("  notes:\n", "  note:\n"), "no job `notes`");
    red(
        &plant_top(
            "  bump:\n",
            "  extra:\n    runs-on: ubuntu-24.04\n    steps: []\n\n  bump:\n",
        ),
        "the jobs are",
    );
}

#[test]
fn one_release_at_a_time_and_none_cancelled() {
    // Catches: two releases racing for the same version, a release cancelled half way, and a
    // pipeline run that did not succeed (it starts this workflow too, to skip every job)
    // queued in the group of the releases, where it would replace a release waiting there.
    red(
        &plant_top(
            "release-${{ github.event.workflow_run.conclusion == 'success' && 'main' || github.run_id }}",
            "release-main",
        ),
        "the concurrency is",
    );
    red(
        &plant_top(
            "release-${{ github.event.workflow_run.conclusion == 'success' && 'main' || github.run_id }}",
            "release-${{ github.run_id }}",
        ),
        "the concurrency is",
    );
    red(
        &plant_top("cancel-in-progress: false", "cancel-in-progress: true"),
        "cancel-in-progress: false",
    );
    red(
        &plant_top("  cancel-in-progress: false\n", ""),
        "the concurrency is",
    );
}

#[test]
fn every_job_needs_exactly_what_the_graph_says() {
    // Catches: a job that runs without a stage before it. `plan` dropped from verify; verify
    // from tag; tag from notes; notes from publish; publish from bump (and so the checks are
    // no longer reached from bump through any other job).
    red(
        &plant("verify", "needs: [plan]\n", "needs: []\n"),
        "`verify` needs",
    );
    red(
        &plant("tag", "needs: [plan, verify]\n", "needs: [plan]\n"),
        "`tag` needs",
    );
    red(
        &plant(
            "notes",
            "needs: [plan, verify, tag]\n",
            "needs: [plan, verify]\n",
        ),
        "`notes` needs",
    );
    red(
        &plant(
            "publish",
            "needs: [plan, verify, tag, notes]\n",
            "needs: [plan, verify, tag]\n",
        ),
        "`publish` needs",
    );
    let found = plant("bump", "needs: [plan, tag, publish]\n", "needs: [plan]\n");
    red(&found, "`bump` needs");
    red(
        &found,
        "`bump` does not need `verify`, directly or through another job",
    );
}

#[test]
fn only_publish_writes_and_no_job_signs() {
    // Catches: a write scope outside publish (a dry run could publish; a token that could
    // push), and an OIDC token in a job that runs a third party's code. Catches: verify,
    // tag and notes get `contents: write`; publish gets `packages: write`, then `id-token:
    // write`; plan gets `id-token: write`; publish loses `contents: write`.
    red(
        &plant(
            "verify",
            "      contents: read\n",
            "      contents: write\n",
        ),
        "`verify` holds `contents: write`",
    );
    red(
        &plant("tag", "      contents: read\n", "      contents: write\n"),
        "`tag` holds `contents: write`",
    );
    red(
        &plant("notes", "      contents: read\n", "      contents: write\n"),
        "`notes` holds `contents: write`",
    );
    red(
        &plant(
            "publish",
            "      contents: write\n",
            "      contents: write\n      packages: write\n",
        ),
        "`publish` holds `packages: write`",
    );
    red(
        &plant(
            "publish",
            "      contents: write\n",
            "      contents: write\n      id-token: write\n",
        ),
        "`publish` holds `id-token: write`, which only",
    );
    red(
        &plant(
            "plan",
            "      actions: read\n",
            "      actions: read\n      id-token: write\n",
        ),
        "`plan` holds `id-token: write`, which only",
    );
    red(
        &plant(
            "publish",
            "      contents: write\n",
            "      contents: read\n",
        ),
        "`publish` holds the scopes",
    );
}

#[test]
fn the_jobs_that_write_run_only_for_a_successful_pipeline_run_that_releases() {
    // Catches: a release of a pipeline run that was cancelled or red, of a pull request's run,
    // of another branch or repository; a release of docs only or of the pipeline itself.
    // Each clause of the condition of each writing job dropped in turn, the condition widened,
    // or the condition dropped.
    for job in ["tag", "notes", "publish", "bump"] {
        let line = format!("    if: {RELEASE_IF}\n");
        red(
            &plant(job, &line, ""),
            &format!("the job `{job}` has `if: (none)`"),
        );
        red(
            &plant(job, &line, "    if: ${{ github.event_name == 'push' }}\n"),
            &format!("the job `{job}` has `if:"),
        );
        for clause in [
            " && needs.plan.outputs.pipeline != 'true'",
            " && needs.plan.outputs.code == 'true'",
            "github.event.workflow_run.conclusion == 'success' && ",
            "github.event.workflow_run.event == 'push' && ",
            "github.event.workflow_run.head_branch == 'main' && ",
            "github.event.workflow_run.head_repository.full_name == github.repository && ",
        ] {
            red(
                &plant(job, clause, ""),
                &format!("the job `{job}` has `if:"),
            );
        }
    }
    let verify = format!("    if: {VERIFY_IF}\n");
    red(
        &plant("verify", &verify, ""),
        "the job `verify` has `if: (none)`",
    );
    let plan = format!("    if: {PLAN_IF}\n");
    red(&plant("plan", &plan, ""), "the job `plan` has `if: (none)`");
    red(
        &plant(
            "plan",
            "github.event.workflow_run.conclusion == 'success' && ",
            "",
        ),
        "the job `plan` has `if:",
    );
}

#[test]
fn the_environment_is_declared_where_the_key_is_read_and_where_writes_happen() {
    // Catches: a key move to an environment secret that would stop working, or a job that
    // reads the key outside the environment. Catches: the environment dropped from tag,
    // publish and bump; another environment; one added to notes.
    for job in ["tag", "publish", "bump"] {
        red(
            &plant(job, "    environment: release\n", ""),
            &format!("the job `{job}` has environment None"),
        );
        red(
            &plant(job, "environment: release", "environment: prod"),
            &format!("the job `{job}` has environment Some(\"prod\")"),
        );
    }
    red(
        &plant(
            "notes",
            "    permissions:\n",
            "    environment: release\n    permissions:\n",
        ),
        "the job `notes` has environment",
    );
}

#[test]
fn a_red_job_stops_the_release() {
    // Catches: a stage that is skipped or ignored. In particular: `always()` on publish;
    // `failure()`; `cancelled()`; `continue-on-error` on a gate step and on the tag step; an
    // `if:` on a step of tag, of verify and of notes (not the previous release's).
    red(
        &plant_top(
            &format!("    if: {RELEASE_IF}\n    runs-on: ubuntu-24.04\n    timeout-minutes: 20\n"),
            "    if: ${{ always() && github.event_name == 'push' }}\n    runs-on: ubuntu-24.04\n    timeout-minutes: 20\n",
        ),
        "uses `always()`",
    );
    red(
        &plant(
            "bump",
            "    environment: release\n",
            "    environment: release\n    continue-on-error: true\n",
        ),
        "uses `continue-on-error`",
    );
    red(
        &plant(
            "verify",
            "      - run: rustup toolchain install\n",
            "      - run: rustup toolchain install\n        if: ${{ failure() }}\n",
        ),
        "uses `failure()`",
    );
    red(
        &plant(
            "tag",
            "      - run: rustup toolchain install\n",
            "      - run: rustup toolchain install\n        if: ${{ cancelled() }}\n",
        ),
        "uses `cancelled()`",
    );
    red(
        &plant(
            "tag",
            "      - run: rustup toolchain install\n",
            "      - run: rustup toolchain install\n        if: ${{ github.event_name == 'push' }}\n",
        ),
        "a step of `tag` (",
    );
    red(
        &plant(
            "verify",
            "      - run: rustup toolchain install\n",
            "      - run: rustup toolchain install\n        if: ${{ github.event_name == 'push' }}\n",
        ),
        "a step of `verify` (",
    );
    red(
        &plant(
            "notes",
            "      - name: This release's identity, from the verified linux tarball\n",
            "      - name: This release's identity, from the verified linux tarball\n        if: ${{ github.event_name == 'push' }}\n",
        ),
        "a step of `notes` (",
    );
}

#[test]
fn only_the_commit_the_pipeline_built_is_checked_out_and_no_input_reaches_a_script() {
    // Catches: a release of code a person or a pull request named, or of the latest commit of
    // main (the `github.sha` of a `workflow_run` run) instead of the one the pipeline built.
    // A checkout of another ref, of the default ref, of another repository, with a token;
    // without `persist-credentials: false`; two checkouts; an expression of an input, of a
    // pull request, of `head_ref`, of the title or branch name of the run that started this.
    let checkout = "          ref: ${{ github.event.workflow_run.head_sha }}\n          persist-credentials: false\n";
    for job in ["verify", "tag", "publish", "bump"] {
        red(
            &plant(
                job,
                checkout,
                "          ref: main\n          persist-credentials: false\n",
            ),
            &format!("the checkout of `{job}` is not of `ref:"),
        );
        red(
            &plant(job, checkout, "          persist-credentials: false\n"),
            &format!("the checkout of `{job}` is not of `ref:"),
        );
    }
    red(
        &plant(
            "tag",
            checkout,
            "          ref: ${{ github.event.workflow_run.head_sha }}\n          repository: other/repo\n          persist-credentials: false\n",
        ),
        "is given `repository`",
    );
    red(
        &plant(
            "tag",
            checkout,
            "          ref: ${{ github.event.workflow_run.head_sha }}\n          token: ${{ github.token }}\n          persist-credentials: false\n",
        ),
        "is given `token`",
    );
    red(
        &plant(
            "publish",
            checkout,
            "          ref: ${{ github.event.workflow_run.head_sha }}\n          fetch-depth: 0\n",
        ),
        "the checkout of `publish` is not `persist-credentials: false`",
    );
    red(
        &plant(
            "tag",
            "      - run: rustup toolchain install\n",
            "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n        with:\n          ref: ${{ github.event.workflow_run.head_sha }}\n          persist-credentials: false\n      - run: rustup toolchain install\n",
        ),
        "has 2 checkouts",
    );
    for expr in [
        "${{ inputs.commit }}",
        "${{ github.event.inputs.commit }}",
        "${{ github.event.pull_request.head.sha }}",
        "${{ github.head_ref }}",
        "${{ github.event.workflow_run.head_commit.message }}",
        "${{ github.event.workflow_run.display_title }}",
        "${{ github.event.workflow_run.pull_requests[0].number }}",
    ] {
        red(
            &plant(
                "plan",
                "          GH_TOKEN: ${{ github.token }}\n",
                &format!("          GH_TOKEN: ${{{{ github.token }}}}\n          X: {expr}\n"),
            ),
            "the workflow reads `",
        );
    }
    red(
        &plant(
            "plan",
            "          GH_TOKEN: ${{ github.token }}\n",
            "          GH_TOKEN: ${{ github.token }}\n          X: ${{ github.event.workflow_run.head_branch }}\n",
        ),
        "outside an `if:`",
    );
}

#[test]
fn the_commit_and_the_run_are_the_pipeline_runs_for_every_job() {
    // Catches: a job acting on `github.sha` (the latest commit of main for a `workflow_run`
    // run), or on a run other than the one that started this, because the workflow-level
    // names point elsewhere or a job sets its own.
    red(
        &plant_top(
            "  COMMIT: ${{ github.event.workflow_run.head_sha }}\n",
            "  COMMIT: ${{ github.sha }}\n",
        ),
        "does not set `COMMIT:",
    );
    red(
        &plant_top(
            "  PIPELINE_RUN: ${{ github.event.workflow_run.id }}\n",
            "  PIPELINE_RUN: ${{ github.run_id }}\n",
        ),
        "does not set `PIPELINE_RUN:",
    );
    red(
        &plant(
            "notes",
            "    env:\n      TAG:",
            "    env:\n      COMMIT: ${{ github.sha }}\n      TAG:",
        ),
        "sets `COMMIT` or `PIPELINE_RUN` itself",
    );
}

#[test]
fn the_release_bots_key_is_read_in_tag_and_bump_only() {
    // Catches: the one key that can create `v*` tags (and open pull requests in the internal
    // repository) readable by a job that runs more code. Catches: the key read in publish,
    // notes, plan; the token minted in verify; the old `RELEASE_TAGGER_*` names; the freeze
    // read outside plan; another secret read; the key not read by tag and bump.
    for job in ["publish", "notes", "plan", "verify"] {
        let (from, to) = if job == "publish" {
            (
                "          GH_TOKEN: ${{ github.token }}\n          TAG:",
                "          GH_TOKEN: ${{ github.token }}\n          K: ${{ secrets.RELEASE_BOT_PRIVATE_KEY }}\n          TAG:",
            )
        } else {
            (
                "      - run: rustup toolchain install\n",
                "      - run: rustup toolchain install\n      - env:\n          K: ${{ secrets.RELEASE_BOT_PRIVATE_KEY }}\n        run: true\n",
            )
        };
        red(
            &plant(job, from, to),
            &format!("the job `{job}` reads `RELEASE_BOT_*`"),
        );
    }
    red(
        &plant(
            "notes",
            "      - run: rustup toolchain install\n",
            "      - uses: actions/create-github-app-token@bcd2ba49218906704ab6c1aa796996da409d3eb1 # v3.2.0\n        with:\n          app-id: 1\n          private-key: x\n      - run: rustup toolchain install\n",
        ),
        "the job `notes` mints an App token",
    );
    red(
        &plant(
            "tag",
            "vars.RELEASE_BOT_APP_ID",
            "vars.RELEASE_TAGGER_APP_ID",
        ),
        "names `RELEASE_TAGGER_*`",
    );
    red(
        &plant(
            "bump",
            "secrets.RELEASE_BOT_PRIVATE_KEY",
            "secrets.OTHER_KEY",
        ),
        "reads a secret other than",
    );
    red(
        &plant(
            "verify",
            "      - run: rustup toolchain install\n",
            "      - run: rustup toolchain install\n      - run: echo \"${{ vars.RELEASE_FREEZE }}\"\n",
        ),
        "the job `verify` reads `RELEASE_FREEZE`",
    );
    red(
        &plant("plan", "${{ vars.RELEASE_FREEZE }}", ""),
        "does not read `vars.RELEASE_FREEZE`",
    );
}

#[test]
fn the_plan_job_sees_the_history_and_the_freeze_and_hands_on_the_class() {
    // Catches: a plan that cannot see the history or the tags (it measures from the last
    // release tag), that ignores the stop switch, or whose classification never reaches the
    // jobs that act on it. No full history; the commit planned not the pipeline's; the
    // freeze dropped, then the variable; each output dropped.
    red(
        &plant("plan", "          fetch-depth: 0\n", ""),
        "does not check out with `fetch-depth: 0`",
    );
    red(
        &plant("plan", "--sha \"$COMMIT\"", "--sha \"$GITHUB_SHA\""),
        "not given `--sha $COMMIT`",
    );
    red(
        &plant("plan", " --freeze \"$FREEZE\"", ""),
        "not given `--freeze $FREEZE`",
    );
    red(
        &plant("plan", "FREEZE: ${{ vars.RELEASE_FREEZE }}", "FREEZE: ''"),
        "does not set `FREEZE:",
    );
    for name in ["version", "tag", "code", "site", "pipeline"] {
        let line = format!("      {name}: ${{{{ steps.plan.outputs.{name} }}}}\n");
        red(
            &plant("plan", &line, ""),
            &format!("does not output `{name}`"),
        );
    }
}

#[test]
fn verify_reads_the_artifacts_of_the_pipeline_run_that_started_the_release() {
    // Catches: files from another run than the one that built the commit, or unchecked files
    // uploaded for the tag to name. `locate` dropped, under `if:`, or without the run;
    // a download of another run, or without the token that reads a run's artifacts;
    // `release check` and the attestations dropped; the upload moved before them.
    red(
        &plant(
            "verify",
            "cargo xtask release locate",
            "cargo xtask release locat",
        ),
        "does not run `cargo xtask release locate`",
    );
    red(
        &plant(
            "verify",
            "      - name: Refuse a commit not on main, or a build that lost an artifact\n        env:",
            "      - name: Refuse a commit not on main, or a build that lost an artifact\n        if: ${{ false }}\n        env:",
        ),
        "`release locate` step of `verify` is under `if:`",
    );
    red(
        &plant("verify", " --run-id \"$PIPELINE_RUN\"", ""),
        "not given `--run-id $PIPELINE_RUN`",
    );
    red(
        &plant(
            "verify",
            "          name: sha256sums\n          path: dist\n          run-id: ${{ github.event.workflow_run.id }}\n",
            "          name: sha256sums\n          path: dist\n          run-id: 1\n",
        ),
        "a download of `verify` is not of run `${{ github.event.workflow_run.id }}`",
    );
    red(
        &plant(
            "verify",
            "          name: sha256sums\n          path: dist\n          run-id: ${{ github.event.workflow_run.id }}\n          github-token: ${{ github.token }}\n",
            "          name: sha256sums\n          path: dist\n          run-id: ${{ github.event.workflow_run.id }}\n",
        ),
        "names a `run-id` without `github-token",
    );
    red(
        &plant(
            "verify",
            "cargo xtask release check dist --commit \"$COMMIT\"",
            "cargo xtask release check dist",
        ),
        "not given `--commit $COMMIT`",
    );
    red(
        &plant(
            "verify",
            "cargo xtask verify-assets attestations dist",
            "cargo xtask verify-assets attestation dist",
        ),
        "does not verify the attestations after `release check`",
    );
    red(
        &plant(
            "verify",
            "          name: release-assets\n",
            "          name: assets\n",
        ),
        "does not upload `dist/` as `release-assets`",
    );
}

#[test]
fn tag_writes_the_record_then_mints_the_token_then_tags_the_commit_of_the_run() {
    // Catches: a tag of another commit or version, a token held longer or wider than the one
    // step needs, a record that names another run than the pipeline run that started this, a
    // record whose inputs come from the wrong place; the order changed; the token given `issues: write`, no
    // repository scope, or the wrong one; a build after it; the tag step on the workflow's
    // own token, with another message file, tag or sha; another step holding the token.
    let record = "--commit \"$COMMIT\" --tag \"$TAG\" --run-id";
    red(
        &plant("tag", record, "--commit \"$COMMIT\" --run-id"),
        "not given `--tag $TAG`",
    );
    red(
        &plant(
            "tag",
            "--pipeline-run \"$PIPELINE_RUN\"",
            "--pipeline-run \"$GITHUB_RUN_ID\"",
        ),
        "not given `--pipeline-run $PIPELINE_RUN`",
    );
    red(
        &plant(
            "tag",
            "TAG: ${{ needs.plan.outputs.tag }}\n        run: cargo xtask release verdict-write",
            "TAG: v9.9.9\n        run: cargo xtask release verdict-write",
        ),
        "does not set `TAG:",
    );
    red(
        &plant(
            "tag",
            "          permission-contents: write\n",
            "          permission-contents: write\n          permission-issues: write\n",
        ),
        "is minted with",
    );
    red(
        &plant(
            "tag",
            "          repositories: ${{ github.event.repository.name }}\n",
            "",
        ),
        "is minted with",
    );
    red(
        &plant(
            "tag",
            "${{ github.event.repository.name }}",
            "komira-internal",
        ),
        "is minted with",
    );
    red(
        &plant(
            "tag",
            "GH_TOKEN: ${{ steps.bot.outputs.token }}",
            "GH_TOKEN: ${{ github.token }}",
        ),
        "does not set `GH_TOKEN:",
    );
    red(
        &plant(
            "tag",
            "--message-file verdict.json",
            "--message-file other.json",
        ),
        "is not given `--message-file verdict.json`",
    );
    red(
        &plant(
            "tag",
            "--sha \"$COMMIT\" --tag \"$TAG\" --message-file",
            "--tag \"$TAG\" --message-file",
        ),
        "not given `--sha $COMMIT`",
    );
    red(
        &plant("tag", "cargo xtask release tag", "cargo xtask release tagx"),
        "does not run `cargo xtask release tag`",
    );
    red(
        &plant("tag", "          path: dist\n", "          path: other\n"),
        "does not download `release-assets` into `dist`",
    );
    // The record after the token: the verdict step moved below the mint step.
    let text = real();
    let step = text
        .split("      # The Beta record is the tag's message")
        .nth(1)
        .and_then(|rest| rest.split("      # The token of the release bot").next())
        .expect("the record step");
    let moved = text
        .replace(&format!("      # The Beta record is the tag's message{step}"), "")
        .replace(
            "      - name: Create the annotated tag",
            &format!("      # The Beta record is the tag's message{step}      - name: Create the annotated tag"),
        );
    assert_ne!(moved, text);
    red(
        &problems(&moved).expect("read"),
        "does not write the record, then mint the token, then tag",
    );
    // A build after the token is minted.
    let late = text.replace(
        "      - name: Create the annotated tag",
        "      - run: cargo build -p xtask --locked\n      - name: Create the annotated tag",
    );
    red(
        &problems(&late).expect("read"),
        "builds after the token is minted",
    );
    // A second step holding the token (not the last one).
    let held = text.replace(
        "      - name: Create the annotated tag",
        "      - run: true\n        env:\n          T: ${{ steps.bot.outputs.token }}\n      - name: Create the annotated tag",
    );
    red(&problems(&held).expect("read"), "holds the bot's token");
}

#[test]
fn notes_check_the_tag_and_hand_on_the_set_and_the_notes() {
    // Catches: notes for another release, a set hash that is not the one the tag check
    // printed, a tag check before the files are checked or against another main. Catches:
    // `release verify` dropped; its flags; its output; a main ref; the check order; the
    // notes dropped or for other flags; the upload.
    red(
        &plant(
            "notes",
            "cargo xtask release verify dist",
            "cargo xtask release check dist",
        ),
        "does not run `cargo xtask release verify`",
    );
    red(
        &plant(
            "notes",
            "release verify dist --tag \"$TAG\"",
            "release verify dist",
        ),
        "not given `--tag $TAG`",
    );
    red(
        &plant(
            "notes",
            "release verify dist --tag \"$TAG\" --commit \"$COMMIT\"",
            "release verify dist --tag \"$TAG\"",
        ),
        "not given `--commit $COMMIT`",
    );
    red(
        &plant(
            "notes",
            "--output \"$GITHUB_OUTPUT\"\n      # What the release changes",
            "--main-ref origin/next --output \"$GITHUB_OUTPUT\"\n      # What the release changes",
        ),
        "main ref other than `origin/main`",
    );
    red(
        &plant(
            "notes",
            "      set-hash: ${{ steps.verify-tag.outputs.set-hash }}\n",
            "      other: x\n",
        ),
        "does not output `set-hash`",
    );
    red(
        &plant(
            "notes",
            "cargo xtask release check dist",
            "cargo xtask release checkdist dist",
        ),
        "does not come after `release check`",
    );
    red(
        &plant("notes", "          fetch-depth: 0\n", ""),
        "does not check out with `fetch-depth: 0`",
    );
    red(
        &plant(
            "notes",
            "      TAG: ${{ needs.plan.outputs.tag }}\n",
            "      TAG: v1.1.1\n",
        ),
        "does not set `TAG:",
    );
    red(
        &plant(
            "notes",
            "cargo xtask release notes dist",
            "cargo xtask release notesx dist",
        ),
        "does not run `cargo xtask release notes`",
    );
    red(
        &plant(
            "notes",
            "--tag \"$TAG\" --commit \"$COMMIT\" --version-json",
            "--commit \"$COMMIT\" --version-json",
        ),
        "`release notes` is not given `--tag $TAG`",
    );
    red(
        &plant("notes", "name: release-notes\n", "name: notes\n"),
        "are not uploaded",
    );
    red(
        &plant("notes", "path: release-notes.md", "path: other.md"),
        "are not uploaded",
    );
    red(
        &plant(
            "notes",
            "if-no-files-found: error",
            "if-no-files-found: ignore",
        ),
        "are not uploaded",
    );
}

#[test]
fn publish_is_given_the_planned_tag_the_set_the_run_and_the_notes() {
    // Catches: a release of other files or notes than the ones checked. In particular: the hash,
    // run id or tag from nowhere; the flag dropped; the token not the workflow's; the notes
    // file another one, not downloaded, or downloaded elsewhere; the files not checked.
    red(
        &plant(
            "publish",
            "SET_HASH: ${{ needs.notes.outputs.set-hash }}",
            "SET_HASH: abc",
        ),
        "does not set `SET_HASH:",
    );
    red(
        &plant("publish", " --set-hash \"$SET_HASH\"", ""),
        "not given `--set-hash $SET_HASH`",
    );
    red(
        &plant(
            "publish",
            "RUN_ID: ${{ github.event.workflow_run.id }}",
            "RUN_ID: 1",
        ),
        "does not set `RUN_ID:",
    );
    red(
        &plant(
            "publish",
            "TAG: ${{ needs.plan.outputs.tag }}",
            "TAG: v1.1.1",
        ),
        "does not set `TAG:",
    );
    red(
        &plant("publish", "--commit \"$COMMIT\"", "--commit \"$TAG\""),
        "not given `--commit $COMMIT`",
    );
    red(
        &plant(
            "publish",
            "GH_TOKEN: ${{ github.token }}\n          TAG",
            "GH_TOKEN: ${{ secrets.RELEASE_BOT_PRIVATE_KEY }}\n          TAG",
        ),
        "does not set `GH_TOKEN:",
    );
    red(
        &plant("publish", " --notes-file release-notes.md", ""),
        "not given `--notes-file release-notes.md`",
    );
    red(
        &plant(
            "publish",
            "--notes-file release-notes.md",
            "--notes-file other.md",
        ),
        "not given `--notes-file release-notes.md`",
    );
    red(
        &plant(
            "publish",
            "          name: release-notes\n",
            "          name: release-assets\n",
        ),
        "does not download the artifact `release-notes`",
    );
    red(
        &plant(
            "publish",
            "          name: release-notes\n",
            "          name: release-notes\n          path: dist\n",
        ),
        "does not download the artifact `release-notes`",
    );
    red(
        &plant(
            "publish",
            "          name: release-assets\n          path: dist\n",
            "          name: release-assets\n          path: other\n",
        ),
        "does not download the artifact `release-assets`",
    );
    red(
        &plant(
            "publish",
            "run: sha256sum --check --strict SHA256SUMS",
            "run: true",
        ),
        "does not check the files against SHA256SUMS",
    );
}

#[test]
fn bump_is_given_the_version_and_a_token_for_the_internal_repository_alone() {
    // Catches: a pull request opened for another version or by a token that reaches more.
    // Catches: the owner, repository or a permission changed or added; the version from
    // nowhere; the commit, internal repository or submodule changed; the workflow's token.
    red(
        &plant(
            "bump",
            "          repositories: komira-internal\n",
            "          repositories: komira-internal,kbf\n",
        ),
        "is minted with",
    );
    red(
        &plant(
            "bump",
            "          permission-pull-requests: write\n",
            "          permission-pull-requests: write\n          permission-actions: write\n",
        ),
        "is minted with",
    );
    red(
        &plant("bump", "          owner: komira-ai\n", ""),
        "is minted with",
    );
    red(
        &plant(
            "bump",
            "VERSION: ${{ needs.plan.outputs.version }}",
            "VERSION: 1.1.1",
        ),
        "does not set `VERSION:",
    );
    red(
        &plant("bump", "--version \"$VERSION\"", "--version \"$COMMIT\""),
        "not given `--version $VERSION`",
    );
    red(
        &plant("bump", "--sha \"$COMMIT\"", "--sha \"$VERSION\""),
        "not given `--sha $COMMIT`",
    );
    red(
        &plant(
            "bump",
            "--internal-repo komira-ai/komira-internal",
            "--internal-repo komira-ai/other",
        ),
        "not given `--internal-repo komira-ai/komira-internal`",
    );
    red(
        &plant("bump", "--submodule kbf", "--submodule other"),
        "not given `--submodule kbf`",
    );
    red(
        &plant(
            "bump",
            "GH_TOKEN: ${{ steps.bot.outputs.token }}",
            "GH_TOKEN: ${{ github.token }}",
        ),
        "does not set `GH_TOKEN:",
    );
    red(
        &plant(
            "bump",
            "cargo xtask release bump-internal",
            "cargo xtask release bump-internl",
        ),
        "does not run `cargo xtask release bump-internal`",
    );
}

#[test]
fn actions_stay_pinned_and_the_token_starts_empty() {
    // Catches: the shared rules loosened in this file. In particular: an action pinned to a tag;
    // the App-token action pinned to a tag; the top-level `permissions: {}` removed; a
    // self-hosted runner.
    red(
        &plant(
            "publish",
            "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n        with:\n          name: release-assets",
            "actions/download-artifact@v8\n        with:\n          name: release-assets",
        ),
        "workflow lint:",
    );
    red(
        &plant(
            "tag",
            "actions/create-github-app-token@bcd2ba49218906704ab6c1aa796996da409d3eb1 # v3.2.0",
            "actions/create-github-app-token@v3",
        ),
        "workflow lint:",
    );
    red(&plant_top("permissions: {}\n", ""), "workflow lint:");
    red(
        &plant("plan", "runs-on: ubuntu-24.04", "runs-on: self-hosted"),
        "self-hosted",
    );
}

#[test]
fn a_binary_runs_only_after_its_files_are_attested() {
    // Catches: an unattested binary executed in a job that sees the tag and the notes.
    // Catches: a binary run by hand; this release's attestation step lost before the run; the
    // previous release's step lost, or under another condition than the run it guards.
    red(
        &plant(
            "notes",
            "cargo xtask release identity dist ",
            "cargo xtask release identity other ",
        ),
        "not `release identity` on `dist` or `previous`",
    );
    red(
        &plant(
            "notes",
            "cargo xtask verify-assets attestations dist ",
            "cargo xtask verify-assets attestations dst ",
        ),
        "runs a binary unpacked from `dist/` without an earlier",
    );
    red(
        &plant(
            "notes",
            "cargo xtask verify-assets attestations previous ",
            "cargo xtask verify-assets attestations prev ",
        ),
        "runs a binary unpacked from `previous/` without an earlier",
    );
    red(
        &plant(
            "notes",
            "      - name: Verify the previous release's attestations as these were\n        if: ${{ steps.previous.outputs.previous != '' }}\n",
            "      - name: Verify the previous release's attestations as these were\n",
        ),
        "runs a binary unpacked from `previous/` without an earlier",
    );
    red(
        &plant(
            "notes",
            "run: cargo xtask release identity dist --commit \"$COMMIT\" --out version.json",
            "run: dist/kbf --version --json > version.json",
        ),
        "not `release identity` on `dist` or `previous`",
    );
    let no_identity = real().replace(
        "cargo xtask release identity ",
        "cargo xtask release identit ",
    );
    red(
        &problems(&no_identity).expect("read"),
        "never runs `release identity`",
    );
    red(
        &plant("notes", " --version-json version.json", ""),
        "not given `--version-json`",
    );
    red(
        &plant(
            "notes",
            " --previous-version-json \"$PREVIOUS_IDENTITY\"",
            "",
        ),
        "not given `--previous-version-json $PREVIOUS_IDENTITY`",
    );
}

#[test]
fn no_release_check_is_given_a_waiver() {
    // Catches: a waiver left on the check of the previous release (a previous release without
    // the version facts of its kbf, or the facts and the SBOM of its task image, would pass and
    // be read for its identity), and one added to the check of the release being made (a set
    // without them would go out). The real workflow is the control: `the_real_release_workflow_meets_every_rule`
    // holds it clean.
    for (verb, plain) in [
        (
            "release check previous --commit \"$PREVIOUS_COMMIT\"",
            "previous",
        ),
        ("release check dist --commit \"$COMMIT\"", "dist"),
    ] {
        let planted = format!("{verb} --without-version-facts");
        red(
            &plant("notes", verb, &planted),
            &format!("`release check {plain}` is given `--without-version-facts`"),
        );
    }
}

#[test]
fn a_workflow_of_another_shape_is_an_error_not_an_empty_answer() {
    // Catches: a lint that passes because a rewritten key read as nothing: the workflow's
    // `env` a string.
    let text = real();
    let err = problems(&text.replacen(
        "\nenv:\n  CARGO_TERM_COLOR: always\n",
        "\nenv: x\nzz:\n  CARGO_TERM_COLOR: always\n",
        1,
    ))
    .expect_err("env that is a string");
    assert!(err.contains("is not a mapping"), "{err}");
}

#[test]
fn a_comment_naming_the_key_is_not_a_read_of_it() {
    // Catches: a rule that fires on a comment (the control for the key rules above, which
    // read text): the key named in a comment of publish is not a read.
    let text = real().replace(
        "    environment: release\n    permissions:\n      contents: write\n",
        "    environment: release\n    # RELEASE_BOT_PRIVATE_KEY is not read here\n    permissions:\n      contents: write\n",
    );
    assert_eq!(problems(&text).expect("read"), Vec::<String>::new());
}

#[test]
fn the_files_are_checked_for_signer_repository_commit_and_sums_in_verify_and_notes() {
    // Catches: an attestation check that accepts another signing workflow, another
    // repository or commit, or a set whose sums were never run. Catches: in each of verify
    // and notes, `--workflow-ref` pointed at ci.yml, `--repo` at another repository,
    // `--commit` at a fixed sha, the `sha256sum` step replaced by `true`.
    let flags = [
        (
            "dist --repo \"$GITHUB_REPOSITORY\" --workflow-ref \"$GITHUB_REPOSITORY/.github/workflows/artifacts.yml\"",
            "dist --repo \"$GITHUB_REPOSITORY\" --workflow-ref \"$GITHUB_REPOSITORY/.github/workflows/ci.yml\"",
            "`--workflow-ref",
        ),
        (
            "attestations dist --repo \"$GITHUB_REPOSITORY\"",
            "attestations dist --repo komira-ai/other",
            "`--repo",
        ),
        (
            "--commit \"$COMMIT\" --trusted-root",
            "--commit 0000000000000000000000000000000000000000 --trusted-root",
            "`--commit",
        ),
    ];
    for job in ["verify", "notes"] {
        for (from, to, want) in flags {
            red(&plant(job, from, to), want);
        }
        red(
            &plant(
                job,
                "        working-directory: dist\n        run: sha256sum --check --strict SHA256SUMS",
                "        working-directory: dist\n        run: true",
            ),
            &format!("the job `{job}` does not check the files against SHA256SUMS"),
        );
    }
}

#[test]
fn only_plan_and_verify_download_from_another_run() {
    // Catches: tag, notes or publish taking files from a run `verify` never checked.
    // A `run-id` on the `release-assets` download of tag, of notes and of publish.
    for job in ["tag", "notes", "publish"] {
        let found = plant(
            job,
            "          name: release-assets\n",
            "          name: release-assets\n          run-id: 1\n",
        );
        red(&found, &format!("a download of `{job}` names a `run-id`"));
    }
}

#[test]
fn the_rehearsals_run_after_publish_read_only_under_the_release_condition() {
    // Catches: a rehearsal (the helper's, and the agent's with the coordinator) starting before
    // the release it updates to is published, holding a write scope or the OIDC token, or
    // running for a pipeline run that cuts no release.
    for job in ["rehearse", "rehearse-agent"] {
        red(
            &plant(job, "needs: [plan, publish]\n", "needs: [plan]\n"),
            &format!("the job `{job}` needs"),
        );
        red(
            &plant(job, "      contents: read\n", "      contents: write\n"),
            &format!("the job `{job}` holds `contents: write`"),
        );
        red(
            &plant(
                job,
                "      contents: read\n",
                "      contents: read\n      id-token: write\n",
            ),
            &format!("the job `{job}` holds `id-token: write`"),
        );
        red(
            &plant(job, " && needs.plan.outputs.pipeline != 'true' }}", " }}"),
            job,
        );
    }
}

#[test]
fn gamma_runs_after_publish_with_the_run_token_and_no_write_scope() {
    // Catches: the gamma stage starting before the release is published or without the
    // run's conditions, holding a write scope or a token other than the OIDC one, or
    // dropping the environment whose name the coordinator's allow-list can require.
    red(
        &plant(
            "gamma",
            "needs: [plan, publish, rehearse, rehearse-agent]\n",
            "needs: [plan, rehearse, rehearse-agent]\n",
        ),
        "the job `gamma` needs",
    );
    red(
        &plant(
            "gamma",
            "needs: [plan, publish, rehearse, rehearse-agent]\n",
            "needs: [plan, bump, rehearse, rehearse-agent]\n",
        ),
        "the job `gamma` needs",
    );
    // Catches: the gamma stage telling the farm about a release no host was updated to, by the
    // helper alone or through the coordinator and the agent.
    red(
        &plant(
            "gamma",
            "needs: [plan, publish, rehearse, rehearse-agent]\n",
            "needs: [plan, publish]\n",
        ),
        "the job `gamma` needs",
    );
    red(
        &plant(
            "gamma",
            "needs: [plan, publish, rehearse, rehearse-agent]\n",
            "needs: [plan, publish, rehearse]\n",
        ),
        "the job `gamma` needs",
    );
    red(
        &plant(
            "gamma",
            "needs: [plan, publish, rehearse, rehearse-agent]\n",
            "needs: [plan, publish, rehearse-agent]\n",
        ),
        "the job `gamma` needs",
    );
    red(
        &plant("gamma", "      contents: read\n", "      contents: write\n"),
        "the job `gamma` holds `contents: write`",
    );
    red(
        &plant(
            "gamma",
            "      id-token: write\n",
            "      id-token: write\n      packages: write\n",
        ),
        "the job `gamma` holds `packages: write`",
    );
    red(
        &plant("gamma", "      id-token: write\n", "      id-token: read\n"),
        "the job `gamma` holds the scopes",
    );
    red(
        &plant("gamma", "    environment: release\n", ""),
        "the job `gamma` has environment",
    );
    red(
        &plant(
            "gamma",
            "needs.plan.outputs.pipeline != 'true' }}",
            "needs.plan.outputs.pipeline != 'false' }}",
        ),
        "the job `gamma` has `if:",
    );
    red(
        &plant("gamma", "github.event.workflow_run.event == 'push' && ", ""),
        "the job `gamma` has `if:",
    );
}

#[test]
fn gamma_reads_the_coordinators_address_from_the_variable_alone() {
    // Catches: an address written in the workflow, the variable read by another job, a
    // secret in the gamma job, and the verb given anything but the variable and the
    // planned version.
    red(
        &plant(
            "gamma",
            "COORDINATOR_URL: ${{ vars.KBF_COORDINATOR_URL }}",
            "COORDINATOR_URL: https://coordinator.example:8443",
        ),
        "writes an address",
    );
    red(
        &plant(
            "gamma",
            "COORDINATOR_URL: ${{ vars.KBF_COORDINATOR_URL }}",
            "COORDINATOR_URL: ${{ secrets.KBF_COORDINATOR_URL }}",
        ),
        "reads a secret",
    );
    red(
        &plant(
            "gamma",
            "COORDINATOR_URL: ${{ vars.KBF_COORDINATOR_URL }}",
            "COORDINATOR_URL: ${{ vars.OTHER }}",
        ),
        "does not set `COORDINATOR_URL:",
    );
    red(
        &plant(
            "bump",
            "          VERSION: ${{ needs.plan.outputs.version }}\n",
            "          VERSION: ${{ needs.plan.outputs.version }}\n          URL: ${{ vars.KBF_COORDINATOR_URL }}\n",
        ),
        "the job `bump` reads `vars.KBF_COORDINATOR_URL`",
    );
    red(
        &plant("gamma", "--url \"$COORDINATOR_URL\"", "--url \"$VERSION\""),
        "not given `--url $COORDINATOR_URL`",
    );
    red(
        &plant(
            "gamma",
            "--ca-pem \"$COORDINATOR_CA\"",
            "--ca-pem \"$VERSION\"",
        ),
        "not given `--ca-pem $COORDINATOR_CA`",
    );
    red(
        &plant(
            "gamma",
            "COORDINATOR_CA: ${{ vars.KBF_COORDINATOR_CA }}",
            "COORDINATOR_CA: ${{ vars.OTHER }}",
        ),
        "does not set `COORDINATOR_CA:",
    );
    red(
        &plant("gamma", "--version \"$VERSION\"", "--version \"$COMMIT\""),
        "not given `--version $VERSION`",
    );
    red(
        &plant(
            "gamma",
            "VERSION: ${{ steps.farm-version.outputs.version }}",
            "VERSION: 1.1.1",
        ),
        "does not set `VERSION:",
    );
    // The release's tag is not the version the hosts report: it never converges.
    red(
        &plant(
            "gamma",
            "VERSION: ${{ steps.farm-version.outputs.version }}",
            "VERSION: ${{ needs.plan.outputs.version }}",
        ),
        "does not set `VERSION:",
    );
    red(
        &plant(
            "gamma",
            "cargo xtask release farm-version ",
            "cargo xtask other ",
        ),
        "does not run `cargo xtask release farm-version`",
    );
    red(
        &plant(
            "gamma",
            "--commit \"$COMMIT\" --output",
            "--commit \"$TAG\" --output",
        ),
        "`cargo xtask release farm-version` is not given `--commit $COMMIT`",
    );
    red(
        &plant("gamma", "        id: farm-version\n", ""),
        "does not have `id: farm-version`",
    );
    red(
        &plant(
            "gamma",
            "--tag \"$TAG\" --commit",
            "--tag \"$COMMIT\" --commit",
        ),
        "`cargo xtask release farm-version` is not given `--tag $TAG`",
    );
    red(
        &plant("gamma", "cargo xtask deploy-farm ", "cargo xtask other "),
        "does not run `cargo xtask deploy-farm`",
    );
}

#[test]
fn the_network_join_of_gamma_uses_the_run_token_and_variables_only() {
    // Catches: a secret given to the join (the job would hold more than the run token), an
    // input added or changed, the join under another condition or unconditional (a farm
    // without a tailnet would fail every release), the join moved before the build (the
    // build's scripts would see the network) or after the verb, an `env:` on it, a second
    // join, and the action used by another job.
    let join = "          oauth-client-id: ${{ vars.TS_CLIENT_ID }}\n";
    red(
        &plant(
            "gamma",
            join,
            "          oauth-client-id: ${{ vars.TS_CLIENT_ID }}\n          oauth-secret: ${{ secrets.TS_OAUTH_SECRET }}\n",
        ),
        "reads a secret",
    );
    red(
        &plant(
            "gamma",
            join,
            "          oauth-client-id: ${{ vars.TS_CLIENT_ID }}\n          authkey: ${{ vars.TS_KEY }}\n",
        ),
        "not exactly",
    );
    red(
        &plant("gamma", "tags: tag:ci", "tags: tag:ci,tag:admin"),
        "not exactly",
    );
    red(
        &plant(
            "gamma",
            "audience: ${{ vars.TS_AUDIENCE }}",
            "audience: ${{ vars.OTHER }}",
        ),
        "not exactly",
    );
    red(
        &plant(
            "gamma",
            "        if: ${{ vars.TS_CLIENT_ID != '' && (vars.KBF_COORDINATOR_URL != '' || vars.FARM_ADDRESS != '') }}\n",
            "        if: ${{ vars.OTHER != '' }}\n",
        ),
        "is under `if:`",
    );
    red(
        &plant(
            "gamma",
            "        if: ${{ vars.TS_CLIENT_ID != '' && (vars.KBF_COORDINATOR_URL != '' || vars.FARM_ADDRESS != '') }}\n",
            "",
        ),
        "not under exactly `if:",
    );
    let step = "      - name: Join the farm's Tailscale network\n";
    let real = real();
    let start = real.find(step).expect("the join step");
    let end = start
        + real[start..]
            .find("      # The version is the release's;")
            .expect("the next comment");
    let block = &real[start..end];
    // The build line of `gamma` (other jobs build xtask too): the last one before the join.
    let build = "      - run: cargo build -p xtask --locked\n";
    let at = real[..start].rfind(build).expect("gamma's build of xtask");
    let moved = [&real[..at], block, &real[at..start], &real[end..]].concat();
    red(
        &problems(&moved).expect("the planted workflow is read"),
        "not after `cargo build -p xtask`",
    );
    // After the verb: the join moved to the end of the file, which is the end of `gamma`.
    let late = [&real[..start], &real[end..], block].concat();
    red(
        &problems(&late).expect("the planted workflow is read"),
        "before `cargo xtask deploy-farm`",
    );
    red(
        &plant(
            "gamma",
            "        if: ${{ vars.TS_CLIENT_ID != '' && (vars.KBF_COORDINATOR_URL != '' || vars.FARM_ADDRESS != '') }}\n",
            "        if: ${{ vars.TS_CLIENT_ID != '' && (vars.KBF_COORDINATOR_URL != '' || vars.FARM_ADDRESS != '') }}\n        env:\n          TS_AUTHKEY: ${{ vars.TS_KEY }}\n",
        ),
        "has an `env:`",
    );
    let twice = real.replacen(block, &format!("{block}{block}"), 1);
    red(
        &problems(&twice).expect("the planted workflow is read"),
        "times, not once",
    );
    red(
        &plant(
            "bump",
            "      - uses: actions/checkout@",
            &format!(
                "      - uses: tailscale/github-action@d1b6cd204f8dceda5b3eaad7f1f767be390056cd # v4.2.0\n{}",
                "      - uses: actions/checkout@"
            ),
        ),
        "the job `bump` uses `tailscale/github-action@`",
    );
}

#[test]
fn the_gamma_steps_are_not_skipped_by_a_condition_of_its_own() {
    // Catches: the unset variable turned into a skipped step (which would show no notice) by
    // an `if:` on the step; the lint of every job's steps refuses it.
    red(
        &plant(
            "gamma",
            "      - name: Set this release as the farm's desired version, and wait for the farm to converge\n",
            "      - name: Set this release as the farm's desired version, and wait for the farm to converge\n        if: ${{ vars.KBF_COORDINATOR_URL != '' }}\n",
        ),
        "a step of `gamma`",
    );
}

const PROOF_NAME: &str = "      - name: Prove the farm with a remote-only Buck2 build\n";

#[test]
fn the_proof_runs_once_after_deploy_farm_and_is_not_skipped() {
    // Catches: the proof missing (a release with no proof of the farm), run before the
    // deploy (it would prove the previous release), twice, under an `if:` (an unset
    // `FARM_ADDRESS` would skip the step and show no notice), or run without the pinned
    // Buck2 fetched before it.
    red(
        &plant("gamma", "cargo xtask prove-farm ", "cargo xtask other "),
        "does not run `cargo xtask prove-farm`",
    );
    red(
        &plant(
            "gamma",
            PROOF_NAME,
            &format!("{PROOF_NAME}        if: ${{{{ vars.FARM_ADDRESS != '' }}}}\n"),
        ),
        "a step of `gamma`",
    );
    red(
        &plant(
            "gamma",
            PROOF_NAME,
            &format!("{PROOF_NAME}        if: ${{{{ vars.FARM_ADDRESS != '' }}}}\n"),
        ),
        "is under a condition",
    );
    let real = real();
    let start = real
        .find("      # The proof. The farm now runs")
        .expect("proof");
    let block = &real[start..];
    let before = real
        .find("      # The version is the release's;")
        .expect("deploy-farm");
    let moved = [&real[..before], block, &real[before..start]].concat();
    red(
        &problems(&moved).expect("the planted workflow is read"),
        "runs before `cargo xtask deploy-farm`",
    );
    let verb = real.find(PROOF_NAME).expect("proof step");
    let twice = [&real[..verb], &real[verb..], &real[verb..]].concat();
    red(
        &problems(&twice).expect("the planted workflow is read"),
        "times, not once",
    );
    red(
        &plant("gamma", "--sha256 \"$BUCK2_SHA256\"", "--sha256 \"$OTHER\""),
        "does not fetch Buck2",
    );
}

#[test]
fn the_proof_reads_the_farm_variables_and_the_nonce_through_env_alone() {
    // Catches: a farm address written in the job (as a URL or a bare grpc address), a
    // secret in the job, a variable other than the four, a fifth `env:` entry, the verb given
    // something other than the variable it is named for, a nonce that is not this run's id
    // and attempt (a cached run would prove nothing), and another job reading a `FARM_`
    // variable.
    red(
        &plant(
            "gamma",
            "FARM_ADDRESS: ${{ vars.FARM_ADDRESS }}",
            "FARM_ADDRESS: grpc://farm.example:8980",
        ),
        "writes a farm address",
    );
    red(
        &plant(
            "gamma",
            "FARM_ADDRESS: ${{ vars.FARM_ADDRESS }}",
            "FARM_ADDRESS: ${{ vars.FARM_ADDRESS }}\n          FARM_URL: https://farm.example",
        ),
        "writes an address",
    );
    red(
        &plant(
            "gamma",
            "FARM_TLS: ${{ vars.FARM_TLS }}",
            "FARM_TLS: ${{ secrets.FARM_TLS }}",
        ),
        "reads a secret",
    );
    for (key, var) in FARM_VARS {
        red(
            &plant(
                "gamma",
                &format!("{key}: {var}"),
                &format!("{key}: ${{{{ vars.OTHER }}}}"),
            ),
            &format!("does not set `{key}:"),
        );
    }
    red(
        &plant(
            "gamma",
            "NONCE: ${{ github.run_id }}-${{ github.run_attempt }}",
            "NONCE: ${{ github.run_id }}",
        ),
        "does not set `NONCE:",
    );
    red(
        &plant(
            "gamma",
            "NONCE: ${{ github.run_id }}-${{ github.run_attempt }}",
            "NONCE: ${{ github.run_id }}-${{ github.run_attempt }}\n          EXTRA: ${{ vars.OTHER }}",
        ),
        "`env:` entries",
    );
    for (flag, var) in [
        ("--address", "FARM_ADDRESS"),
        ("--instance", "FARM_INSTANCE"),
        ("--tls", "FARM_TLS"),
        ("--properties", "FARM_PROPERTIES"),
        ("--nonce", "NONCE"),
    ] {
        red(
            &plant(
                "gamma",
                &format!("{flag} \"${var}\""),
                &format!("{flag} \"$VERSION\""),
            ),
            &format!("not given `{flag} ${var}`"),
        );
    }
    red(
        &plant(
            "gamma",
            "--project crates/kbf-integration-test/gamma/buck2",
            "--project crates/kbf-integration-test/beta/buck2",
        ),
        "not given `--project",
    );
    red(
        &plant(
            "bump",
            "          VERSION: ${{ needs.plan.outputs.version }}\n",
            "          VERSION: ${{ needs.plan.outputs.version }}\n          ADDRESS: ${{ vars.FARM_ADDRESS }}\n",
        ),
        "the job `bump` reads a `FARM_` variable",
    );
}

#[test]
fn the_buck2_fetch_step_takes_its_pins_from_the_job_env_alone() {
    // Catches: a step-level `env:` on the fetch step that overrides the checksum or the
    // version, which would fetch a Buck2 the pins lint never compared with beta.yml's.
    let fetch = "      - name: Fetch buck2 (pinned version, checked by SHA-256)\n";
    for key in ["BUCK2_SHA256", "BUCK2_VERSION"] {
        red(
            &plant(
                "gamma",
                fetch,
                &format!("{fetch}        env:\n          {key}: other\n"),
            ),
            "Buck2 fetch step of `gamma` has an `env:`",
        );
    }
}

#[test]
fn the_rehearsal_token_is_the_runs_own_in_a_private_file_and_never_in_a_command() {
    // Catches: the rehearsal's helper listing releases without the run's token (the shared
    // address's anonymous quota refuses it), the token taken from a secret, written to a file
    // others can read, or put in a command line.
    let token = "printf '%s' \"$GH_TOKEN\" > /var/tmp/kbf-update-rehearsal.token";
    red(
        &plant(
            REHEARSE,
            token,
            "printf '%s' \"${{ github.token }}\" > /var/tmp/kbf-update-rehearsal.token",
        ),
        "the job `rehearse` puts a `${{ }}` value in a command",
    );
    red(
        &plant(
            REHEARSE,
            "GH_TOKEN: ${{ github.token }}",
            "GH_TOKEN: ${{ secrets.SOME_TOKEN }}",
        ),
        "the job `rehearse` writes a token that is not `github.token`",
    );
    red(
        &plant(
            REHEARSE,
            "install -m 0600 /dev/null /var/tmp/kbf-update-rehearsal.token",
            "install -m 0644 /dev/null /var/tmp/kbf-update-rehearsal.token",
        ),
        "the job `rehearse` makes its token file with a mode other than 0600",
    );
    red(
        &plant(REHEARSE, token, "echo done"),
        "the job `rehearse` does not write its token file from `GH_TOKEN`",
    );
}

#[test]
fn the_rehearsal_names_the_token_file_in_the_channel_the_helper_reads() {
    // Catches: the rehearsal's channel dropping `token_file` again, so the helper's release
    // listing is anonymous and shares a hosted runner's quota. The test cannot run in a pull
    // request (it needs root and systemd); this reads its source.
    let path = "crates/kbf-host-install/tests/rehearsal/mod.rs";
    let source = std::fs::read_to_string(crate::repo_root().join(path)).expect(path);
    assert!(
        source.contains("source = \\\"github\\\"\\ntoken_file = "),
        "{path}: the channel does not name a `token_file`"
    );
    assert!(
        !source.contains("no token. (The helper"),
        "{path}: still says the channel names no token"
    );
}
