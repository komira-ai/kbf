//! The `test` job's coverage steps keep the gate whole: the step that runs
//! `kbf-coverage` against the floors file must not be skipped by an `if:`, and a step
//! must check on pull requests that no floor is lower than the base's. Nothing else
//! notices when a refactor of the job drops the second step or gives either a
//! condition: the gate would pass every pull request, and floors could be lowered. The
//! rules read the workflow; [`wiring`] returns how it does not meet them. The split of the
//! tests into shards, and what the gate takes from them, are the rules of [`shards`].

use super::workflows;

mod shards;

/// The workflow and job that run the gate, and the floors file they name.
const CI: &str = ".github/workflows/ci.yml";
const JOB: &str = "test";
const FLOORS: &str = "coverage-floors";
/// Where the checkout of the base goes: under the ignored `target/`, so that the repo lints
/// of a later test run do not list it as an untracked nested repository.
const BASE_DIR: &str = "target/base-floors";
/// Where the checkout of the base puts its copy of the floors file.
const BASE: &str = "target/base-floors/coverage-floors";

/// The `with:` input `key` of `step`.
fn input<'a>(step: &'a workflows::Step, key: &str) -> Option<&'a str> {
    step.with
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// The words of a `run:` script, with quotes stripped.
fn words(run: &str) -> Vec<&str> {
    run.split_whitespace()
        .map(|w| w.trim_matches('"'))
        .collect()
}

/// The word after `flag`.
fn flag<'a>(words: &[&'a str], flag: &str) -> Option<&'a str> {
    let at = words.iter().position(|w| *w == flag)?;
    words.get(at + 1).copied()
}

/// The kbf-coverage steps of `job` meet the rules; an error says how they do not.
fn wiring(workflow: &str, job: &str, floors_exist: bool) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, job)?;
    let runs = |step: &&workflows::Step| {
        step.run
            .as_deref()
            .is_some_and(|r| r.contains("-p kbf-coverage "))
    };
    let (gate, lowering): (Vec<_>, Vec<_>) = steps
        .iter()
        .filter(runs)
        .partition(|s| !s.run.as_deref().unwrap_or_default().contains("no-lowering"));
    let gate = gate.first().ok_or_else(|| {
        format!("the job `{job}` has no step running the gate (`-p kbf-coverage`)")
    })?;
    let gate_words = words(gate.run.as_deref().unwrap_or_default());
    for f in ["--lcov", "--root"] {
        if flag(&gate_words, f).is_none() {
            return Err(format!("the gate step has no `{f}`"));
        }
    }
    if flag(&gate_words, "--floors") != Some(FLOORS) {
        return Err(format!("the gate step does not pass `--floors {FLOORS}`"));
    }
    if let Some(condition) = &gate.condition {
        return Err(format!(
            "the gate step runs `if: {condition}`, so a pull request can skip it"
        ));
    }
    if !floors_exist {
        return Err(format!(
            "the file `{FLOORS}` does not exist at the repository root"
        ));
    }
    let step = lowering
        .first()
        .ok_or_else(|| format!("the job `{job}` has no step running `kbf-coverage no-lowering`"))?;
    let lw = words(step.run.as_deref().unwrap_or_default());
    if flag(&lw, "--floors") != Some(FLOORS) || flag(&lw, "--base") != Some(BASE) {
        return Err(format!(
            "the no-lowering step needs `--base {BASE}` and `--floors {FLOORS}`"
        ));
    }
    if !lw.contains(&"--allow-missing-base") {
        return Err(
            "the no-lowering step lacks `--allow-missing-base`, so the change that \
                    adds the file (a base without one) would fail"
                .to_owned(),
        );
    }
    let condition = step
        .condition
        .as_deref()
        .ok_or("the no-lowering step has no `if:`, so it runs on pushes, where there is no base")?;
    if !condition.contains("github.event_name == 'pull_request'") {
        return Err(format!(
            "the no-lowering step runs `if: {condition}`, not only on pull requests"
        ));
    }
    // A failed gate step skips later steps unless the condition says otherwise.
    if !condition.contains("!cancelled()") && !condition.contains("always()") {
        return Err(format!(
            "the no-lowering step runs `if: {condition}`, which a failed gate step skips; \
             add `!cancelled() &&`"
        ));
    }
    let at = |s: &workflows::Step| steps.iter().position(|o| std::ptr::eq(o, s));
    if at(step) <= at(gate) {
        return Err("the no-lowering step comes before the gate step".to_owned());
    }
    // The base's copy comes from a checkout of the base commit into `BASE_DIR`.
    let checkout = steps
        .iter()
        .find(|s| {
            s.uses
                .as_deref()
                .is_some_and(|u| u.starts_with("actions/checkout@"))
                && input(s, "path") == Some(BASE_DIR)
        })
        .ok_or_else(|| format!("no actions/checkout step puts the base in `path: {BASE_DIR}`"))?;
    if !input(checkout, "ref").is_some_and(|r| r.contains("github.event.pull_request.base.sha")) {
        return Err(
            "the base checkout does not take `ref: ${{ github.event.pull_request.base.sha }}`"
                .to_owned(),
        );
    }
    if !checkout
        .condition
        .as_deref()
        .is_some_and(|c| c.contains("github.event_name == 'pull_request'"))
    {
        return Err(
            "the base checkout is not conditional on pull requests, where there is a base"
                .to_owned(),
        );
    }
    // A failed gate step skips later steps unless the condition says otherwise; a skipped
    // checkout leaves no base file, which `--allow-missing-base` reads as a base without one.
    let c = checkout.condition.as_deref().unwrap_or_default();
    if !c.contains("!cancelled()") && !c.contains("always()") {
        return Err(format!(
            "the base checkout runs `if: {c}`, which a failed gate step skips, and the \
             no-lowering step would then find no base file and pass; add `!cancelled() &&`"
        ));
    }
    if at(checkout) >= at(step) {
        return Err("the base checkout comes after the no-lowering step".to_owned());
    }
    Ok(())
}

