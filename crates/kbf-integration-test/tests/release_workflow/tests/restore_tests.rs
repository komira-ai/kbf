//! Tests of the restore rules of `gamma` ([`super::super::restore`]): each changes the real
//! workflow in one place and names the rule it catches.

use super::*;

const RESTORE_IF_LINE: &str = "        if: ${{ failure() }}\n";
const RESTORE_RUN: &str = "        run: cargo xtask deploy-farm restore --url \"$URL\" --ca-pem \"$CA_PEM\" --version \"$RELEASE\" --run \"$RUN\"\n";

#[test]
fn a_green_run_makes_no_restore() {
    // Catches: a restore that runs after a green job (it would set a good release back) or
    // that does not run after a red one: the step's condition is exactly `failure()`. Without
    // it, under `always()` or under another condition, the lint is red; and the one
    // `failure()` allowed does not let another step run after a red one.
    let found = problems(&real()).expect("read");
    assert!(found.is_empty(), "{found:#?}");
    let steps = workflows::job_steps(&real(), GAMMA).expect("gamma");
    let last = steps.last().expect("a step");
    assert_eq!(last.condition.as_deref(), Some("${{ failure() }}"));
    assert!(lines(last).any(|l| l.starts_with("cargo xtask deploy-farm restore ")));

    let none = plant("gamma", RESTORE_IF_LINE, "");
    red(&none, "is not under exactly `if: ${{ failure() }}`");
    let always = plant("gamma", RESTORE_IF_LINE, "        if: ${{ always() }}\n");
    red(&always, "the workflow uses `always()`");
    red(&always, "a step of `gamma`");
    let other = plant(
        "gamma",
        RESTORE_IF_LINE,
        "        if: ${{ vars.KBF_COORDINATOR_URL != '' }}\n",
    );
    red(&other, "is not under exactly `if: ${{ failure() }}`");
    red(&other, "a step of `gamma`");
    let proof = "      - name: Prove the farm with a remote-only Buck2 build\n";
    let second = plant(
        "gamma",
        proof,
        &format!("{proof}        if: ${{{{ failure() }}}}\n"),
    );
    red(&second, "the workflow uses `failure()`");
}

#[test]
fn the_restore_is_the_jobs_last_step_and_runs_once() {
    // Catches: no restore (a red gamma leaves the release desired), a step after the restore
    // (it could fail with nothing set back), and a second restore.
    red(
        &plant(
            "gamma",
            "cargo xtask deploy-farm restore ",
            "cargo xtask deploy-farm other ",
        ),
        "does not run `cargo xtask deploy-farm restore`",
    );
    let after = plant(
        "gamma",
        RESTORE_RUN,
        &format!("{RESTORE_RUN}      - run: cargo xtask release notes\n"),
    );
    red(&after, "is not the last step of `gamma`");
    red(&after, "a step of `gamma`");
    let twice = plant(
        "gamma",
        RESTORE_RUN,
        &format!(
            "{RESTORE_RUN}      - name: again\n{RESTORE_IF_LINE}        env:\n          URL: ${{{{ vars.KBF_COORDINATOR_URL }}}}\n{RESTORE_RUN}"
        ),
    );
    red(
        &twice,
        "runs `cargo xtask deploy-farm restore` 2 times, not once",
    );
}

#[test]
fn the_restore_is_given_the_deploys_address_version_and_the_run() {
    // Catches: a restore told another coordinator, another release's version, or a run that
    // is not this attempt's (its write id would not tell its own write from another's), and
    // one given more of the environment than those four.
    red(
        &plant("gamma", "--run \"$RUN\"", "--run \"$URL\""),
        "`cargo xtask deploy-farm restore` is not given `--run $RUN`",
    );
    red(
        &plant("gamma", "--version \"$RELEASE\"", "--version \"$URL\""),
        "is not given `--version $RELEASE`",
    );
    red(
        &plant("gamma", "--url \"$URL\"", "--url \"$CA_PEM\""),
        "is not given `--url $URL`",
    );
    red(
        &plant("gamma", "--ca-pem \"$CA_PEM\"", "--ca-pem \"$URL\""),
        "is not given `--ca-pem $CA_PEM`",
    );
    red(
        &plant(
            "gamma",
            "          RUN: ${{ github.run_id }}-${{ github.run_attempt }}\n",
            "          RUN: ${{ github.run_id }}\n",
        ),
        "that runs `cargo xtask deploy-farm restore` is given",
    );
    red(
        &plant(
            "gamma",
            "          RELEASE: ${{ steps.farm-version.outputs.version }}\n",
            "          RELEASE: ${{ steps.farm-version.outputs.version }}\n          EXTRA: x\n",
        ),
        "that runs `cargo xtask deploy-farm restore` is given",
    );
}

