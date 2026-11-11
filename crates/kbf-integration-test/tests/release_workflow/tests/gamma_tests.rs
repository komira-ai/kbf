//! Tests of the gamma rules ([`super::super::gamma`]): each changes the real workflow in one place
//! and names the rule it catches.

use super::*;

const SWITCH: &str = "        if: ${{ vars.KBF_SETS_DESIRED == 'true' }}\n";
const NOTICE_RUN: &str = "run: echo \"::notice title=gamma::";
const RUSTUP: &str =
    "        timeout-minutes: 10\n        if: ${{ vars.KBF_SETS_DESIRED == 'true' }}\n";
const BUILD: &str =
    "        timeout-minutes: 30\n        if: ${{ vars.KBF_SETS_DESIRED == 'true' }}\n";
const FETCH: &str = "        timeout-minutes: 5\n        if: ${{ vars.KBF_SETS_DESIRED == 'true' }}\n        run: cargo xtask fetch";
const PROVE: &str =
    "        timeout-minutes: 20\n        if: ${{ vars.KBF_SETS_DESIRED == 'true' }}\n";
const JOIN_LINE: &str = "        if: ${{ vars.KBF_SETS_DESIRED == 'true' && vars.TS_CLIENT_ID != '' && vars.FARM_ADDRESS != '' }}\n";
const PROOF_NAME: &str = "      - name: Prove the farm with a remote-only Buck2 build\n";
const JOIN_NAME: &str = "      - name: Join the farm's Tailscale network\n";

#[test]
fn gamma_no_longer_sets_the_update_coordinator() {
    // Catches: the deploy step, its version step or its restore coming back into `gamma` (the job
    // would wait on an update coordinator whose role table is empty, since hostkeeper deploys the
    // farm's nodes), and any job reading a variable of the retired deploy.
    let found = problems(&real()).expect("read");
    assert!(found.is_empty(), "{found:#?}");
    let deploy = format!(
        "{SWITCH}        run: cargo xtask deploy-farm --url \"$URL\" --version \"$VERSION\"\n"
    );
    red(
        &plant(
            "gamma",
            "          NONCE: ${{ github.run_id }}-${{ github.run_attempt }}\n",
            &format!(
                "          NONCE: ${{{{ github.run_id }}}}-${{{{ github.run_attempt }}}}\n      - name: deploy\n        timeout-minutes: 5\n{deploy}"
            ),
        ),
        "runs or names `deploy-farm`",
    );
    red(
        &plant(
            "gamma",
            "          NONCE: ${{ github.run_id }}-${{ github.run_attempt }}\n",
            "          NONCE: ${{ github.run_id }}-${{ github.run_attempt }}\n      - name: version\n        timeout-minutes: 5\n        if: ${{ vars.KBF_SETS_DESIRED == 'true' }}\n        run: cargo xtask release farm-version --tag \"$TAG\"\n",
        ),
        "runs or names `farm-version`",
    );
    for variable in [
        "KBF_COORDINATOR_URL",
        "KBF_COORDINATOR_CA",
        "KBF_MIN_UP_STORAGE",
        "KBF_REQUIRE_COORDINATOR_CURRENT",
    ] {
        red(
            &plant(
                "gamma",
                "          NONCE: ${{ github.run_id }}-${{ github.run_attempt }}\n",
                &format!(
                    "          NONCE: ${{{{ github.run_id }}}}-${{{{ github.run_attempt }}}}\n          URL: ${{{{ vars.{variable} }}}}\n"
                ),
            ),
            "the workflow reads `KBF_",
        );
        red(
            &plant(
                "bump",
                "          VERSION: ${{ needs.plan.outputs.version }}\n",
                &format!(
                    "          VERSION: ${{{{ needs.plan.outputs.version }}}}\n          URL: ${{{{ vars.{variable} }}}}\n"
                ),
            ),
            "the workflow reads `KBF_",
        );
    }
}

#[test]
fn the_notice_is_the_first_step_after_the_checkout() {
    // Catches: the notice missing (a job that ends green with nothing said when the switch is
    // off), under a condition, written with an expression, or no longer the step right after the
    // checkout.
    red(
        &plant("gamma", NOTICE_RUN, "run: echo \"hostkeeper::"),
        "is not an unconditional one-line",
    );
    red(
        &plant("gamma", NOTICE_RUN, "run: true # \"::notice title=gamma::"),
        "is not an unconditional one-line",
    );
    red(
        &plant(
            "gamma",
            "        timeout-minutes: 1\n        run: echo",
            &format!("        timeout-minutes: 1\n{SWITCH}        run: echo"),
        ),
        "is not an unconditional one-line",
    );
    red(
        &plant(
            "gamma",
            "KBF_SETS_DESIRED is true.",
            "KBF_SETS_DESIRED is ${{ vars.KBF_SETS_DESIRED }}.",
        ),
        "is not an unconditional one-line",
    );
    red(
        &plant("gamma", "kbf's update coordinator, and", "kbf's $HOME, and"),
        "is not an unconditional one-line",
    );
    let step = "      - name: The farm's Linux nodes are deployed by hostkeeper\n";
    red(
        &plant(
            "gamma",
            step,
            &format!(
                "      - run: true\n        timeout-minutes: 1\n        if: ${{{{ vars.KBF_SETS_DESIRED == 'true' }}}}\n{step}"
            ),
        ),
        "is not an unconditional one-line",
    );
}

