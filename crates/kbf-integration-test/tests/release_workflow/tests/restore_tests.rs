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
fn the_job_timeout_leaves_room_for_the_proof_and_the_restore() {
    // Catches: a deadline left to the verb's default (the timeout cannot be checked against
    // it), and a job timeout that leaves less than the room the proof and the restore need
    // after the deadline: a timeout one minute short, a deadline one minute long, and no
    // timeout at all. A longer timeout is fine.
    red(
        &plant("gamma", "--deadline-min 60 ", ""),
        "is not given `--deadline-min <minutes>`",
    );
    let short = "is not at least the deploy's deadline (60) and 30 minutes more";
    red(
        &plant(
            "gamma",
            "    timeout-minutes: 90\n",
            "    timeout-minutes: 89\n",
        ),
        short,
    );
    red(
        &plant("gamma", "    timeout-minutes: 90\n", ""),
        "(none) is not at least",
    );
    red(
        &plant("gamma", "--deadline-min 60 ", "--deadline-min 61 "),
        "is not at least the deploy's deadline (61) and 30 minutes more",
    );
    let longer = plant(
        "gamma",
        "    timeout-minutes: 90\n",
        "    timeout-minutes: 120\n",
    );
    assert!(longer.is_empty(), "{longer:#?}");
}
