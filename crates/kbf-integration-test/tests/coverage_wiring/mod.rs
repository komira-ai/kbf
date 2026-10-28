//! The `test` job's coverage steps keep the gate whole: the step that runs
//! `kbf-coverage` against the floors file must not be skipped by an `if:`, and a step
//! must check on pull requests that no floor is lower than the base's. Nothing else
//! notices when a refactor of the job drops the second step or gives either a
//! condition: the gate would pass every pull request, and floors could be lowered. The
//! rules read the workflow; [`wiring`] returns how it does not meet them.
//!
//! The fast tests run in parts (the `test-shard` job, a matrix) and the `test` job gates what
//! the last part reports. [`split_wiring`] holds the rules of that split: the parts' raw
//! profiles must be put together before the one report is made (the parts' lcov reports
//! cannot be added: a line is counted once for each function it is in), the parts' profile
//! files must have names of their own, and `test` must fail, not be skipped, when a part
//! fails: GitHub counts a job skipped for a failed `needs` as passing a required check, so
//! without it a red part would leave the required `test` check green.

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

const SHARD: &str = "test-shard";
/// The directory `llvm-cov report` merges every `*.profraw` of.
const PROFILES: &str = "target/llvm-cov-target";

/// The `run:` of `step`, or "".
fn run_of(step: &workflows::Step) -> &str {
    step.run.as_deref().unwrap_or_default()
}

/// The split coverage run of `workflow` meets the rules; an error says how it does not.
fn split_wiring(workflow: &str) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, SHARD)?;
    let env = workflows::job_env(workflow, SHARD)?;
    let profile = env
        .iter()
        .find(|(k, _)| k == "LLVM_PROFILE_FILE_NAME")
        .map(|(_, v)| v.as_str())
        .ok_or("the job `test-shard` does not set LLVM_PROFILE_FILE_NAME")?;
    if !profile.contains("matrix.shard") {
        return Err(
            "LLVM_PROFILE_FILE_NAME does not name the part (`matrix.shard`), so the \
                    parts' files could overwrite one another"
                .to_owned(),
        );
    }
    let at = |pred: &dyn Fn(&workflows::Step) -> bool| steps.iter().position(pred);
    let uses = |s: &workflows::Step, name: &str| {
        s.uses
            .as_deref()
            .is_some_and(|u| u.starts_with(&format!("{name}@")))
    };
    let upload = at(&|s| {
        uses(s, "actions/upload-artifact")
            && input(s, "name").is_some_and(|n| n.starts_with("profraw-"))
    })
    .ok_or("no step uploads a part's raw profiles (`name: profraw-...`)")?;
    let wait = at(&|s| run_of(s).contains("xtask wait-artifacts"))
        .ok_or("no step runs `cargo xtask wait-artifacts`")?;
    let download = at(&|s| uses(s, "actions/download-artifact"))
        .ok_or("no step downloads the other parts' raw profiles")?;
    let report =
        at(&|s| run_of(s).contains("llvm-cov report")).ok_or("no step runs `llvm-cov report`")?;
    if !(upload < wait && wait < download && download < report) {
        return Err("the steps are not in the order upload, wait, download, report".to_owned());
    }
    let last = steps[wait].condition.as_deref().unwrap_or_default();
    if !last.contains("env.SHARDS") || !last.contains("==") {
        return Err(format!(
            "the wait step runs `if: {last}`, which does not pick the last part (`== env.SHARDS`)"
        ));
    }
    for (what, i) in [("download", download), ("report", report)] {
        if steps[i].condition.as_deref() != Some(last) {
            return Err(format!(
                "the {what} step has another `if:` than the wait step, so a part that did not wait would report"
            ));
        }
    }
    let words = words(run_of(&steps[wait]));
    if flag(&words, "--of") != Some("$SHARDS") {
        return Err("the wait step does not pass `--of \"$SHARDS\"`".to_owned());
    }
    let prefix = flag(&words, "--artifact-prefix").unwrap_or_default();
    let pattern = input(&steps[download], "pattern").unwrap_or_default();
    if prefix.is_empty() || !pattern.starts_with(prefix) {
        return Err(
            "the download step's `pattern` does not match what the wait step waits for".to_owned(),
        );
    }
    if input(&steps[upload], "name").is_some_and(|n| !n.starts_with(prefix)) {
        return Err(
            "the upload step's `name` does not begin with the wait step's `--artifact-prefix`"
                .to_owned(),
        );
    }
    if input(&steps[upload], "overwrite") != Some("true") {
        return Err("the upload step lacks `overwrite: true`, so a re-run of the run fails on the names its first run uploaded".to_owned());
    }
    if input(&steps[download], "path") != Some(PROFILES)
        || input(&steps[download], "merge-multiple") != Some("true")
    {
        return Err(format!(
            "the download step must put the profiles flat in `{PROFILES}` (`merge-multiple: true`)"
        ));
    }
    Ok(())
}

