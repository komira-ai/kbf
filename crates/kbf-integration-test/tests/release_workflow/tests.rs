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
fn only_a_push_to_main_and_a_dispatch_start_the_workflow() {
    // Catches: a release started by a pull request, a schedule, a tag or a push of another
    // branch; a dispatch dropped; a dispatch that takes an input a person could point at
    // another commit with. Mutants: the push dropped; a `pull_request`; a schedule; a tag push;
    // an unfiltered push; the dispatch dropped; an input added.
    let on = "on:\n  push:\n    branches: [main]\n  workflow_dispatch:\n";
    red(
        &plant_top(on, "on:\n  workflow_dispatch:\n"),
        "does not run on `push`",
    );
    red(
        &plant_top(
            on,
            "on:\n  pull_request:\n  push:\n    branches: [main]\n  workflow_dispatch:\n",
        ),
        "runs on `pull_request`",
    );
    red(
        &plant_top(
            on,
            "on:\n  schedule:\n    - cron: '0 0 * * *'\n  push:\n    branches: [main]\n  workflow_dispatch:\n",
        ),
        "runs on `schedule`",
    );
    red(
        &plant_top(on, "on:\n  push:\n    tags: ['v*']\n  workflow_dispatch:\n"),
        "not exactly `branches: [main]`",
    );
    red(
        &plant_top(on, "on:\n  push:\n  workflow_dispatch:\n"),
        "not exactly `branches: [main]`",
    );
    red(
        &plant_top(on, "on:\n  push:\n    branches: [main]\n"),
        "has no `workflow_dispatch:` entry",
    );
    red(
        &plant_top(
            on,
            "on:\n  push:\n    branches: [main]\n  workflow_dispatch:\n    inputs:\n      commit:\n        required: true\n        type: string\n",
        ),
        "declares inputs",
    );
}

#[test]
fn a_push_trigger_needs_the_whole_chain_of_jobs() {
    // Catches: the push trigger present while a gate of the chain is missing, so a push to
    // main would tag or publish without it. Mutants: each of gates, plan, verify, tag, publish
    // and bump renamed away in turn (each must name the missing job, as the push is present).
    // `triggers` is read alone: the later rules stop at a missing job with an error, and the
    // chain rule must stand on its own when they do not.
    // The list itself is pinned here, not read back from `CHAIN`: shrinking `CHAIN` must not
    // shrink this test with it.
    assert_eq!(
        CHAIN,
        ["gates", "plan", "verify", "tag", "publish", "bump"],
        "the jobs a push needs"
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
        red(&found, &format!("runs on `push` but has no job `{job}`"));
    }
}

#[test]
fn the_job_list_is_pinned() {
    // Catches: the chain changed without its rule. Mutants: the job `publish` renamed; a job
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
    // Catches: two releases racing for the same version, or a release cancelled half way.
    // Mutants: the group without the per-run fallback of a dispatch; cancel-in-progress true.
    red(
        &plant_top(
            "release-${{ github.event_name == 'push' && 'push' || github.run_id }}",
            "release-push",
        ),
        "concurrency group",
    );
    red(
        &plant_top("cancel-in-progress: false", "cancel-in-progress: true"),
        "cancel-in-progress: false",
    );
}

