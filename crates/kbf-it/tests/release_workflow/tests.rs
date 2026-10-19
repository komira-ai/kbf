//! Tests of the release workflow lint. The real `release.yml` is the control (no
//! problem); each other test plants one defect on its text and names the defect it
//! catches. A plant must change the text, so a rule whose anchor text moved cannot go
//! quiet.

use super::*;

const PATH: &str = ".github/workflows/release.yml";

fn real() -> String {
    std::fs::read_to_string(crate::repo_root().join(PATH)).expect(PATH)
}

/// The problems of the real workflow with `from` replaced by `to` (once or more).
fn plant(from: &str, to: &str) -> Vec<String> {
    let text = real();
    assert!(text.contains(from), "the real workflow has no `{from}`");
    problems(&text.replace(from, to)).expect("the planted workflow is read")
}

fn red(found: &[String], want: &str) {
    assert!(
        found.iter().any(|p| p.contains(want)),
        "want a problem with `{want}`: {found:#?}"
    );
}

#[test]
fn the_real_release_workflow_meets_every_rule() {
    // Catches: release.yml drifting from the prod stage (no verdict, no notes, no
    // approval, a write scope outside publish), and the control for every plant below.
    let found = problems(&real()).expect("release.yml is read");
    assert!(found.is_empty(), "{PATH}:\n{}", found.join("\n"));
}

#[test]
fn a_verify_job_without_the_verdict_step_is_refused() {
    // Catches: the verdict check dropped, so a tag with no verdict (or a red one) is
    // released. Mutant: the step runs `release check` again instead of `release verdict`.
    let found = plant(
        "cargo xtask release verdict dist",
        "cargo xtask release check dist",
    );
    red(&found, "does not run `cargo xtask release verdict`");
}

#[test]
fn a_verdict_that_cannot_stop_the_job_is_refused() {
    // Catches: the verdict under `if:`, so the job passes without it. Mutant: an `if`.
    let found = plant(
        "        id: verdict\n",
        "        id: verdict\n        if: ${{ github.event_name == 'push' }}\n",
    );
    red(&found, "`release verdict` step of `verify` is under `if:`");
}

#[test]
fn a_verdict_not_given_this_tag_or_commit_is_refused() {
    // Catches: a verdict checked for the wrong tag or commit (the check would pass for
    // any release). Mutants: the tag, then the commit, dropped from the arguments.
    red(
        &plant(
            "release verdict dist --tag \"$TAG\"",
            "release verdict dist",
        ),
        "not given `--tag \"$TAG\"`",
    );
    red(
        &plant(
            "release verdict dist --tag \"$TAG\" --commit \"$COMMIT\"",
            "release verdict dist --tag \"$TAG\"",
        ),
        "not given `--commit \"$COMMIT\"`",
    );
}

#[test]
fn a_verdict_before_the_files_are_checked_is_refused() {
    // Catches: the verdict naming a set no one has verified yet. Mutant: the check step
    // renamed so that the verdict now comes first.
    let found = plant(
        "cargo xtask release check dist",
        "cargo xtask release checkdist dist",
    );
    red(&found, "does not come after `release check`");
}

#[test]
fn a_verdict_against_another_main_is_refused() {
    // Catches: main-only weakened by pointing the on-main test at another ref.
    let found = plant(
        "--output \"$GITHUB_OUTPUT\"\n      - name: Write",
        "--main-ref origin/feature/arch-v2 --output \"$GITHUB_OUTPUT\"\n      - name: Write",
    );
    red(&found, "main ref other than `origin/main`");
}

#[test]
fn the_set_hash_must_travel_from_the_verdict_to_publish() {
    // Catches: publish releasing files the verdict does not name. Mutants: the job
    // output dropped; publish not given `--set-hash`; publish's SET_HASH from nowhere.
    red(
        &plant(
            "      set-hash: ${{ steps.verdict.outputs.set-hash }}\n",
            "",
        ),
        "does not output `set-hash` from the verdict step",
    );
    red(
        &plant(" --set-hash \"$SET_HASH\"", ""),
        "not given `--set-hash \"$SET_HASH\"`",
    );
    red(
        &plant(
            "SET_HASH: ${{ needs.verify.outputs.set-hash }}",
            "SET_HASH: abc",
        ),
        "does not set `SET_HASH:",
    );
}