#[test]
fn every_step_is_bounded_and_the_job_timeout_covers_them_all() {
    // Catches: a step that can hang until the job's timeout cuts the restore, and a budget
    // shorter than the verbs can run: a step with no timeout of its own (the proof, the
    // build), a deadline left to the verb's default, a deploy step shorter than the deploy's
    // longest run (83 minutes for a deadline of 60: its baseline read and POST each tried four
    // times with tokens, the deadline, a last read; 82 is short, as is 85 for a deadline of
    // 63, or for a pause raised to 60 seconds), a restore step shorter than the restore's (30
    // minutes: three calls each tried four times with tokens; 29 and the earlier 10 are short),
    // and a job timeout one minute below the sum of its steps'. A longer job timeout is fine.
    let proof = "      - name: Prove the farm with a remote-only Buck2 build\n        timeout-minutes: 20\n";
    red(
        &plant(
            "gamma",
            proof,
            "      - name: Prove the farm with a remote-only Buck2 build\n",
        ),
        "the step \"Prove the farm with a remote-only Buck2 build\" of `gamma` has no `timeout-minutes`",
    );
    red(
        &plant(
            "gamma",
            "      - run: cargo build -p xtask --locked\n        timeout-minutes: 30\n",
            "      - run: cargo build -p xtask --locked\n",
        ),
        "the step \"cargo build -p xtask --locked\" of `gamma` has no `timeout-minutes`",
    );
    red(
        &plant("gamma", "--deadline-min 60 ", ""),
        "is not given `--deadline-min <minutes>`",
    );
    let deploy = "      - name: Set this release as the farm's desired version, and wait for the farm to converge\n";
    let short_deploy = plant(
        "gamma",
        &format!("{deploy}        timeout-minutes: 85\n"),
        &format!("{deploy}        timeout-minutes: 82\n"),
    );
    red(
        &short_deploy,
        "the deploy step's `timeout-minutes` is less than 83",
    );
    let at_bound = plant(
        "gamma",
        &format!("{deploy}        timeout-minutes: 85\n"),
        &format!("{deploy}        timeout-minutes: 83\n"),
    );
    assert!(at_bound.is_empty(), "{at_bound:#?}");
    red(
        &plant("gamma", "--deadline-min 60 ", "--deadline-min 63 "),
        "the deploy step's `timeout-minutes` is less than 86",
    );
    red(
        &plant(
            "gamma",
            "--deadline-min 60 ",
            "--deadline-min 60 --every-secs 60 ",
        ),
        "the deploy step's `timeout-minutes` is less than",
    );
    let restore = "        timeout-minutes: 30\n        if: ${{ failure() }}\n";
    for short in ["29", "10"] {
        red(
            &plant(
                "gamma",
                restore,
                &format!("        timeout-minutes: {short}\n        if: ${{{{ failure() }}}}\n"),
            ),
            "the restore step's `timeout-minutes` is less than 30",
        );
    }
    red(
        &plant(
            "gamma",
            "    timeout-minutes: 200\n",
            "    timeout-minutes: 194\n",
        ),
        "the `timeout-minutes` of `gamma` (194) is less than the sum of its steps' (195)",
    );
    red(
        &plant("gamma", "    timeout-minutes: 200\n", ""),
        "the `timeout-minutes` of `gamma` (none)",
    );
    let longer = plant(
        "gamma",
        "    timeout-minutes: 200\n",
        "    timeout-minutes: 240\n",
    );
    assert!(longer.is_empty(), "{longer:#?}");
}

#[test]
fn the_restore_is_given_its_four_flags_and_nothing_else() {
    // Catches: a restore that keeps every flag the lint looks for and adds one that sends it
    // elsewhere or stops it: a `--service` of another service (it would set that service
    // back), a flag given twice (a usage error, so nothing is restored), and a stray word.
    let run = "--run \"$RUN\"";
    for extra in [
        "--service tierstore",
        "--run \"$RUN\"",
        "--url \"$URL\"",
        "now",
    ] {
        red(
            &plant("gamma", run, &format!("{run} {extra}")),
            "`cargo xtask deploy-farm restore` is given [",
        );
    }
}