#[test]
fn the_coverage_run_is_sharded_and_merged_whole() {
    // Catches: a part of a core left out or run twice, a core left out, a shard whose profiles
    // could overwrite another's or go unchecked, a gate that does not wait for every shard
    // (so tests would count as never run), and a required check renamed or skippable.
    let text = std::fs::read_to_string(super::repo_root().join(CI)).expect(CI);
    assert_eq!(shards::shard_wiring(&text), Ok(()), "{CI}");
    assert_eq!(shards::gate_wiring(&text), Ok(()), "{CI}");
    assert_eq!(shards::check_names(&text), Ok(()), "{CI}");
}

#[test]
fn the_test_job_gates_coverage_and_floors() {
    // Catches: the gate step skipped by a condition, or the no-lowering step dropped,
    // renamed, run on pushes, or skipped after a failed gate.
    let text = std::fs::read_to_string(super::repo_root().join(CI)).expect(CI);
    let floors_exist = super::repo_root().join(FLOORS).is_file();
    assert_eq!(wiring(&text, JOB, floors_exist), Ok(()), "{CI}");
}

/// The rules [`wiring`] reads the workflow by, on planted workflows.
mod rules {
    use super::*;

    const GATE: &str = "      - run: cargo run -p kbf-coverage --locked -- --lcov l --root r --floors coverage-floors\n";
    const CHECKOUT: &str = "      - uses: actions/checkout@abc # v7\n        if: ${{ !cancelled() && github.event_name == 'pull_request' }}\n        with:\n          ref: ${{ github.event.pull_request.base.sha }}\n          path: target/base-floors\n";
    const LOWERING: &str = "      - if: ${{ !cancelled() && github.event_name == 'pull_request' }}\n        run: cargo run -p kbf-coverage --locked -- no-lowering --allow-missing-base --base target/base-floors/coverage-floors --floors coverage-floors\n";
    const PR: &str = "github.event_name == 'pull_request'";

    fn workflow(steps: &str) -> String {
        format!(
            "on: push\npermissions: {{}}\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n{steps}"
        )
    }

    fn check(steps: &str) -> Result<(), String> {
        wiring(&workflow(steps), "test", true)
    }

    /// An error with `${{ }}` taken off conditions, so the messages read as written.
    fn plain(e: &str) -> String {
        e.replace("`if: ${{ ", "`if: ").replace(" }}`", "`")
    }

    #[test]
    fn the_shape_ci_uses_passes() {
        assert_eq!(check(&format!("{GATE}{CHECKOUT}{LOWERING}")), Ok(()));
        let always = LOWERING.replace("!cancelled()", "always()");
        assert_eq!(check(&format!("{GATE}{CHECKOUT}{always}")), Ok(()));
    }