#[test]
fn every_job_needs_exactly_what_the_graph_says() {
    // Catches: a job that runs without a stage before it. Mutants: gates dropped from plan;
    // plan from verify; verify from tag; tag from notes; notes from publish; publish from
    // bump (and so the gates are no longer reached from bump through any other job).
    red(
        &plant("plan", "needs: [gates]\n", "needs: []\n"),
        "`plan` needs []",
    );
    red(
        &plant("verify", "needs: [gates, plan]\n", "needs: [plan]\n"),
        "`verify` needs",
    );
    red(
        &plant(
            "tag",
            "needs: [gates, plan, verify]\n",
            "needs: [gates, plan]\n",
        ),
        "`tag` needs",
    );
    red(
        &plant(
            "notes",
            "needs: [gates, plan, verify, tag]\n",
            "needs: [gates, plan, verify]\n",
        ),
        "`notes` needs",
    );
    red(
        &plant(
            "publish",
            "needs: [gates, plan, verify, tag, notes]\n",
            "needs: [gates, plan, verify, tag]\n",
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
    // push), and an OIDC token in a job that runs a third party's code. Mutants: verify,
    // tag and notes get `contents: write`; publish gets `packages: write`, then `id-token:
    // write`; gates gets `id-token: write`; publish loses `contents: write`.
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
            "gates",
            "      actions: read\n",
            "      actions: read\n      id-token: write\n",
        ),
        "`gates` holds `id-token: write`, which only",
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
fn the_jobs_that_write_run_only_for_a_push_to_main_that_releases() {
    // Catches: a dry run (a dispatch) that tags or publishes; a release of docs only or of
    // the pipeline itself. Mutants per job: the `if:` dropped; widened to any push; the
    // pipeline clause dropped; the code clause dropped; the branch clause dropped.
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
        red(
            &plant(job, " && needs.plan.outputs.pipeline != 'true' }}", " }}"),
            &format!("the job `{job}` has `if:"),
        );
        red(
            &plant(job, " && needs.plan.outputs.code == 'true'", ""),
            &format!("the job `{job}` has `if:"),
        );
        red(
            &plant(job, " && github.ref == 'refs/heads/main'", ""),
            &format!("the job `{job}` has `if:"),
        );
    }
    let verify = format!("    if: {VERIFY_IF}\n");
    red(
        &plant("verify", &verify, ""),
        "the job `verify` has `if: (none)`",
    );
    red(
        &plant(
            "plan",
            "    needs: [gates]\n",
            "    needs: [gates]\n    if: ${{ false }}\n",
        ),
        "the job `plan` has `if:",
    );
}

#[test]
fn the_environment_is_declared_where_the_key_is_read_and_where_writes_happen() {
    // Catches: a key move to an environment secret that would stop working, or a job that
    // reads the key outside the environment. Mutants: the environment dropped from tag,
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
    // Catches: a stage that is skipped or ignored. Mutants: `always()` on publish;
    // `failure()`; `cancelled()`; `continue-on-error` on a gate step and on the tag step; an
    // `if:` on a step of tag, of verify and of notes (not the previous release's).
    red(
        &plant_top(
            "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.code == 'true' && needs.plan.outputs.pipeline != 'true' }}\n    runs-on: ubuntu-24.04\n    timeout-minutes: 20\n",
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
fn only_the_commit_of_the_run_is_checked_out_and_no_input_reaches_a_script() {
    // Catches: a release of code a person or a pull request named. Mutants: a checkout of a
    // ref, of another repository, with a token; without `persist-credentials: false`; two
    // checkouts; an expression of an input, of a pull request, of `head_ref`.
    let checkout = "        with:\n          persist-credentials: false\n";
    for job in ["gates", "tag", "publish", "bump"] {
        red(
            &plant(
                job,
                checkout,
                "        with:\n          ref: main\n          persist-credentials: false\n",
            ),
            &format!("the checkout of `{job}` is given `ref`"),
        );
    }
    red(
        &plant(
            "tag",
            checkout,
            "        with:\n          repository: other/repo\n          persist-credentials: false\n",
        ),
        "is given `repository`",
    );
    red(
        &plant(
            "tag",
            checkout,
            "        with:\n          token: ${{ github.token }}\n          persist-credentials: false\n",
        ),
        "is given `token`",
    );
    red(
        &plant(
            "publish",
            checkout,
            "        with:\n          fetch-depth: 0\n",
        ),
        "the checkout of `publish` is not `persist-credentials: false`",
    );
    red(
        &plant(
            "tag",
            "      - run: rustup toolchain install\n",
            "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n        with:\n          persist-credentials: false\n      - run: rustup toolchain install\n",
        ),
        "has 2 checkouts",
    );
    for expr in [
        "${{ inputs.commit }}",
        "${{ github.event.inputs.commit }}",
        "${{ github.event.pull_request.head.sha }}",
        "${{ github.head_ref }}",
    ] {
        red(
            &plant(
                "gates",
                "          GH_TOKEN: ${{ github.token }}\n",
                &format!("          GH_TOKEN: ${{{{ github.token }}}}\n          X: {expr}\n"),
            ),
            "the workflow reads `",
        );
    }
}

#[test]
fn the_release_bots_key_is_read_in_tag_and_bump_only() {
    // Catches: the one key that can create `v*` tags (and open pull requests in the internal
    // repository) readable by a job that runs more code. Mutants: the key read in publish,
    // notes, gates; the token minted in verify; the old `RELEASE_TAGGER_*` names; the freeze
    // read outside plan; another secret read; the key not read by tag and bump.
    for job in ["publish", "notes", "gates", "verify"] {
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
fn the_gates_job_waits_for_this_commit_on_main_and_hands_on_the_run_ids() {
    // Catches: gates satisfied by another commit or branch, or run ids that go nowhere.
    // Mutants: the sha dropped; the branch changed; the output file dropped; each output
    // pointing at another step output.
    let flags = "--sha \"$GITHUB_SHA\" --branch main --output";
    red(
        &plant("gates", flags, "--branch main --output"),
        "not given `--sha $GITHUB_SHA`",
    );
    red(
        &plant("gates", "--branch main", "--branch feature"),
        "not given `--branch main`",
    );
    red(
        &plant(
            "gates",
            "--sha \"$GITHUB_SHA\" --branch main --output \"$GITHUB_OUTPUT\"",
            "--sha \"$GITHUB_SHA\" --branch main",
        ),
        "not given `--output $GITHUB_OUTPUT`",
    );
    red(
        &plant(
            "gates",
            "beta-run: ${{ steps.gates.outputs.beta_run }}",
            "beta-run: ${{ steps.gates.outputs.ci_run }}",
        ),
        "does not output `beta-run`",
    );
    red(
        &plant(
            "gates",
            "      artifacts-run: ${{ steps.gates.outputs.artifacts_run }}\n",
            "",
        ),
        "does not output `artifacts-run`",
    );
}

#[test]
fn the_plan_job_sees_the_push_and_the_freeze_and_hands_on_the_class() {
    // Catches: a plan that cannot diff the push or see the tags, that ignores the stop
    // switch, or whose classification never reaches the jobs that act on it. Mutants: no full
    // history; `--before` another variable; the freeze dropped, then the variable; the
    // dispatch fallback for `before` dropped; each output dropped.
    red(
        &plant("plan", "          fetch-depth: 0\n", ""),
        "does not check out with `fetch-depth: 0`",
    );
    red(
        &plant("plan", "--before \"$BEFORE\"", "--before \"$GITHUB_SHA\""),
        "not given `--before $BEFORE`",
    );
    red(
        &plant("plan", " --freeze \"$FREEZE\"", ""),
        "not given `--freeze $FREEZE`",
    );
    red(
        &plant("plan", "FREEZE: ${{ vars.RELEASE_FREEZE }}", "FREEZE: ''"),
        "does not set `FREEZE:",
    );
    red(
        &plant("plan", " || '0000000000000000000000000000000000000000'", ""),
        "does not set `BEFORE:",
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
fn verify_checks_the_files_of_the_run_the_gates_saw_before_anything_is_tagged() {
    // Catches: files from another run than the gates saw, or unchecked files uploaded for
    // the tag to name. Mutants: locate dropped, or under `if:`; a download of another
    // run; COMMIT not the sha; `release check` and the
    // attestations dropped; the upload moved before them.
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
        &plant(
            "verify",
            "          name: sha256sums\n          path: dist\n          run-id: ${{ needs.gates.outputs.artifacts-run }}\n",
            "          name: sha256sums\n          path: dist\n          run-id: 1\n",
        ),
        "a download of `verify` is not of run `${{ needs.gates.outputs.artifacts-run }}`",
    );
    red(
        &plant(
            "verify",
            "      COMMIT: ${{ github.sha }}\n",
            "      COMMIT: ${{ needs.plan.outputs.tag }}\n",
        ),
        "does not set `COMMIT: ${{ github.sha }}`",
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
    // step needs, a record that names other runs than the gates'. Mutants: the record's
    // inputs from the wrong place; the order changed; the token given `issues: write`, no
    // repository scope, or the wrong one; a build after it; the tag step on the workflow's
    // own token, with another message file, tag or sha; another step holding the token.
    let record = "--commit \"$GITHUB_SHA\" --tag \"$TAG\" --run-id";
    red(
        &plant("tag", record, "--commit \"$GITHUB_SHA\" --run-id"),
        "not given `--tag $TAG`",
    );
    red(
        &plant(
            "tag",
            "BETA_RUN: ${{ needs.gates.outputs.beta-run }}",
            "BETA_RUN: ${{ needs.gates.outputs.ci-run }}",
        ),
        "does not set `BETA_RUN:",
    );
    red(
        &plant(
            "tag",
            "TAG: ${{ needs.plan.outputs.tag }}\n          CI_RUN",
            "TAG: v9.9.9\n          CI_RUN",
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
            "--sha \"$GITHUB_SHA\" --tag \"$TAG\" --message-file",
            "--tag \"$TAG\" --message-file",
        ),
        "not given `--sha $GITHUB_SHA`",
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
    // printed, a tag check before the files are checked or against another main. Mutants:
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
    // Catches: a release of other files or notes than the ones checked. Mutants: the hash,
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
            "RUN_ID: ${{ needs.gates.outputs.artifacts-run }}",
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
        &plant("publish", "--commit \"$GITHUB_SHA\"", "--commit \"$TAG\""),
        "not given `--commit $GITHUB_SHA`",
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
    // Mutants: the owner, repository or a permission changed or added; the version from
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
        &plant(
            "bump",
            "--version \"$VERSION\"",
            "--version \"$GITHUB_SHA\"",
        ),
        "not given `--version $VERSION`",
    );
    red(
        &plant("bump", "--sha \"$GITHUB_SHA\"", "--sha \"$VERSION\""),
        "not given `--sha $GITHUB_SHA`",
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
    // Catches: the shared rules loosened in this file. Mutants: an action pinned to a tag;
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
        &plant("gates", "runs-on: ubuntu-24.04", "runs-on: self-hosted"),
        "self-hosted",
    );
}

#[test]
fn a_binary_runs_only_after_its_files_are_attested() {
    // Catches: an unattested binary executed in a job that sees the tag and the notes.
    // Mutants: a binary run by hand; this release's attestation step lost before the run; the
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
fn only_the_previous_releases_check_waives_the_version_facts() {
    // Catches: the first release with version facts failing on the release before it (which
    // has none) so that nothing is published, and the other way round: a waiver on the check
    // of the release being made, so a set without the facts of its kbf goes out.
    // Mutants: the flag dropped from `previous`; the flag added to `dist`.
    red(
        &plant(
            "notes",
            "release check previous --commit \"$PREVIOUS_COMMIT\" --without-version-facts",
            "release check previous --commit \"$PREVIOUS_COMMIT\"",
        ),
        "`release check previous` is not given `--without-version-facts`",
    );
    red(
        &plant(
            "notes",
            "release check dist --commit \"$COMMIT\"",
            "release check dist --commit \"$COMMIT\" --without-version-facts",
        ),
        "`release check dist` is given `--without-version-facts`",
    );
}

#[test]
fn a_workflow_of_another_shape_is_an_error_not_an_empty_answer() {
    // Catches: a lint that passes because a rewritten key read as nothing. Mutant: the job
    // `env` of verify a string.
    let text = real();
    let err = problems(&text.replacen(
        "    env:\n      COMMIT: ${{ github.sha }}\n    steps:",
        "    env: x\n    zz:\n      COMMIT: ${{ github.sha }}\n    steps:",
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
    // repository or commit, or a set whose sums were never run. Mutants: in each of verify
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
fn only_verify_downloads_from_another_run() {
    // Catches: tag, notes or publish taking files from a run `verify` never checked.
    // Mutants: a `run-id` on the `release-assets` download of tag, of notes and of publish.
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
fn the_gates_job_outlasts_its_own_wait() {
    // Catches: the job killed at its timeout before the 3-hour wait for the gates reports.
    // Mutant: `timeout-minutes: 180`, then 20.
    for m in ["180", "20"] {
        red(
            &plant(
                "gates",
                "timeout-minutes: 200",
                &format!("timeout-minutes: {m}"),
            ),
            "the job `gates` has no `timeout-minutes` above 180",
        );
    }
}