/// The `test` job of `workflow` fails, rather than is skipped, when a part fails.
fn required_check_wiring(workflow: &str, job: &str) -> Result<(), String> {
    if !workflows::job_needs(workflow, job)?
        .iter()
        .any(|n| n == SHARD)
    {
        return Err(format!("the job `{job}` does not need `{SHARD}`"));
    }
    if !workflows::job_has_key(workflow, job, "if")? {
        return Err(format!(
            "the job `{job}` has no `if:`, so it is skipped, not failed, when a part fails, and a skipped required check passes"
        ));
    }
    let steps = workflows::job_steps(workflow, job)?;
    let gate = steps
        .iter()
        .position(|s| run_of(s).contains("-p kbf-coverage "))
        .ok_or_else(|| format!("the job `{job}` has no step running `kbf-coverage`"))?;
    let checks = steps[..gate].iter().any(|s| {
        s.condition.is_none()
            && s.env
                .iter()
                .any(|(_, v)| v.contains("needs.") && v.contains(".result"))
    });
    if !checks {
        return Err("no step before the gate reads a part's `needs.<job>.result`".to_owned());
    }
    Ok(())
}

#[test]
fn the_coverage_run_is_split_and_put_together_whole() {
    // Catches: the parts' profiles not put together before the report, a part that
    // reports without waiting, profile files that could overwrite one another, and the
    // required check skipped, not failed, when a part fails.
    let text = std::fs::read_to_string(super::repo_root().join(CI)).expect(CI);
    assert_eq!(split_wiring(&text), Ok(()), "{CI}");
    assert_eq!(required_check_wiring(&text, JOB), Ok(()), "{CI}");
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

/// The rules [`split_wiring`] and [`required_check_wiring`] read the workflow by.
mod split_rules {
    use super::*;

    const LAST: &str = "${{ format('{0}', matrix.shard) == env.SHARDS }}";
    const HEAD: &str = "  test-shard:\n    runs-on: ubuntu-latest\n    env:\n      LLVM_PROFILE_FILE_NAME: kbf-part${{ matrix.shard }}-%p-%4m.profraw\n    steps:\n";
    const UPLOAD: &str = "      - uses: actions/upload-artifact@abc # v7\n        with:\n          name: profraw-part-${{ matrix.shard }}\n          overwrite: true\n";
    const WAIT: &str = "      - if: ${{ format('{0}', matrix.shard) == env.SHARDS }}\n        run: cargo xtask wait-artifacts --artifact-prefix profraw-part- --of \"$SHARDS\"\n";
    const DOWNLOAD: &str = "      - if: ${{ format('{0}', matrix.shard) == env.SHARDS }}\n        uses: actions/download-artifact@abc # v8\n        with:\n          pattern: profraw-part-*\n          path: target/llvm-cov-target\n          merge-multiple: true\n";
    const REPORT: &str = "      - if: ${{ format('{0}', matrix.shard) == env.SHARDS }}\n        run: cargo llvm-cov report --lcov\n";
    const TEST: &str = "  test:\n    needs: test-shard\n    if: ${{ !cancelled() }}\n    runs-on: ubuntu-latest\n    steps:\n      - env:\n          R: ${{ needs.test-shard.result }}\n        run: test \"$R\" = success\n      - run: cargo run -p kbf-coverage --locked -- --lcov l\n";

    fn workflow(steps: &str, head: &str, test: &str) -> String {
        format!("on: push\npermissions: {{}}\njobs:\n{head}{steps}{test}")
    }

    fn good() -> String {
        format!("{UPLOAD}{WAIT}{DOWNLOAD}{REPORT}")
    }

    #[test]
    fn the_shape_ci_uses_passes() {
        let w = workflow(&good(), HEAD, TEST);
        assert_eq!(split_wiring(&w), Ok(()));
        assert_eq!(required_check_wiring(&w, "test"), Ok(()));
    }

    #[test]
    fn each_drift_of_the_split_is_refused() {
        // Catches: each way the parts' profiles stop being put together whole.
        let all = good();
        let cases = [
            (
                HEAD.replace(
                    "      LLVM_PROFILE_FILE_NAME: kbf-part${{ matrix.shard }}-%p-%4m.profraw\n",
                    "      X: 1\n",
                ),
                all.clone(),
                "the job `test-shard` does not set LLVM_PROFILE_FILE_NAME",
            ),
            (
                HEAD.replace("part${{ matrix.shard }}", "part"),
                all.clone(),
                "LLVM_PROFILE_FILE_NAME does not name the part (`matrix.shard`), so the parts' files could overwrite one another",
            ),
            (
                HEAD.to_owned(),
                format!("{WAIT}{DOWNLOAD}{REPORT}"),
                "no step uploads a part's raw profiles (`name: profraw-...`)",
            ),
            (
                HEAD.to_owned(),
                format!("{UPLOAD}{DOWNLOAD}{REPORT}"),
                "no step runs `cargo xtask wait-artifacts`",
            ),
            (
                HEAD.to_owned(),
                format!("{UPLOAD}{WAIT}{REPORT}"),
                "no step downloads the other parts' raw profiles",
            ),
            (
                HEAD.to_owned(),
                format!("{UPLOAD}{WAIT}{DOWNLOAD}"),
                "no step runs `llvm-cov report`",
            ),
            (
                HEAD.to_owned(),
                format!("{UPLOAD}{DOWNLOAD}{WAIT}{REPORT}"),
                "the steps are not in the order upload, wait, download, report",
            ),
            (
                HEAD.to_owned(),
                format!("{UPLOAD}{REPORT}{WAIT}{DOWNLOAD}"),
                "the steps are not in the order upload, wait, download, report",
            ),
            (
                HEAD.to_owned(),
                all.replace(
                    &format!("      - if: {LAST}\n        run: cargo xtask"),
                    "      - run: cargo xtask",
                ),
                "the wait step runs `if: `, which does not pick the last part (`== env.SHARDS`)",
            ),
            (
                HEAD.to_owned(),
                all.replacen(
                    &format!("      - if: {LAST}\n        uses: actions/download"),
                    "      - uses: actions/download",
                    1,
                ),
                "the download step has another `if:` than the wait step, so a part that did not wait would report",
            ),
            (
                HEAD.to_owned(),
                all.replace(
                    &format!("      - if: {LAST}\n        run: cargo llvm-cov"),
                    "      - run: cargo llvm-cov",
                ),
                "the report step has another `if:` than the wait step, so a part that did not wait would report",
            ),
            (
                HEAD.to_owned(),
                all.replace(" --of \"$SHARDS\"", ""),
                "the wait step does not pass `--of \"$SHARDS\"`",
            ),
            (
                HEAD.to_owned(),
                all.replace("pattern: profraw-part-*", "pattern: other-*"),
                "the download step's `pattern` does not match what the wait step waits for",
            ),
            (
                HEAD.to_owned(),
                all.replace("--artifact-prefix profraw-part- ", ""),
                "the download step's `pattern` does not match what the wait step waits for",
            ),
            (
                HEAD.to_owned(),
                all.replace(
                    "name: profraw-part-${{ matrix.shard }}",
                    "name: profraw-other",
                ),
                "the upload step's `name` does not begin with the wait step's `--artifact-prefix`",
            ),
            (
                HEAD.to_owned(),
                all.replace("          overwrite: true\n", ""),
                "the upload step lacks `overwrite: true`, so a re-run of the run fails on the names its first run uploaded",
            ),
            (
                HEAD.to_owned(),
                all.replace("path: target/llvm-cov-target", "path: elsewhere"),
                "the download step must put the profiles flat in `target/llvm-cov-target` (`merge-multiple: true`)",
            ),
            (
                HEAD.to_owned(),
                all.replace("merge-multiple: true", "merge-multiple: false"),
                "the download step must put the profiles flat in `target/llvm-cov-target` (`merge-multiple: true`)",
            ),
        ];
        for (head, steps, want) in cases {
            assert_eq!(
                split_wiring(&workflow(&steps, &head, TEST)),
                Err(want.to_owned()),
                "{head}{steps}"
            );
        }
    }

    #[test]
    fn each_drift_of_the_required_check_is_refused() {
        // Catches: `test` skipped, not failed, when a part fails: no `needs`, no `if:`,
        // and no step that fails it on the parts' result.
        let all = good();
        let cases = [
            (
                TEST.replace("    needs: test-shard\n", ""),
                "the job `test` does not need `test-shard`",
            ),
            (
                TEST.replace("    if: ${{ !cancelled() }}\n", ""),
                "the job `test` has no `if:`, so it is skipped, not failed, when a part fails, and a skipped required check passes",
            ),
            (
                TEST.replace("-p kbf-coverage ", "-p other "),
                "the job `test` has no step running `kbf-coverage`",
            ),
            (
                TEST.replace("needs.test-shard.result", "github.run_id"),
                "no step before the gate reads a part's `needs.<job>.result`",
            ),
            (
                TEST.replace("      - env:", "      - if: ${{ always() }}\n        env:"),
                "no step before the gate reads a part's `needs.<job>.result`",
            ),
        ];
        for (test, want) in cases {
            assert_eq!(
                required_check_wiring(&workflow(&all, HEAD, &test), "test"),
                Err(want.to_owned()),
                "{test}"
            );
        }
    }
}
