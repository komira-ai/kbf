//! Tests of the pipeline lint. The real workflow files are the control (no problem); each other
//! test changes one thing in one file and names the rule it holds.

use super::*;

const DIR: &str = ".github/workflows";

fn read(name: &str) -> String {
    std::fs::read_to_string(crate::repo_root().join(DIR).join(name)).expect(name)
}

fn real() -> Files {
    let mut others = Vec::new();
    for entry in std::fs::read_dir(crate::repo_root().join(DIR)).expect("workflow dir") {
        let name = entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        let own = [
            "pipeline.yml",
            "ci.yml",
            "artifacts.yml",
            "beta.yml",
            "smoke.yml",
            "release.yml",
        ];
        if (name.ends_with(".yml") || name.ends_with(".yaml")) && !own.contains(&name.as_str()) {
            others.push((name.clone(), read(&name)));
        }
    }
    Files {
        pipeline: read("pipeline.yml"),
        ci: read("ci.yml"),
        artifacts: read("artifacts.yml"),
        beta: read("beta.yml"),
        smoke: read("smoke.yml"),
        release: read("release.yml"),
        others,
    }
}

/// The problems with `from` replaced by `to` once in the file `file`, which must hold it once.
fn plant(file: &str, from: &str, to: &str) -> Vec<String> {
    let mut files = real();
    let text = match file {
        "pipeline.yml" => &mut files.pipeline,
        "ci.yml" => &mut files.ci,
        "artifacts.yml" => &mut files.artifacts,
        "beta.yml" => &mut files.beta,
        "smoke.yml" => &mut files.smoke,
        "release.yml" => &mut files.release,
        other => panic!("no file {other}"),
    };
    assert_eq!(
        text.matches(from).count(),
        1,
        "`{from}` is not unique in {file}"
    );
    *text = text.replace(from, to);
    problems(&files).unwrap_or_else(|e| vec![e])
}

fn red(found: &[String], want: &str) {
    assert!(
        found.iter().any(|p| p.contains(want)),
        "want a problem with `{want}`: {found:#?}"
    );
}