    #[test]
    fn each_drift_is_refused() {
        // Catches: each way the gate or the no-lowering check stops binding.
        let with = |lowering: String| format!("{GATE}{CHECKOUT}{lowering}");
        let cases = [
            (
                String::new(),
                "the job `test` has no step running the gate (`-p kbf-coverage`)",
            ),
            (
                GATE.replace("--floors coverage-floors", "--floors other"),
                "the gate step does not pass `--floors coverage-floors`",
            ),
            (
                GATE.replace("--lcov l ", ""),
                "the gate step has no `--lcov`",
            ),
            (
                GATE.replace("--root r ", ""),
                "the gate step has no `--root`",
            ),
            (
                GATE.replace(
                    "      - run",
                    "      - if: ${{ github.event_name == 'push' }}\n        run",
                ),
                "the gate step runs `if: github.event_name == 'push'`, so a pull request can skip it",
            ),
            (
                GATE.to_owned(),
                "the job `test` has no step running `kbf-coverage no-lowering`",
            ),
            (
                with(LOWERING.replace("--base target/base-floors/coverage-floors", "--base b")),
                "the no-lowering step needs `--base target/base-floors/coverage-floors` and `--floors coverage-floors`",
            ),
            (
                with(LOWERING.replace("--floors coverage-floors", "--floors x")),
                "the no-lowering step needs `--base target/base-floors/coverage-floors` and `--floors coverage-floors`",
            ),
            (
                with(LOWERING.replace("--allow-missing-base ", "")),
                "the no-lowering step lacks `--allow-missing-base`, so the change that adds the file (a base without one) would fail",
            ),
            (
                with(LOWERING.replace(
                    &format!("      - if: ${{{{ !cancelled() && {PR} }}}}\n        run"),
                    "      - run",
                )),
                "the no-lowering step has no `if:`, so it runs on pushes, where there is no base",
            ),
            (
                with(LOWERING.replace("!cancelled() && ", "")),
                "the no-lowering step runs `if: github.event_name == 'pull_request'`, which a failed gate step skips; add `!cancelled() &&`",
            ),
            (
                with(LOWERING.replace(PR, "true")),
                "the no-lowering step runs `if: !cancelled() && true`, not only on pull requests",
            ),
            (
                format!("{LOWERING}{GATE}"),
                "the no-lowering step comes before the gate step",
            ),
            (
                format!("{GATE}{LOWERING}"),
                "no actions/checkout step puts the base in `path: target/base-floors`",
            ),
            (
                format!(
                    "{GATE}{}{LOWERING}",
                    CHECKOUT.replace("path: target/base-floors", "path: other")
                ),
                "no actions/checkout step puts the base in `path: target/base-floors`",
            ),
            (
                format!(
                    "{GATE}{}{LOWERING}",
                    CHECKOUT.replace("github.event.pull_request.base.sha", "github.sha")
                ),
                "the base checkout does not take `ref: ${{ github.event.pull_request.base.sha }}`",
            ),
            (
                format!(
                    "{GATE}{}{LOWERING}",
                    CHECKOUT.replace(
                        &format!("        if: ${{{{ !cancelled() && {PR} }}}}\n"),
                        ""
                    )
                ),
                "the base checkout is not conditional on pull requests, where there is a base",
            ),
            (
                format!(
                    "{GATE}{}{LOWERING}",
                    CHECKOUT.replace("!cancelled() && ", "")
                ),
                "the base checkout runs `if: github.event_name == 'pull_request'`, which a failed gate step skips, and the no-lowering step would then find no base file and pass; add `!cancelled() &&`",
            ),
            (
                format!("{GATE}{}{LOWERING}", CHECKOUT.replace(PR, "true")),
                "the base checkout is not conditional on pull requests, where there is a base",
            ),
            (
                format!("{GATE}{LOWERING}{CHECKOUT}"),
                "the base checkout comes after the no-lowering step",
            ),
        ];
        for (steps, want) in cases {
            assert_eq!(
                check(&steps).map_err(|e| plain(&e)),
                Err(plain(want)),
                "{steps}"
            );
        }
        assert_eq!(
            wiring(
                &workflow(&format!("{GATE}{CHECKOUT}{LOWERING}")),
                "test",
                false
            ),
            Err(format!(
                "the file `{FLOORS}` does not exist at the repository root"
            ))
        );
    }
}