#[test]
fn every_later_step_of_gamma_is_under_exactly_the_switch() {
    // Catches: a step that runs when `KBF_SETS_DESIRED` is not `true` (the build, the join, the
    // fetch of Buck2 or the proof), whether it has no condition or another one (the halting rule
    // names it too), and the join under the switch alone, without the variables it needs.
    for (anchor, name) in [
        (RUSTUP, "rustup"),
        (BUILD, "cargo build"),
        (FETCH, "Fetch buck2"),
        (PROVE, "Prove the farm"),
    ] {
        let without = anchor.replace(SWITCH, "");
        let found = plant("gamma", anchor, &without);
        red(&found, "after the notice is not under exactly");
        red(&found, name);
        let other = anchor.replace(SWITCH, "        if: ${{ vars.OTHER == 'true' }}\n");
        let found = plant("gamma", anchor, &other);
        red(&found, "after the notice is not under exactly");
        red(&found, "a step of `gamma`");
    }
    red(
        &plant("gamma", JOIN_LINE, ""),
        "is not under exactly `if: ${{ vars.KBF_SETS_DESIRED == 'true' && vars.TS_CLIENT_ID != '' && vars.FARM_ADDRESS != '' }}`",
    );
    red(
        &plant("gamma", JOIN_LINE, SWITCH),
        "is not under exactly `if: ${{ vars.KBF_SETS_DESIRED == 'true' && vars.TS_CLIENT_ID",
    );
    red(
        &plant(
            "gamma",
            JOIN_LINE,
            "        if: ${{ vars.TS_CLIENT_ID != '' && vars.FARM_ADDRESS != '' }}\n",
        ),
        "is not under exactly",
    );
}

#[test]
fn the_network_join_of_gamma_uses_the_run_token_and_variables_only() {
    // Catches: a secret given to the join (the job would hold more than the run token), an input
    // added or changed, an `env:` on it, the join moved before the build (the build's scripts
    // would see the network) or after the proof (which would run without it), a second join, and
    // the action used by another job.
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
            JOIN_LINE,
            &format!("{JOIN_LINE}        env:\n          TS_AUTHKEY: ${{{{ vars.TS_KEY }}}}\n"),
        ),
        "has an `env:`",
    );
    let real = real();
    let start = real.find(JOIN_NAME).expect("the join step");
    let end = start
        + real[start..]
            .find("      # The proof. Build a small project")
            .expect("the comment of the proof");
    let block = &real[start..end];
    let without = [&real[..start], &real[end..]].concat();
    let build = "      - run: cargo build -p xtask --locked\n";
    let moved = in_job(&without, "gamma", build, &format!("{block}{build}"));
    red(
        &problems(&moved).expect("the changed workflow is read"),
        "not after `cargo build -p xtask`",
    );
    let nonce = "--nonce \"$NONCE\"\n";
    let late = in_job(&without, "gamma", nonce, &format!("{nonce}{block}"));
    red(
        &problems(&late).expect("the changed workflow is read"),
        "before `cargo xtask prove-farm`",
    );
    let twice = real.replacen(block, &format!("{block}{block}"), 1);
    red(
        &problems(&twice).expect("the changed workflow is read"),
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
fn the_proof_runs_once_with_the_pinned_buck2_fetched_before_it() {
    // Catches: the proof missing (a release with no proof of the farm), run twice, run without
    // the pinned Buck2 fetched before it, or a Buck2 fetched with other pins.
    red(
        &plant("gamma", "cargo xtask prove-farm ", "cargo xtask other "),
        "does not run `cargo xtask prove-farm`",
    );
    let real = real();
    let verb = real.find(PROOF_NAME).expect("proof step");
    let end = verb
        + real[verb..]
            .find("\n\n")
            .expect("the blank line after gamma");
    let twice = [&real[..end], "\n", &real[verb..end], &real[end..]].concat();
    red(
        &problems(&twice).expect("the changed workflow is read"),
        "times, not once",
    );
    red(
        &plant("gamma", "--sha256 \"$BUCK2_SHA256\"", "--sha256 \"$OTHER\""),
        "does not fetch Buck2",
    );
    let fetch = "      - name: Fetch buck2 (pinned version, checked by SHA-256)\n";
    let start = real.find(fetch).expect("fetch");
    let stop = real.find(PROOF_NAME).expect("proof");
    let block = &real[start..stop];
    let swapped = [
        &real[..start],
        &real[stop..end],
        "\n",
        block.trim_end(),
        &real[end..],
    ]
    .concat();
    red(
        &problems(&swapped).expect("the changed workflow is read"),
        "does not fetch Buck2",
    );
}

#[test]
fn every_step_of_gamma_has_a_timeout_and_the_job_covers_them() {
    // Catches: a step that would run until the job's timeout cuts it, and a job timeout below the
    // sum of its steps' (a step cut by the job instead of by its own timeout).
    red(
        &plant(
            "gamma",
            "      - run: cargo build -p xtask --locked\n        timeout-minutes: 30\n",
            "      - run: cargo build -p xtask --locked\n",
        ),
        "has no `timeout-minutes` of its own",
    );
    red(
        &plant(
            "gamma",
            "    timeout-minutes: 90\n",
            "    timeout-minutes: 30\n",
        ),
        "is less than the sum of its steps'",
    );
}
