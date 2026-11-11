//! Tests of the frontend deploy's rules ([`super::super::deploy`]): each changes the real
//! workflow in one place and names the rule it catches.

use super::*;

const DIGEST: &str = "deploy-digest";
const DEPLOY: &str = "deploy";
const ENDPOINT_IF: &str = " && vars.CP_DEPLOY_ENDPOINT != '' }}";
const CALL: &str = "komira-ai/docker-swarm-updater/.github/workflows/deploy.yml@c1fcc47610e2886d4d9e561be3c0c76f0452d1f6 # v0.1.3";
const VERB: &str = "run: cargo xtask release image-digest --repo \"$GITHUB_REPOSITORY\" --tag \"$TAG\" --commit \"$COMMIT\" --output \"$GITHUB_OUTPUT\"";

/// Asserts that every plant of `job` in `plants` (from, to) is red with its own `want`.
fn each_red(job: &str, plants: &[(&str, &str, &str)]) {
    for (from, to, want) in plants {
        red(&plant(job, from, to), want);
    }
}

#[test]
fn both_jobs_run_only_for_a_release_with_the_endpoint_set_and_in_their_order() {
    // Catches: a deploy that runs when no endpoint is set (the endpoint test dropped from
    // either job, so a fork or a repository without the deploy front would call it), a
    // deploy that no longer waits for `gamma` or for the digest it is given, and a digest
    // job that runs before the release is published.
    let found = problems(&real()).expect("read");
    assert!(found.is_empty(), "{found:#?}");
    for job in [DIGEST, DEPLOY] {
        red(
            &plant(job, ENDPOINT_IF, " }}"),
            &format!("the job `{job}` has `if: "),
        );
    }
    each_red(
        DEPLOY,
        &[
            (
                "needs: [plan, gamma, deploy-digest]",
                "needs: [plan, deploy-digest]",
                "the job `deploy` needs",
            ),
            (
                "needs: [plan, gamma, deploy-digest]",
                "needs: [plan, gamma]",
                "the job `deploy` needs",
            ),
        ],
    );
    each_red(
        DIGEST,
        &[(
            "needs: [plan, publish]",
            "needs: [plan, verify]",
            "the job `deploy-digest` needs",
        )],
    );
}

#[test]
fn the_deploy_calls_exactly_the_pinned_workflow_with_exactly_its_inputs() {
    // Catches: another called workflow or commit, a changed, missing or added input (another
    // service, version, environment, tag, wait, or an address not from the digest job), and a
    // key that would give the called workflow more than its inputs and the token: `secrets`,
    // a matrix, an `env`, a runner or steps.
    let other = CALL.replace("c1fcc476", "00000000");
    red(&plant(DEPLOY, CALL, &other), "the job `deploy` calls");
    let plants = [
        ("service: kbf-frontend", "service: kbf-coordinator"),
        (
            "version: ${{ github.event.workflow_run.head_sha }}",
            "version: ${{ needs.plan.outputs.tag }}",
        ),
        ("environment: controlplane", "environment: release"),
        ("tailscale-tags: tag:cp-deploy", "tailscale-tags: tag:ci"),
        ("wait-minutes: 240", "wait-minutes: 0"),
        ("      wait-minutes: 240\n", ""),
        (
            "image-url: ${{ needs.deploy-digest.outputs.image-url }}",
            "image-url: https://example.com/x.tar",
        ),
        (
            "      tailscale-tags: tag:cp-deploy\n",
            "      tailscale-tags: tag:cp-deploy\n      signer-ref: refs/heads/other\n",
        ),
    ];
    for (from, to) in plants {
        red(&plant(DEPLOY, from, to), "the job `deploy` passes");
    }
    for (key, value) in [
        ("secrets", "inherit"),
        ("strategy", "{matrix: {a: [1]}}"),
        ("env", "{A: b}"),
        ("runs-on", "ubuntu-24.04"),
        ("steps", "[]"),
    ] {
        let found = plant(
            DEPLOY,
            "    permissions:\n      id-token: write\n",
            &format!("    permissions:\n      id-token: write\n    {key}: {value}\n"),
        );
        red(&found, &format!("the job `deploy` has `{key}:`"));
    }
}