#[test]
fn the_notes_must_be_written_and_uploaded() {
    // Catches: a dry run that shows no notes. Mutants: the notes step dropped; the
    // artifact renamed; the artifact path another file than the one written.
    red(
        &plant(
            "cargo xtask release notes dist",
            "cargo xtask release notesx dist",
        ),
        "does not run `cargo xtask release notes`",
    );
    red(
        &plant("name: release-notes\n", "name: notes\n"),
        "are not uploaded",
    );
    red(
        &plant("path: release-notes.md", "path: other.md"),
        "are not uploaded",
    );
}

#[test]
fn publish_must_run_in_the_prod_environment() {
    // Catches: a publish with no required reviewer. Mutants: no `environment`; another
    // environment; a mapping that names another one.
    red(
        &plant("    environment: prod\n", ""),
        "has environment None",
    );
    red(
        &plant("environment: prod", "environment: staging"),
        "not `prod`",
    );
    red(
        &plant("environment: prod", "environment:\n      name: staging"),
        "not `prod`",
    );
}

#[test]
fn publish_runs_only_for_a_pushed_tag_after_verify() {
    // Catches: a dry run (a dispatch) that reaches the publish job. Mutants: the `if:`
    // dropped; widened to any push; `needs` dropped.
    red(
        &plant(
            "    if: ${{ github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v') }}\n",
            "",
        ),
        "so a dry run may publish",
    );
    red(
        &plant(
            "${{ github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v') }}",
            "${{ github.event_name == 'push' }}",
        ),
        "so a dry run may publish",
    );
    red(
        &plant("    needs: [verify]\n", ""),
        "does not need `verify`",
    );
}

#[test]
fn the_dry_run_holds_no_write_scope() {
    // Catches: the verify job (all a dry run runs) able to write. Mutants: verify gets
    // `contents: write`; verify gets `id-token: write`; publish gets another write scope.
    red(
        &plant(
            "      contents: read\n      actions: read",
            "      contents: write\n      actions: read",
        ),
        "`verify` holds `contents: write`",
    );
    red(
        &plant(
            "      attestations: read\n",
            "      attestations: read\n      id-token: write\n",
        ),
        "`verify` holds `id-token: write`",
    );
    red(
        &plant(
            "      contents: write\n",
            "      contents: write\n      packages: write\n",
        ),
        "`publish` holds `packages: write`",
    );
}

#[test]
fn only_a_tag_push_or_a_dry_run_starts_the_workflow() {
    // Catches: a release started from a pull request, a branch push or a schedule.
    // Mutants: a `pull_request` trigger; a branch filter beside the tags; a schedule; the
    // tag filter widened.
    red(
        &plant("on:\n  push:", "on:\n  pull_request:\n  push:"),
        "runs on `pull_request`",
    );
    red(
        &plant("    tags: ['v*']", "    tags: ['v*']\n    branches: [main]"),
        "not exactly `tags: ['v*']`",
    );
    red(
        &plant(
            "on:\n  push:",
            "on:\n  schedule:\n    - cron: '0 0 * * *'\n  push:",
        ),
        "runs on `schedule`",
    );
    red(
        &plant("tags: ['v*']", "tags: ['*']"),
        "not exactly `tags: ['v*']`",
    );
}

#[test]
fn a_commit_not_on_main_must_still_be_refused() {
    // Catches: main-only lost. Mutants: the locate step dropped; under `if:`; the
    // checkout without full history (no tag object, no origin/main).
    red(
        &plant("cargo xtask release locate", "cargo xtask release locat"),
        "does not run `cargo xtask release locate`",
    );
    red(
        &plant(
            "        id: locate\n",
            "        id: locate\n        if: ${{ false }}\n",
        ),
        "`release locate` step of `verify` is under `if:`",
    );
    red(&plant("          fetch-depth: 0\n", ""), "`fetch-depth: 0`");
}

