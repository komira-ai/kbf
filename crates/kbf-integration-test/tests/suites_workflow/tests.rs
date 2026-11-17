//! Tests of the suites lint. The real workflow is the control (no problem); each other test
//! changes one thing in the real file and names the rule it holds.

use super::*;

fn real() -> String {
    std::fs::read_to_string(crate::repo_root().join(".github/workflows/suites.yml"))
        .expect("suites.yml")
}

/// The problems with `from` replaced by `to` once in the real file, which must hold it once.
fn plant(from: &str, to: &str) -> Vec<String> {
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

const RELEASE_RED_NEEDS: &str =
    "    needs: [beta]\n    if: ${{ always() && github.ref == 'refs/heads/main' }}\n";
const TOKEN_STEP: &str = "      - name: Keep the RELEASE-RED issue\n        env:\n          GH_TOKEN: ${{ github.token }}\n";

#[test]
fn the_real_suites_workflow_meets_every_rule() {
    // Catches: suites.yml drifting from its shape; the control for every test below.
    let found = problems(&real()).expect("suites.yml is read");
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
fn only_a_push_to_main_runs_the_suites() {
    // Catches: the suite started by a branch push, a pull request or a hand dispatch (the
    // issue job would write issues for a run that is not main's), a rename, and a missing
    // top-level `permissions: {}`.
    red(
        &plant(
            "    branches: [main]\n",
            "    branches: [main, 'release/**']\n",
        ),
        "is filtered by",
    );
    red(&plant("    branches: [main]\n", ""), "is filtered by");
    red(
        &plant(
            "on:\n  push:\n    branches: [main]\n",
            "on:\n  push:\n    branches: [main]\n  pull_request:\n",
        ),
        "not on `push` alone",
    );
    red(
        &plant(
            "on:\n  push:\n    branches: [main]\n",
            "on:\n  push:\n    branches: [main]\n  workflow_dispatch:\n",
        ),
        "not on `push` alone",
    );
    red(
        &plant("name: suites\n", "name: beta\n"),
        "suites.yml is named",
    );
    red(
        &plant("\npermissions: {}\n", "\npermissions:\n  contents: read\n"),
        "no top-level `permissions: {}`",
    );
}

#[test]
fn a_suite_that_has_started_is_never_cancelled() {
    // Catches: a group shared with the pipeline or another ref, and a push that cancels the
    // running suite (an hour of tests thrown away, and no issue kept for it).
    red(
        &plant(
            "  cancel-in-progress: false\n",
            "  cancel-in-progress: true\n",
        ),
        "the concurrency of suites.yml is",
    );
    red(
        &plant(
            "  group: suites-${{ github.ref }}\n",
            "  group: pipeline-${{ github.ref }}\n",
        ),
        "the concurrency of suites.yml is",
    );
    red(
        &plant("  group: suites-${{ github.ref }}\n", "  group: suites\n"),
        "the concurrency of suites.yml is",
    );
}

#[test]
fn the_jobs_are_beta_then_release_red() {
    // Catches: a job added, dropped or reordered (a third job would run beside the token or
    // beside the suite unreviewed).
    red(
        &plant(
            "  release-red:\n",
            "  extra:\n    runs-on: ubuntu-24.04\n    steps: []\n\n  release-red:\n",
        ),
        "the jobs of suites.yml are",
    );
}

#[test]
fn beta_calls_beta_yml_with_nothing_else() {
    // Catches: the call pointed at another workflow, made conditional (a skipped suite
    // reports nothing), made to wait, given secrets or steps, or granted more or less than
    // beta.yml's jobs ask for.
    red(
        &plant(
            "uses: ./.github/workflows/beta.yml",
            "uses: ./.github/workflows/ci.yml",
        ),
        "the job `beta` of suites.yml calls",
    );
    red(
        &plant(
            "  beta:\n",
            "  beta:\n    if: ${{ github.event.before != '' }}\n",
        ),
        "the job `beta` of suites.yml has `if:",
    );
    red(
        &plant("  beta:\n", "  beta:\n    needs: [release-red]\n"),
        "the job `beta` of suites.yml needs",
    );
    red(
        &plant(
            "uses: ./.github/workflows/beta.yml\n",
            "uses: ./.github/workflows/beta.yml\n    secrets: inherit\n",
        ),
        "the job `beta` of suites.yml passes `secrets:`",
    );
    red(
        &plant(
            "uses: ./.github/workflows/beta.yml\n",
            "uses: ./.github/workflows/beta.yml\n    steps: []\n",
        ),
        "the job `beta` of suites.yml has steps",
    );
    red(
        &plant("      attestations: read\n", ""),
        "the job `beta` of suites.yml grants the scopes",
    );
    red(
        &plant("      attestations: read\n", "      attestations: write\n"),
        "the job `beta` of suites.yml grants the scopes",
    );
    red(
        &plant(
            "      attestations: read\n",
            "      attestations: read\n      packages: read\n",
        ),
        "the job `beta` of suites.yml grants the scopes",
    );
}

#[test]
fn release_red_runs_after_the_suite_whatever_it_did_and_only_for_main() {
    // Catches: an issue job that waits for nothing (it reads a suite still running), one that
    // does not run after a red suite (the issue would never open), and one that can run for a
    // ref that is not main.
    red(
        &plant(
            RELEASE_RED_NEEDS,
            "    if: ${{ always() && github.ref == 'refs/heads/main' }}\n",
        ),
        "the job `release-red` needs [], not",
    );
    red(
        &plant("    needs: [beta]\n", "    needs: [beta, release-red]\n"),
        "the job `release-red` needs",
    );
    for condition in [
        "${{ always() }}",
        "${{ github.ref == 'refs/heads/main' }}",
        "${{ always() && github.ref != 'refs/heads/main' }}",
        "${{ always() || github.ref == 'refs/heads/main' }}",
        "${{ failure() && github.ref == 'refs/heads/main' }}",
    ] {
        red(
            &plant(
                RELEASE_RED_NEEDS,
                &format!("    needs: [beta]\n    if: {condition}\n"),
            ),
            "the job `release-red` has `if:",
        );
    }
    red(
        &plant(RELEASE_RED_NEEDS, "    needs: [beta]\n"),
        "the job `release-red` has `if: (none)`",
    );
}

#[test]
fn release_red_holds_exactly_its_four_scopes() {
    // Catches: scopes beyond `contents: read`, `actions: read`, `checks: read` and `issues:
    // write` on the job that holds the issue token, and one of the four missing.
    red(
        &plant(
            "      issues: write\n",
            "      issues: write\n      packages: write\n",
        ),
        "the job `release-red` of suites.yml grants the scopes",
    );
    red(
        &plant("      issues: write\n", "      issues: read\n"),
        "the job `release-red` of suites.yml grants the scopes",
    );
    red(
        &plant("      actions: read\n", "      actions: write\n"),
        "the job `release-red` of suites.yml grants the scopes",
    );
    red(
        &plant("      issues: write\n", ""),
        "the job `release-red` of suites.yml grants the scopes",
    );
    red(
        &plant("      checks: read\n", ""),
        "the job `release-red` of suites.yml grants the scopes",
    );
}

#[test]
fn the_token_is_held_by_the_last_step_alone_after_xtask_is_built() {
    // Catches: no step that keeps the issue (a command changed), a token in another
    // environment variable, a token or a secret in an earlier step (a build script would run
    // beside it), the build of xtask after the token or missing, and a step after the one that
    // holds the token.
    red(
        &plant("--run-id \"$GITHUB_RUN_ID\"", "--run-id 1"),
        "the last step of `release-red` does not run",
    );
    red(
        &plant(
            "          GH_TOKEN: ${{ github.token }}\n",
            "          GH_TOKEN: ${{ github.token }}\n          OTHER: 1\n",
        ),
        "does not have exactly `GH_TOKEN",
    );
    red(
        &plant(
            "          GH_TOKEN: ${{ github.token }}\n",
            "          TOKEN: ${{ github.token }}\n",
        ),
        "does not have exactly `GH_TOKEN",
    );
    red(
        &plant(
            "      - run: cargo build -p xtask --locked\n",
            "      - run: cargo build -p xtask --locked\n        env:\n          GH_TOKEN: ${{ github.token }}\n",
        ),
        "before the last holds a token or a secret",
    );
    red(
        &plant(
            "      - run: cargo build -p xtask --locked\n",
            "      - run: cargo build -p xtask --locked\n        env:\n          KEY: ${{ secrets.KEY }}\n",
        ),
        "before the last holds a token or a secret",
    );
    red(
        &plant(
            "      - run: cargo build -p xtask --locked\n",
            "      - run: cargo build -p xtask\n",
        ),
        "does not run `cargo build -p xtask --locked` before its last step",
    );
    red(
        &plant(
            "      - run: cargo build -p xtask --locked\n",
            "      - run: cargo build -p xtask --locked --release\n",
        ),
        "does not run `cargo build -p xtask --locked` before its last step",
    );
    // The build moved behind the token: it is then the last step, and the keeper is not.
    let text = real();
    let keeper = text
        .find(TOKEN_STEP)
        .expect("the step that holds the token");
    let moved = format!(
        "{}      - run: cargo build -p xtask --locked\n",
        &text[keeper..]
    );
    let without = text[..keeper].replace("      - run: cargo build -p xtask --locked\n", "");
    red(
        &problems(&format!("{without}{moved}")).expect("the workflow is read"),
        "the last step of `release-red` does not run",
    );
    red(
        &problems(&format!("{text}      - run: echo done\n")).expect("the workflow is read"),
        "the last step of `release-red` does not run",
    );
}

#[test]
fn a_job_without_steps_is_reported() {
    // Catches: an emptied `release-red`, which would hold the grant and keep no issue.
    let text = real();
    let at = text.find("    steps:\n").expect("steps");
    let found = problems(&format!("{}    steps: []\n", &text[..at])).expect("the workflow is read");
    red(&found, "has no steps");
}