#[test]
fn the_real_pipeline_meets_every_rule() {
    // Catches: the pipeline files drifting from the one sequential pipeline, and the control
    // for every test below.
    let found = problems(&real()).expect("the workflows are read");
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
fn the_builds_start_with_ci_signing_needs_ci_and_the_build_and_smoke_needs_the_signing() {
    // Catches: a build that waits for ci again (the 50 minutes this ordering removes), a signing
    // call that can start before ci or before the files are built (an unsigned or failed commit
    // signed, or nothing to sign), a smoke that can start before the files are signed, a beta
    // that waits for a stage it does not need (an hour added to the lane), a stage calling
    // another workflow, and a stage added or reordered.
    red(
        &plant(
            "pipeline.yml",
            "  artifacts:\n    if:",
            "  artifacts:\n    needs: [ci]\n    if:",
        ),
        "the job `artifacts` needs [\"ci\"], not []",
    );
    red(
        &plant(
            "pipeline.yml",
            "    needs: [ci, artifacts]\n",
            "    needs: [artifacts]\n",
        ),
        "the job `artifacts-sign` needs [\"artifacts\"], not [\"ci\", \"artifacts\"]",
    );
    red(
        &plant(
            "pipeline.yml",
            "    needs: [ci, artifacts]\n",
            "    needs: [ci]\n",
        ),
        "the job `artifacts-sign` needs [\"ci\"], not [\"ci\", \"artifacts\"]",
    );
    red(
        &plant(
            "pipeline.yml",
            "    needs: [artifacts-sign]\n",
            "    needs: [artifacts]\n",
        ),
        "the job `smoke` needs [\"artifacts\"], not [\"artifacts-sign\"]",
    );
    red(
        &plant(
            "pipeline.yml",
            "  beta:\n",
            "  beta:\n    needs: [artifacts]\n",
        ),
        "the job `beta` needs [\"artifacts\"], not []",
    );
    red(
        &plant(
            "pipeline.yml",
            "uses: ./.github/workflows/beta.yml",
            "uses: ./.github/workflows/ci.yml",
        ),
        "the job `beta` calls",
    );
    red(
        &plant(
            "pipeline.yml",
            "  beta:\n",
            "  extra:\n    needs: [ci]\n    runs-on: ubuntu-24.04\n    steps: []\n\n  beta:\n",
        ),
        "the pipeline's jobs are",
    );
}

#[test]
fn the_two_calls_of_artifacts_differ_by_their_stage_input() {
    // Catches: a call whose stage is swapped, missing or extended (the build call would sign, or
    // the sign call would rebuild), so the order of the pipeline and the jobs that ran disagree.
    red(
        &plant(
            "pipeline.yml",
            "      stage: build\n",
            "      stage: sign\n",
        ),
        "the job `artifacts` passes [(\"stage\", \"sign\")], not exactly `stage: build`",
    );
    red(
        &plant(
            "pipeline.yml",
            "      stage: sign\n",
            "      stage: build\n",
        ),
        "the job `artifacts-sign` passes [(\"stage\", \"build\")], not exactly `stage: sign`",
    );
    red(
        &plant("pipeline.yml", "    with:\n      stage: build\n", ""),
        "the job `artifacts` passes [], not exactly `stage: build`",
    );
    red(
        &plant(
            "pipeline.yml",
            "      stage: sign\n",
            "      stage: sign\n      extra: x\n",
        ),
        "the job `artifacts-sign` passes",
    );
}

#[test]
fn artifacts_yml_is_called_by_exactly_the_build_and_the_sign_job() {
    // Catches: a third call of artifacts.yml (from release.yml or the pipeline), or one of the two
    // calls pointed at another file, so a push to main would build or sign twice, or not at all.
    red(
        &plant(
            "release.yml",
            "\njobs:\n",
            "\njobs:\n  again:\n    uses: ./.github/workflows/artifacts.yml\n",
        ),
        "artifacts.yml is called by [\"pipeline.yml#artifacts\", \"pipeline.yml#artifacts-sign\", \"release.yml#again\"]",
    );
    red(
        &plant(
            "pipeline.yml",
            "  artifacts-sign:\n    needs: [ci, artifacts]\n    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n    uses: ./.github/workflows/artifacts.yml\n",
            "  artifacts-sign:\n    needs: [ci, artifacts]\n    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n    uses: ./.github/workflows/ci.yml\n",
        ),
        "artifacts.yml is called by [\"pipeline.yml#artifacts\"], not by",
    );
}

#[test]
fn only_the_sign_stage_runs_the_signing_jobs_and_it_runs_nothing_else() {
    // Catches: a signing job that the build call (or a pull request) can run (signing a commit
    // before ci has passed), a verify job outside the sign stage, a build or sums job that the
    // sign call would run again (a rebuild, or a second set of files under the same names), an
    // attest job that needs a job the sign call skips (it would be skipped and nothing signed,
    // with the pipeline green), and a new job without a stage condition.
    let stage = "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && inputs.stage == 'sign' }}";
    let push = "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}";
    red(
        &plant(
            "artifacts.yml",
            &format!("  attest-linux:\n    if: {stage}\n"),
            &format!("  attest-linux:\n    if: {push}\n"),
        ),
        "the job `attest-linux` of artifacts.yml holds a signing scope and has `if:",
    );
    red(
        &plant(
            "artifacts.yml",
            &format!("  verify-darwin-arm64:\n    needs: [attest-darwin-arm64]\n    if: {stage}\n"),
            &format!("  verify-darwin-arm64:\n    needs: [attest-darwin-arm64]\n    if: {push}\n"),
        ),
        "the job `verify-darwin-arm64` of artifacts.yml verifies and has `if:",
    );
    red(
        &plant(
            "artifacts.yml",
            "  sums-linux:\n    needs: [linux-x86_64, linux-arm64]\n    if: ${{ (github.event_name != 'pull_request' || contains(github.event.pull_request.labels.*.name, 'needs-artifacts')) && inputs.stage != 'sign' }}\n",
            "  sums-linux:\n    needs: [linux-x86_64, linux-arm64]\n    if: ${{ github.event_name != 'pull_request' || contains(github.event.pull_request.labels.*.name, 'needs-artifacts') }}\n",
        ),
        "the job `sums-linux` of artifacts.yml has `if:",
    );
    red(
        &plant(
            "artifacts.yml",
            "  sums:\n    needs: [darwin-arm64, linux-x86_64, linux-arm64]\n    if: ${{ (github.event_name != 'pull_request' || contains(github.event.pull_request.labels.*.name, 'needs-artifacts')) && inputs.stage != 'sign' }}\n",
            "  sums:\n    needs: [darwin-arm64, linux-x86_64, linux-arm64]\n",
        ),
        "the job `sums` of artifacts.yml has `if: (none)`",
    );
    // The exclusion must apply to the whole condition: an `||` beside it, or a group that closes
    // before the end, still lets the sign call run the job.
    let good = "  sums:\n    needs: [darwin-arm64, linux-x86_64, linux-arm64]\n    if: ${{ (github.event_name != 'pull_request' || contains(github.event.pull_request.labels.*.name, 'needs-artifacts')) && inputs.stage != 'sign' }}\n";
    for loose in [
        "${{ github.event_name != 'pull_request' || inputs.stage != 'sign' }}",
        "${{ (github.event_name != 'pull_request') || (github.ref != '') && inputs.stage != 'sign' }}",
        "${{ (github.event_name != 'pull_request') && inputs.stage != 'sign' || always() }}",
    ] {
        red(
            &plant(
                "artifacts.yml",
                good,
                &format!(
                    "  sums:\n    needs: [darwin-arm64, linux-x86_64, linux-arm64]\n    if: {loose}\n"
                ),
            ),
            "the job `sums` of artifacts.yml has `if:",
        );
    }
    red(
        &plant(
            "artifacts.yml",
            "  attest-linux:\n",
            "  attest-linux:\n    needs: [sums-linux]\n",
        ),
        "the job `attest-linux` of artifacts.yml needs `sums-linux`, which the sign call skips",
    );
    red(
        &plant(
            "artifacts.yml",
            "\njobs:\n",
            "\njobs:\n  extra:\n    runs-on: ubuntu-24.04\n    steps: []\n",
        ),
        "the job `extra` of artifacts.yml has `if: (none)`",
    );
}

#[test]
fn a_stage_that_can_be_skipped_or_continue_after_red_is_refused() {
    // Catches: a skipped stage (a skipped job counts as success for the jobs after it and for
    // the run), `always()`, `failure()`, `cancelled()` and `continue-on-error`: a red or
    // cancelled stage must stop the stages after it.
    red(
        &plant(
            "pipeline.yml",
            "  smoke:\n    needs: [artifacts-sign]\n",
            "  smoke:\n    needs: [artifacts-sign]\n    if: ${{ github.event.before != '' }}\n",
        ),
        "the job `smoke` has `if:",
    );
    red(
        &plant(
            "pipeline.yml",
            "  beta:\n",
            "  beta:\n    if: ${{ github.event.before != '' }}\n",
        ),
        "the job `beta` has `if:",
    );
    red(
        &plant(
            "pipeline.yml",
            "  ci:\n",
            "  ci:\n    if: ${{ always() }}\n",
        ),
        "uses `always()`",
    );
    red(
        &plant(
            "pipeline.yml",
            "  artifacts:\n    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n",
            "  artifacts:\n    if: ${{ always() && github.event_name == 'push' }}\n",
        ),
        "uses `always()`",
    );
    red(
        &plant(
            "pipeline.yml",
            "  smoke:\n    needs: [artifacts-sign]\n",
            "  smoke:\n    needs: [artifacts]\n    continue-on-error: true\n",
        ),
        "uses `continue-on-error`",
    );
}

#[test]
fn the_run_is_green_only_when_every_stage_is() {
    // Catches: a closing job that does not need a stage (the run would be green, and the
    // release made, with that stage red or still running), one that can be skipped into a pass,
    // and a stage that does not run because its workflow was dropped from the closing job's list.
    red(
        &plant(
            "pipeline.yml",
            "    needs: [ci, beta, artifacts, artifacts-sign, smoke]\n",
            "    needs: [ci, artifacts, artifacts-sign, smoke]\n",
        ),
        "the job `passed` needs",
    );
    red(
        &plant(
            "pipeline.yml",
            "    needs: [ci, beta, artifacts, artifacts-sign, smoke]\n",
            "    needs: [ci, beta, artifacts, smoke, watch]\n",
        ),
        "the job `passed` needs",
    );
    red(
        &plant(
            "pipeline.yml",
            "  passed:\n",
            "  passed:\n    if: ${{ github.event.before != '' }}\n",
        ),
        "the job `passed` has `if:",
    );
}

#[test]
fn the_watch_cancels_the_run_from_its_last_step_alone_and_only_for_a_push_to_main() {
    // Catches: no fail-fast (a watch that is gone or runs another command or another closing
    // job), a watch that waits for the stages it is to cancel, one that can run for a pull request
    // (the lint lets `actions: write` into no other job), a token in a step that runs before the
    // cancel (a build script would run beside it), a build after the token, a step after the
    // cancel, and scopes beyond `contents: read` and `actions: write`.
    red(
        &plant("pipeline.yml", "  watch:\n", "  watch:\n    needs: [ci]\n"),
        "the job `watch` needs",
    );
    red(
        &plant(
            "pipeline.yml",
            "  watch:\n    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n",
            "  watch:\n    if: ${{ always() }}\n",
        ),
        "the job `watch` has `if:",
    );
    red(
        &plant("pipeline.yml", "--last-job passed", "--last-job ci"),
        "the job `watch` does not run",
    );
    red(
        &plant(
            "pipeline.yml",
            "      - run: cargo build -p xtask --locked\n",
            "      - run: cargo build -p xtask --locked\n        env:\n          GH_TOKEN: ${{ github.token }}\n",
        ),
        "other than the one that cancels holds a token",
    );
    red(
        &plant(
            "pipeline.yml",
            "          GH_TOKEN: ${{ github.token }}\n",
            "          GH_TOKEN: ${{ github.token }}\n          OTHER: 1\n",
        ),
        "does not have exactly `GH_TOKEN",
    );
    red(
        &plant(
            "pipeline.yml",
            "--last-job passed\n",
            "--last-job passed\n      - run: cargo build -p xtask --locked\n",
        ),
        "builds after the step that holds the token",
    );
    red(
        &plant(
            "pipeline.yml",
            "--last-job passed\n",
            "--last-job passed\n      - run: echo done\n",
        ),
        "is not its last",
    );
    red(
        &plant(
            "pipeline.yml",
            "      actions: write\n",
            "      actions: read\n",
        ),
        "the job `watch` grants the scopes",
    );
    red(
        &plant(
            "pipeline.yml",
            "      actions: write\n",
            "      actions: write\n      packages: write\n",
        ),
        "the job `watch` grants the scopes",
    );
}

#[test]
fn the_called_workflows_still_run_on_pull_requests() {
    // Catches: the pull-request gate or the merge queue losing ci.yml or artifacts.yml when
    // their push trigger moved into the pipeline, a called workflow that cannot be called,
    // and beta (which tests a push to main's artifacts) gaining a pull-request trigger.
    red(
        &plant("ci.yml", "  pull_request:\n", ""),
        "ci.yml does not run on `pull_request`",
    );
    red(
        &plant("ci.yml", "  merge_group:\n", ""),
        "ci.yml does not run on `merge_group`",
    );
    red(
        &plant(
            "artifacts.yml",
            "  pull_request:\n    # `labeled`: adding `needs-artifacts` to an open pull request starts the build.\n    types: [opened, synchronize, reopened, labeled]\n",
            "",
        ),
        "artifacts.yml does not run on `pull_request`",
    );
    red(
        &plant("artifacts.yml", "  merge_group:\n", ""),
        "artifacts.yml does not run on `merge_group`",
    );
    for file in ["ci.yml", "artifacts.yml"] {
        red(
            &plant(file, "  workflow_call:\n", ""),
            &format!("{file} has no `workflow_call` trigger"),
        );
    }
    red(
        &plant(
            "beta.yml",
            "  workflow_call:\n",
            "  workflow_call:\n  pull_request:\n",
        ),
        "beta.yml runs on `pull_request`",
    );
    red(
        &plant(
            "smoke.yml",
            "  workflow_call:\n",
            "  workflow_call:\n  pull_request:\n",
        ),
        "smoke.yml runs on `pull_request`",
    );
}

#[test]
fn a_push_to_main_runs_each_check_once() {
    // Catches: a second run of a check beside the pipeline's: a push trigger left on (or put
    // back on) ci.yml, artifacts.yml or beta.yml, another workflow calling one of them, or the
    // pipeline not calling one.
    for file in ["ci.yml", "artifacts.yml", "beta.yml", "smoke.yml"] {
        let from = "  workflow_call:\n";
        red(
            &plant(
                file,
                from,
                "  workflow_call:\n  push:\n    branches: [main]\n",
            ),
            &format!("{file} has a `push` trigger"),
        );
    }
    red(
        &plant(
            "release.yml",
            "\njobs:\n",
            "\njobs:\n  again:\n    uses: ./.github/workflows/ci.yml\n",
        ),
        "ci.yml is called by [\"pipeline.yml#ci\", \"release.yml#again\"]",
    );
    red(
        &plant(
            "pipeline.yml",
            "uses: ./.github/workflows/beta.yml",
            "uses: ./.github/workflows/ci.yml",
        ),
        "beta.yml is called by []",
    );
    red(
        &plant(
            "pipeline.yml",
            "on:\n  push:\n    branches: [main]\n",
            "on:\n  push:\n    branches: [main]\n  pull_request:\n",
        ),
        "runs on [\"push\", \"pull_request\"]",
    );
}

#[test]
fn only_a_push_to_main_runs_the_pipeline() {
    // Catches: the pipeline started by a branch push, a pull request or a hand dispatch (the
    // attest jobs would sign, or refuse, for them).
    red(
        &plant(
            "pipeline.yml",
            "    branches: [main]\n",
            "    branches: [main, 'release/**']\n",
        ),
        "is filtered by",
    );
    red(
        &plant("pipeline.yml", "    branches: [main]\n", ""),
        "is filtered by",
    );
    red(
        &plant(
            "pipeline.yml",
            "on:\n  push:\n    branches: [main]\n",
            "on:\n  push:\n    branches: [main]\n  workflow_dispatch:\n",
        ),
        "not on `push` alone",
    );
}

#[test]
fn smoke_tests_the_artifacts_of_the_same_run_and_builds_no_kbf() {
    // Catches: a smoke that rebuilds the kbf it tests, takes it from another run, skips the
    // check of the downloaded files, or runs the farm from another directory than the one
    // holding the artifact's kbf.
    red(
        &plant(
            "smoke.yml",
            "          pattern: assets-*\n          path: dist\n          merge-multiple: true\n",
            "          pattern: assets-*\n          path: dist\n          merge-multiple: true\n          run-id: 1\n",
        ),
        "smoke.yml names a `run-id`",
    );
    red(
        &plant(
            "smoke.yml",
            "          pattern: assets-*\n",
            "          pattern: bundles-*\n",
        ),
        "does not download `assets-*` into `dist`",
    );
    red(
        &plant(
            "smoke.yml",
            "          name: sha256sums\n",
            "          name: other\n",
        ),
        "does not download `sha256sums` into `dist`",
    );
    red(
        &plant(
            "smoke.yml",
            "run: cargo xtask verify-assets sums dist",
            "run: ls dist",
        ),
        "does not check the downloaded files",
    );
    red(
        &plant(
            "smoke.yml",
            "run: cargo xtask build-commit check-asset",
            "run: echo check-asset",
        ),
        "does not run `build-commit check-asset`",
    );
    red(
        &plant(
            "smoke.yml",
            "-C \"$RUNNER_TEMP/bin\" kbf",
            "-C \"$RUNNER_TEMP/bin\" kbf-deploy",
        ),
        "does not check the files, then check the tarball's kbf",
    );
    red(
        &plant(
            "smoke.yml",
            "--bin-dir \"$RUNNER_TEMP/bin\"",
            "--bin-dir target/debug",
        ),
        "does not run from `--bin-dir",
    );
    red(
        &plant(
            "smoke.yml",
            "run: cargo build -p kbf-harness -p kbf-integration-test --bins --locked",
            "run: cargo build -p kbf-cli -p kbf-harness -p kbf-integration-test --bins --locked",
        ),
        "the job `smoke` builds `kbf`",
    );
}

#[test]
fn beta_downloads_no_artifact_so_it_can_start_beside_ci() {
    // Catches: a job added to beta.yml that reads the run's artifacts, which do not exist yet
    // when beta starts beside ci: it would fail on every push.
    red(
        &plant(
            "beta.yml",
            "  test-x86:\n    runs-on: ubuntu-latest\n",
            "  test-x86:\n    runs-on: ubuntu-latest\n    env:\n      X: actions/download-artifact\n",
        ),
        "beta.yml downloads an artifact",
    );
}

#[test]
fn a_running_pipeline_is_never_cancelled_and_neither_is_a_release() {
    // Catches: a pipeline that cancels the run in progress (a build and its attestations
    // thrown away when a push arrives, so a busy main would never finish one); a pipeline holding anything
    // that writes (a tag, a release, the bot's key, an environment), which a cancel could leave
    // half done; a release in the pipeline's group, or one that cancels a release; a called
    // workflow whose group would cancel its caller.
    red(
        &plant(
            "pipeline.yml",
            "  cancel-in-progress: false\n",
            "  cancel-in-progress: true\n",
        ),
        "a running pipeline always finishes",
    );
    red(
        &plant(
            "pipeline.yml",
            "  group: pipeline-${{ github.ref }}\n",
            "  group: pipeline\n",
        ),
        "the pipeline's concurrency is",
    );
    red(
        &plant(
            "pipeline.yml",
            "  ci:\n    uses: ./.github/workflows/ci.yml\n    permissions:\n      contents: read\n",
            "  ci:\n    uses: ./.github/workflows/ci.yml\n    permissions:\n      contents: write\n",
        ),
        "the job `ci` grants the scopes",
    );
}

#[test]
fn a_called_workflow_never_asks_for_more_than_its_caller_grants() {
    // Catches: a job of ci.yml, artifacts.yml or beta.yml with a scope (or a level) its calling
    // job in pipeline.yml does not grant, which GitHub refuses at startup for the whole
    // pipeline: no ci, artifacts, beta or release on a push to main. Pull-request runs cannot
    // show it, because the pipeline has no pull-request trigger.
    red(
        &plant(
            "pipeline.yml",
            "    uses: ./.github/workflows/beta.yml\n    permissions:\n      contents: read\n      attestations: read\n",
            "    uses: ./.github/workflows/beta.yml\n    permissions:\n      contents: read\n",
        ),
        "the job `mixed-version` of beta.yml asks for `attestations: read`",
    );
    // Both calls are checked: GitHub compares the permissions of a called workflow's jobs with
    // the caller's grant whether or not the job runs in that call.
    for stage in ["build", "sign"] {
        let job = if stage == "build" {
            "artifacts"
        } else {
            "artifacts-sign"
        };
        red(
            &plant(
                "pipeline.yml",
                &format!(
                    "      stage: {stage}\n    permissions:\n      contents: read\n      id-token: write\n      attestations: write\n"
                ),
                &format!(
                    "      stage: {stage}\n    permissions:\n      contents: read\n      id-token: write\n      attestations: read\n"
                ),
            ),
            &format!(
                "the job `attest-linux` of artifacts.yml asks for `attestations: write`, but the job `{job}` of pipeline.yml"
            ),
        );
    }
    red(
        &plant(
            "ci.yml",
            "      contents: read\n    env:\n      DENY_VERSION",
            "      contents: read\n      packages: read\n    env:\n      DENY_VERSION",
        ),
        "of ci.yml asks for `packages: read`",
    );
}

#[test]
fn no_stage_passes_secrets_to_its_workflow() {
    // Catches: `secrets: inherit` (or a list of secrets) on the call of ci, artifacts or beta,
    // which would put the release bot's key in a run a newer push cancels.
    for (call, next) in [
        ("  ci:\n    uses: ./.github/workflows/ci.yml\n", "ci"),
        ("  artifacts:\n", "artifacts"),
        (
            "  artifacts-sign:\n    needs: [ci, artifacts]\n",
            "artifacts-sign",
        ),
        ("  beta:\n    uses: ./.github/workflows/beta.yml\n", "beta"),
        ("  smoke:\n    needs: [artifacts-sign]\n", "smoke"),
    ] {
        red(
            &plant(
                "pipeline.yml",
                call,
                &format!("{call}    secrets: inherit\n"),
            ),
            &format!("the job `{next}` passes `secrets:`"),
        );
    }
}

#[test]
fn the_release_follows_this_pipeline() {
    // Catches: a rename of the pipeline the release does not follow (it would never start), a
    // release that follows another workflow.
    red(
        &plant("pipeline.yml", "name: pipeline\n", "name: main\n"),
        "pipeline.yml is named",
    );
    red(
        &plant("release.yml", "workflows: [pipeline]", "workflows: [ci]"),
        "release.yml follows",
    );
}