#[test]
fn actions_stay_pinned_and_the_token_starts_empty() {
    // Catches: the shared rules loosened in this file. Mutants: an action pinned to a
    // tag; the top-level `permissions: {}` removed; a self-hosted runner.
    red(
        &plant(
            "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n        with:\n          name: release-notes",
            "actions/upload-artifact@v7\n        with:\n          name: release-notes",
        ),
        "not pinned to a full commit SHA",
    );
    red(
        &plant("\npermissions: {}\n", "\n"),
        "no top-level `permissions: {}`",
    );
    red(
        &plant(
            "runs-on: ubuntu-24.04\n    timeout-minutes: 20",
            "runs-on: self-hosted\n    timeout-minutes: 20",
        ),
        "self-hosted",
    );
}

#[test]
fn the_verify_env_must_bind_the_tag_and_commit_it_was_triggered_for() {
    // Catches: every step checking a tag or commit other than the one released (it fails
    // closed only on a real tag push). Mutants: TAG from `github.sha`; COMMIT from
    // `inputs.tag`; either dropped.
    red(
        &plant("github.ref_name || inputs.tag", "github.sha || inputs.tag"),
        "does not set `TAG:",
    );
    red(
        &plant("github.sha || inputs.commit", "github.sha || inputs.tag"),
        "does not set `COMMIT:",
    );
    red(
        &plant(
            "      COMMIT: ${{ github.event_name",
            "      COMMITX: ${{ github.event_name",
        ),
        "does not set `COMMIT:",
    );
}

#[test]
fn the_notes_must_be_for_this_tag_and_commit() {
    // Catches: notes for another release reaching the approver. Mutants: the commit
    // dropped; the tag a constant; the tag dropped.
    red(
        &plant(
            "release notes dist --tag \"$TAG\" --commit \"$COMMIT\"",
            "release notes dist --tag \"$TAG\"",
        ),
        "`release notes` is not given `--commit \"$COMMIT\"`",
    );
    red(
        &plant(
            "release notes dist --tag \"$TAG\"",
            "release notes dist --tag v0.0.0",
        ),
        "`release notes` is not given `--tag \"$TAG\"`",
    );
    red(
        &plant(
            "release notes dist --tag \"$TAG\" --commit",
            "release notes dist --commit",
        ),
        "`release notes` is not given `--tag \"$TAG\"`",
    );
}

#[test]
fn the_dry_run_must_take_a_required_commit_and_tag() {
    // Catches: a dispatch with an empty TAG or COMMIT. Mutants: the `tag` input renamed;
    // the `commit` input renamed; `tag` no longer required.
    red(
        &plant("      tag:\n", "      tagx:\n"),
        "required input `tag`",
    );
    red(
        &plant("      commit:\n", "      commitx:\n"),
        "required input `commit`",
    );
    red(
        &plant(
            "checked as a release would check it\n        required: true\n        type: string\n\npermissions",
            "checked as a release would check it\n        required: false\n        type: string\n\npermissions",
        ),
        "required input `tag`",
    );
}

#[test]
fn the_notes_upload_must_fail_when_the_file_is_missing() {
    // Catches: a dry run that shows no notes and still passes. Mutant: the first
    // upload's `if-no-files-found` set to `ignore`.
    red(
        &plant(
            "path: release-notes.md\n          retention-days: 7\n          if-no-files-found: error",
            "path: release-notes.md\n          retention-days: 7\n          if-no-files-found: ignore",
        ),
        "are not uploaded",
    );
}

#[test]
fn a_dispatch_or_env_of_another_shape_is_an_error_not_an_empty_answer() {
    // Catches: a lint that passes because a rewritten key read as nothing. Mutants: the
    // dispatch without `inputs`; without a mapping; an input that is not a mapping; the
    // verify `env` a string.
    let text = real();
    for (from, to, want) in [
        (
            "  workflow_dispatch:\n    inputs:",
            "  workflow_dispatch:\n    inputz:",
            "no `inputs:` mapping",
        ),
        (
            "  workflow_dispatch:\n",
            "  workflow_dispatch: yes\n  zzz:\n",
            "no `workflow_dispatch:` mapping",
        ),
        (
            "      tag:\n        description",
            "      tag: x\n      zz:\n        description",
            "is not a mapping",
        ),
        (
            "    env:\n      # The tag's",
            "    env: x\n    zz:\n      # The tag's",
            "is not a mapping",
        ),
    ] {
        assert!(text.contains(from), "no `{from}`");
        let err = problems(&text.replace(from, to)).expect_err(want);
        assert!(err.contains(want), "want `{want}`: {err}");
    }
}
