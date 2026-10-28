//! The `test` job's coverage steps keep the gate whole: the step that runs
//! `kbf-coverage` against the floors file must not be skipped by an `if:`, and a step
//! must check on pull requests that no floor is lower than the base's. Nothing else
//! notices when a refactor of the job drops the second step or gives either a
//! condition: the gate would pass every pull request, and floors could be lowered. The
//! rules read the workflow; [`wiring`] returns how it does not meet them.
//!
//! The fast tests run in shards (the `test-shard` job, a matrix), and the `test` job
//! merges the shards' tracefiles and gates the merge. [`merge_wiring`] holds that part:
//! the gate must read the file the merge wrote, and the job must fail, not be skipped,
//! when a shard fails: GitHub counts a job skipped for a failed `needs` as passing a
//! required check, so without it a red shard would leave the required `test` check green.

use super::workflows;

/// The workflow and job that run the gate, and the floors file they name.
const CI: &str = ".github/workflows/ci.yml";
const JOB: &str = "test";
const FLOORS: &str = "coverage-floors";
/// Where the checkout of the base puts its copy of the floors file.
const BASE: &str = "base/coverage-floors";

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

/// Whether a `run:` script is the `kbf-coverage merge` verb.
fn is_merge(run: &str) -> bool {
    run.contains("-p kbf-coverage ") && words(run).contains(&"merge")
}

/// The kbf-coverage steps of `job` meet the rules; an error says how they do not.
fn wiring(workflow: &str, job: &str, floors_exist: bool) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, job)?;
    let runs = |step: &&workflows::Step| {
        step.run
            .as_deref()
            .is_some_and(|r| r.contains("-p kbf-coverage ") && !is_merge(r))
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
    // The base's copy comes from a checkout of the base commit into `base`.
    let checkout = steps
        .iter()
        .find(|s| {
            s.uses
                .as_deref()
                .is_some_and(|u| u.starts_with("actions/checkout@"))
                && input(s, "path") == Some("base")
        })
        .ok_or("no actions/checkout step puts the base in `path: base`")?;
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

/// The `job` merges the shards' tracefiles into the file its gate reads, and fails when a
/// shard did; an error says how it does not.
fn merge_wiring(workflow: &str, job: &str) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, job)?;
    let at = |pred: &dyn Fn(&workflows::Step) -> bool| steps.iter().position(pred);
    let merge = at(&|s| s.run.as_deref().is_some_and(is_merge))
        .ok_or_else(|| format!("the job `{job}` has no step running `kbf-coverage merge`"))?;
    let gate = at(&|s| {
        s.run.as_deref().is_some_and(|r| {
            r.contains("-p kbf-coverage ") && !is_merge(r) && !r.contains("no-lowering")
        })
    })
    .ok_or_else(|| format!("the job `{job}` has no step running the gate"))?;
    if merge >= gate {
        return Err("the merge step comes after the gate step".to_owned());
    }
    let merge_step = &steps[merge];
    if let Some(condition) = &merge_step.condition {
        return Err(format!(
            "the merge step runs `if: {condition}`, so the gate could read a stale file"
        ));
    }
    let merge_words = words(merge_step.run.as_deref().unwrap_or_default());
    let gate_words = words(steps[gate].run.as_deref().unwrap_or_default());
    let out = flag(&merge_words, "--out");
    if out.is_none() || out != flag(&gate_words, "--lcov") {
        return Err(
            "the gate step does not read the file the merge step writes (`--out`)".to_owned(),
        );
    }
    for f in ["--dir", "--expect"] {
        if flag(&merge_words, f).is_none() {
            return Err(format!("the merge step has no `{f}`"));
        }
    }
    if !workflows::job_has_key(workflow, job, "if")? {
        return Err(format!(
            "the job `{job}` has no `if:`, so it is skipped, not failed, when a shard fails, and a skipped required check passes"
        ));
    }
    if workflows::job_needs(workflow, job)?.is_empty() {
        return Err(format!("the job `{job}` needs no shard job"));
    }
    let checks = steps[..merge].iter().any(|s| {
        s.condition.is_none()
            && s.env
                .iter()
                .any(|(_, v)| v.contains("needs.") && v.contains(".result"))
    });
    if !checks {
        return Err("no step before the merge reads a shard job's `needs.<job>.result`".to_owned());
    }
    Ok(())
}

