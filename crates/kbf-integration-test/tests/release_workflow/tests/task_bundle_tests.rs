//! Tests of the rules of the job that pushes the macOS `kbf-task` bundle
//! ([`super::super::task_bundle`]): each changes the real workflow in one place and names the rule
//! it catches.

use super::*;

const JOB: &str = "task-bundle";
const PUSH: &str = "cargo xtask push-task-bundle --asset \"dist/kbf-task-bundle-$COMMIT-darwin-arm64.tar.gz\" --repository \"ghcr.io/$GITHUB_REPOSITORY_OWNER/kbf-task\" --tag \"$TAG\" --summary \"$GITHUB_STEP_SUMMARY\"";
const SUMS: &str = "        run: sha256sum --check --strict SHA256SUMS\n";

#[test]
fn the_real_job_is_clean_and_holds_packages_write_alone() {
    // Catches: the job dropped (its row in the graph names it), and a second job that may write
    // packages: `publish` given `packages: write` is red twice, as a writer outside the list and
    // as a job that names the scope.
    let found = problems(&real()).expect("read");
    assert!(found.is_empty(), "{found:#?}");
    let publish = plant(
        "publish",
        "      contents: write\n",
        "      contents: write\n      packages: write\n",
    );
    red(&publish, "`publish` holds `packages: write`");
    red(&publish, "the job `publish` names the `packages` scope");
    red(
        &plant(JOB, "      packages: write\n", "      packages: read\n"),
        "the job `task-bundle` holds the scopes",
    );
    red(
        &plant(
            JOB,
            "      packages: write\n",
            "      packages: write\n      contents: write\n",
        ),
        "`task-bundle` holds `contents: write`",
    );
}

#[test]
fn the_job_runs_only_after_the_release_is_published_and_the_files_checked() {
    // Catches: a push before the release exists or without the gate jobs, a push under a looser
    // condition, a push in an environment, and a push before (or without) the sums check.
    red(
        &plant(
            JOB,
            "needs: [plan, verify, publish]",
            "needs: [plan, verify]",
        ),
        "the job `task-bundle` needs",
    );
    red(
        &plant(
            JOB,
            "needs: [plan, verify, publish]",
            "needs: [plan, publish]",
        ),
        "the job `task-bundle` needs",
    );
    red(
        &plant(JOB, " && needs.plan.outputs.code == 'true' }}", " }}"),
        "the job `task-bundle` has `if: ",
    );
    red(
        &plant(
            JOB,
            "    runs-on: ubuntu-24.04\n",
            "    runs-on: ubuntu-24.04\n    environment: release\n",
        ),
        "the job `task-bundle` has environment",
    );
    red(
        &plant(JOB, SUMS, "        run: echo no check\n"),
        "the job `task-bundle` does not check the files against SHA256SUMS",
    );
}

#[test]
fn the_push_is_of_this_commits_asset_to_the_one_repository_with_the_token_alone() {
    // Catches: another asset, another registry or repository, another tag, no summary, a token
    // in a step that does not push (or a second variable beside it), the planned tag replaced,
    // and a third-party action or a download of another artifact.
    for (from, to, want) in [
        (
            "--asset \"dist/kbf-task-bundle-$COMMIT-darwin-arm64.tar.gz\"",
            "--asset \"dist/kbf-task-bundle-old-darwin-arm64.tar.gz\"",
            "is not given `--asset dist/kbf-task-bundle-$COMMIT-darwin-arm64.tar.gz`",
        ),
        (
            "--repository \"ghcr.io/$GITHUB_REPOSITORY_OWNER/kbf-task\"",
            "--repository \"docker.io/someone/kbf-task\"",
            "is not given `--repository ghcr.io/$GITHUB_REPOSITORY_OWNER/kbf-task`",
        ),
        (
            "--tag \"$TAG\"",
            "--tag latest",
            "is not given `--tag $TAG`",
        ),
        (
            " --summary \"$GITHUB_STEP_SUMMARY\"",
            "",
            "is not given `--summary $GITHUB_STEP_SUMMARY`",
        ),
        (
            "          TAG: ${{ needs.plan.outputs.tag }}\n",
            "          TAG: v0.0.1\n",
            "does not set `TAG: ${{ needs.plan.outputs.tag }}`",
        ),
        (
            "          TAG: ${{ needs.plan.outputs.tag }}\n",
            "          TAG: ${{ needs.plan.outputs.tag }}\n          EXTRA: x\n",
            "not exactly `TAG` and `GH_TOKEN`",
        ),
        (
            "      - run: cargo build -p xtask --locked\n",
            "      - run: cargo build -p xtask --locked\n        env:\n          GH_TOKEN: ${{ github.token }}\n",
            "a step of `task-bundle` that does not push holds `GH_TOKEN`",
        ),
        (
            "          name: release-assets\n",
            "          name: other-assets\n",
            "is not of `release-assets` into `dist`",
        ),
    ] {
        red(&plant(JOB, from, to), want);
    }
    red(
        &plant(JOB, PUSH, "cargo xtask push-task-bundle --asset x"),
        "the job `task-bundle` runs",
    );
    red(
        &plant(
            JOB,
            "      - uses: actions/download-artifact@",
            "      - uses: someone/download@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v1\n        with:\n          name: x\n      - uses: actions/download-artifact@",
        ),
        "the job `task-bundle` uses `someone/download@",
    );
}

#[test]
fn the_sums_are_checked_before_the_push() {
    // Catches: the check moved after the push, where a file that does not match its sum would
    // already be on the registry.
    let text = real();
    let job_at = text.find("\n  task-bundle:\n").expect("the job");
    let region = &text[job_at..];
    let check = "      - name: Check the files against SHA256SUMS\n        timeout-minutes: 5\n        working-directory: dist\n        run: sha256sum --check --strict SHA256SUMS\n";
    let push_at = region
        .find("      - name: Push the macOS kbf-task bundle")
        .expect("the push");
    let push_end = region[push_at..]
        .find("\n\n")
        .map_or(region.len(), |n| push_at + n + 1);
    let push = &region[push_at..push_end];
    let moved = region
        .replacen(check, "", 1)
        .replacen(push, &format!("{push}{check}"), 1);
    let planted = [&text[..job_at], moved.as_str()].concat();
    let found = problems(&planted).expect("read");
    red(
        &found,
        "the job `task-bundle` pushes before it checks the files against SHA256SUMS",
    );
}
