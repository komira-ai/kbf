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
fn the_pipeline_runs_ci_then_artifacts_then_beta_each_needing_the_one_before() {
    // Catches: a stage that can start before the one it follows (so beta could test files
    // that are not built yet, or the release follow a run that skipped a check), a stage
    // calling another workflow, and a stage added or reordered.
    red(
        &plant("pipeline.yml", "    needs: [ci]\n", "    needs: []\n"),
        "the job `artifacts` needs [], not [\"ci\"]",
    );
    red(
        &plant(
            "pipeline.yml",
            "    needs: [artifacts]\n",
            "    needs: [ci]\n",
        ),
        "the job `beta` needs [\"ci\"], not [\"artifacts\"]",
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
fn a_stage_that_can_be_skipped_or_continue_after_red_is_refused() {
    // Catches: a skipped stage (a skipped job counts as success for the jobs after it and for
    // the run), `always()`, `failure()`, `cancelled()` and `continue-on-error`: a red or
    // cancelled stage must stop the stages after it.
    red(
        &plant(
            "pipeline.yml",
            "  beta:\n    needs: [artifacts]\n",
            "  beta:\n    needs: [artifacts]\n    if: ${{ github.event.before != '' }}\n",
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
            "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n",
            "    if: ${{ always() && github.event_name == 'push' }}\n",
        ),
        "uses `always()`",
    );
    red(
        &plant(
            "pipeline.yml",
            "  beta:\n    needs: [artifacts]\n",
            "  beta:\n    needs: [artifacts]\n    continue-on-error: true\n",
        ),
        "uses `continue-on-error`",
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
}

#[test]
fn a_push_to_main_runs_each_check_once() {
    // Catches: a second run of a check beside the pipeline's: a push trigger left on (or put
    // back on) ci.yml, artifacts.yml or beta.yml, another workflow calling one of them, or the
    // pipeline not calling one.
    for file in ["ci.yml", "artifacts.yml", "beta.yml"] {
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
        "ci.yml is called by [\"pipeline.yml\", \"release.yml\"]",
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
fn beta_tests_the_artifacts_of_the_same_run_and_builds_no_kbf() {
    // Catches: a beta that rebuilds the kbf it tests, takes it from another run, skips the
    // check of the downloaded files, or runs the farm from another directory than the one
    // holding the artifact's kbf.
    red(
        &plant(
            "beta.yml",
            "          pattern: assets-*\n          path: dist\n          merge-multiple: true\n",
            "          pattern: assets-*\n          path: dist\n          merge-multiple: true\n          run-id: 1\n",
        ),
        "beta.yml names a `run-id`",
    );
    red(
        &plant(
            "beta.yml",
            "          pattern: assets-*\n",
            "          pattern: bundles-*\n",
        ),
        "does not download `assets-*` into `dist`",
    );
    red(
        &plant(
            "beta.yml",
            "          name: sha256sums\n",
            "          name: other\n",
        ),
        "does not download `sha256sums` into `dist`",
    );
    red(
        &plant(
            "beta.yml",
            "run: cargo xtask verify-assets sums dist",
            "run: ls dist",
        ),
        "does not check the downloaded files",
    );
    red(
        &plant(
            "beta.yml",
            "run: cargo xtask build-commit check-asset",
            "run: echo check-asset",
        ),
        "does not run `build-commit check-asset`",
    );
    red(
        &plant(
            "beta.yml",
            "-C \"$RUNNER_TEMP/bin\" kbf",
            "-C \"$RUNNER_TEMP/bin\" kbf-deploy",
        ),
        "does not check the files, then check the tarball's kbf",
    );
    red(
        &plant(
            "beta.yml",
            "--bin-dir \"$RUNNER_TEMP/bin\"",
            "--bin-dir target/debug",
        ),
        "does not run from `--bin-dir",
    );
    red(
        &plant(
            "beta.yml",
            "run: cargo build -p kbf-harness -p kbf-integration-test --bins --locked",
            "run: cargo build -p kbf-cli -p kbf-harness -p kbf-integration-test --bins --locked",
        ),
        "the job `smoke` builds `kbf`",
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
            "    permissions:\n      contents: read\n\n  artifacts:",
            "    permissions:\n      contents: write\n\n  artifacts:",
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
    red(
        &plant(
            "pipeline.yml",
            "      attestations: write\n",
            "      attestations: read\n",
        ),
        "the job `attest-linux` of artifacts.yml asks for `attestations: write`",
    );
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
        ("  artifacts:\n    needs: [ci]\n", "artifacts"),
        ("  beta:\n    needs: [artifacts]\n", "beta"),
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