#[test]
fn only_the_deploy_holds_the_oidc_token_and_nothing_else() {
    // Catches: the deploy given a scope beside `id-token: write` or losing it, the digest job
    // given the OIDC token or a write scope, and either job put in an environment.
    each_red(
        DEPLOY,
        &[
            (
                "      id-token: write\n",
                "      id-token: write\n      contents: read\n",
                "the job `deploy` holds the scopes",
            ),
            (
                "      id-token: write\n",
                "      id-token: read\n",
                "the job `deploy` holds the scopes",
            ),
        ],
    );
    each_red(
        DIGEST,
        &[
            (
                "      contents: read\n",
                "      contents: read\n      id-token: write\n",
                "the job `deploy-digest` holds `id-token: write`, which only",
            ),
            (
                "      contents: read\n",
                "      contents: write\n",
                "the job `deploy-digest` holds `contents: write`",
            ),
            (
                "    timeout-minutes: 30\n",
                "    timeout-minutes: 30\n    environment: controlplane\n",
                "the job `deploy-digest` has environment",
            ),
        ],
    );
}

#[test]
fn the_digest_job_runs_the_verb_for_this_release_and_nothing_else() {
    // Catches: the verb given another tag, commit or output, or not run at all; another
    // command or action in the job (a download of another run's files, a network join); an
    // `env:` beside the tag and the run token, or either missing; and outputs that are not
    // exactly the verb's three lines of its own step.
    let plants = [
        (
            VERB,
            &*VERB.replace("--tag \"$TAG\"", "--tag \"$COMMIT\""),
            "is not given `--tag $TAG`",
        ),
        (
            VERB,
            &*VERB.replace("--commit \"$COMMIT\"", "--commit \"$TAG\""),
            "is not given `--commit $COMMIT`",
        ),
        (
            VERB,
            &*VERB.replace("--output \"$GITHUB_OUTPUT\"", "--output out"),
            "is not given `--output $GITHUB_OUTPUT`",
        ),
        (
            VERB,
            &*VERB.replace("--repo \"$GITHUB_REPOSITORY\"", "--repo o/r"),
            "is not given `--repo $GITHUB_REPOSITORY`",
        ),
        (
            VERB,
            "run: cargo xtask release farm-version",
            "does not run `cargo xtask release image-digest`",
        ),
        (
            "      - run: rustup toolchain install\n",
            "      - run: rustup toolchain install\n      - run: cargo xtask fetch --url x\n",
            "the job `deploy-digest` runs",
        ),
        (
            "      - run: rustup toolchain install\n",
            "      - run: rustup toolchain install\n      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n",
            "the job `deploy-digest` uses `actions/download-artifact@",
        ),
        (
            "          TAG: ${{ needs.plan.outputs.tag }}\n",
            "          TAG: ${{ needs.plan.outputs.version }}\n",
            "does not set `TAG: ${{ needs.plan.outputs.tag }}`",
        ),
        (
            "          GH_TOKEN: ${{ github.token }}\n",
            "",
            "does not set `GH_TOKEN: ${{ github.token }}`",
        ),
        (
            "          GH_TOKEN: ${{ github.token }}\n",
            "          GH_TOKEN: ${{ github.token }}\n          EXTRA: x\n",
            "not exactly `TAG` and `GH_TOKEN`",
        ),
        (
            "      bundle-url: ${{ steps.image.outputs.bundle-url }}\n",
            "",
            "does not output `bundle-url`",
        ),
        (
            "      digest: ${{ steps.image.outputs.digest }}\n",
            "      digest: ${{ steps.other.outputs.digest }}\n",
            "does not output `digest`",
        ),
        (
            "      bundle-url: ${{ steps.image.outputs.bundle-url }}\n",
            "      bundle-url: ${{ steps.image.outputs.bundle-url }}\n      extra: x\n",
            "not exactly `digest`, `image-url` and `bundle-url`",
        ),
    ];
    for (from, to, want) in plants {
        red(&plant(DIGEST, from, to), want);
    }
}

#[test]
fn no_other_job_reads_the_deploy_variables() {
    // Catches: a `CP_DEPLOY_*` variable (the deploy front's address, audience or tailnet
    // client) read by a job other than the two deploy jobs, while a comment naming one is
    // not a read.
    let found = plant(
        "gamma",
        "          FARM_ADDRESS: ${{ vars.FARM_ADDRESS }}\n",
        "          FARM_ADDRESS: ${{ vars.CP_DEPLOY_ENDPOINT }}\n",
    );
    red(&found, "the job `gamma` reads a `CP_DEPLOY_*` variable");
    let commented = plant(
        "gamma",
        "    timeout-minutes: 200\n",
        "    timeout-minutes: 200\n    # not CP_DEPLOY_ENDPOINT\n",
    );
    assert!(
        !commented.iter().any(|p| p.contains("CP_DEPLOY_")),
        "{commented:#?}"
    );
}