#[test]
fn the_test_job_merges_the_shards_and_fails_with_them() {
    // Catches: the merge dropped, run after the gate, or writing a file the gate does not
    // read, and the job skipped (a passing required check) instead of failed when a shard
    // fails.
    let text = std::fs::read_to_string(super::repo_root().join(CI)).expect(CI);
    assert_eq!(merge_wiring(&text, JOB), Ok(()), "{CI}");
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
    const CHECKOUT: &str = "      - uses: actions/checkout@abc # v7\n        if: ${{ !cancelled() && github.event_name == 'pull_request' }}\n        with:\n          ref: ${{ github.event.pull_request.base.sha }}\n          path: base\n";
    const LOWERING: &str = "      - if: ${{ !cancelled() && github.event_name == 'pull_request' }}\n        run: cargo run -p kbf-coverage --locked -- no-lowering --allow-missing-base --base base/coverage-floors --floors coverage-floors\n";
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
                with(LOWERING.replace("--base base/coverage-floors", "--base b")),
                "the no-lowering step needs `--base base/coverage-floors` and `--floors coverage-floors`",
            ),
            (
                with(LOWERING.replace("--floors coverage-floors", "--floors x")),
                "the no-lowering step needs `--base base/coverage-floors` and `--floors coverage-floors`",
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
                "no actions/checkout step puts the base in `path: base`",
            ),
            (
                format!(
                    "{GATE}{}{LOWERING}",
                    CHECKOUT.replace("path: base", "path: other")
                ),
                "no actions/checkout step puts the base in `path: base`",
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

/// The rules [`merge_wiring`] reads the workflow by, on planted workflows.
mod merge_rules {
    use super::*;

    const RESULT: &str = "      - env:\n          SHARDS: ${{ needs.test-shard.result }}\n        run: test \"$SHARDS\" = success\n";
    const MERGE: &str = "      - run: cargo run -p kbf-coverage --locked -- merge --dir d --expect 3 --out m.info\n";
    const GATE: &str = "      - run: cargo run -p kbf-coverage --locked -- --lcov m.info --root r --floors coverage-floors\n";

    fn workflow(job_head: &str, steps: &str) -> String {
        format!(
            "on: push\npermissions: {{}}\njobs:\n  test-shard:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n  test:\n    runs-on: ubuntu-latest\n{job_head}    steps:\n{steps}"
        )
    }

    const HEAD: &str = "    needs: test-shard\n    if: ${{ !cancelled() }}\n";

    fn check(job_head: &str, steps: &str) -> Result<(), String> {
        merge_wiring(&workflow(job_head, steps), "test")
    }

    #[test]
    fn the_shape_ci_uses_passes() {
        assert_eq!(check(HEAD, &format!("{RESULT}{MERGE}{GATE}")), Ok(()));
    }

    #[test]
    fn each_drift_is_refused() {
        // Catches: each way the merge or the shard failure stops binding.
        let all = format!("{RESULT}{MERGE}{GATE}");
        let cases = [
            (
                HEAD,
                format!("{RESULT}{GATE}"),
                "the job `test` has no step running `kbf-coverage merge`",
            ),
            (
                HEAD,
                format!("{RESULT}{MERGE}"),
                "the job `test` has no step running the gate",
            ),
            (
                HEAD,
                format!("{RESULT}{GATE}{MERGE}"),
                "the merge step comes after the gate step",
            ),
            (
                HEAD,
                all.replace(
                    "      - run: cargo run -p kbf-coverage --locked -- merge",
                    "      - if: ${{ github.event_name == 'push' }}\n        run: cargo run -p kbf-coverage --locked -- merge",
                ),
                "the merge step runs `if: ${{ github.event_name == 'push' }}`, so the gate could read a stale file",
            ),
            (
                HEAD,
                all.replace("--out m.info", "--out other.info"),
                "the gate step does not read the file the merge step writes (`--out`)",
            ),
            (
                HEAD,
                all.replace(" --out m.info", ""),
                "the gate step does not read the file the merge step writes (`--out`)",
            ),
            (HEAD, all.replace("--dir d ", ""), "the merge step has no `--dir`"),
            (
                HEAD,
                all.replace("--expect 3 ", ""),
                "the merge step has no `--expect`",
            ),
            (
                "    needs: test-shard\n",
                all.clone(),
                "the job `test` has no `if:`, so it is skipped, not failed, when a shard fails, and a skipped required check passes",
            ),
            (
                "    if: ${{ !cancelled() }}\n",
                all.clone(),
                "the job `test` needs no shard job",
            ),
            (
                HEAD,
                all.replace("needs.test-shard.result", "github.run_id"),
                "no step before the merge reads a shard job's `needs.<job>.result`",
            ),
            (
                HEAD,
                all.replace(
                    "      - env:",
                    "      - if: ${{ always() }}\n        env:",
                ),
                "no step before the merge reads a shard job's `needs.<job>.result`",
            ),
        ];
        for (head, steps, want) in cases {
            assert_eq!(check(head, &steps), Err(want.to_owned()), "{head}{steps}");
        }
    }

    #[test]
    fn the_merge_step_is_not_taken_for_the_gate() {
        // Catches: `wiring` taking the merge step, which has no `--floors`, for the
        // gate step, so that the real gate went unchecked.
        let steps = format!("{MERGE}{GATE}");
        assert_eq!(
            wiring(&workflow(HEAD, &steps), "test", true).map_err(|e| e.contains("no-lowering")),
            Err(true)
        );
    }
}
