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
//!
//! [`partition_wiring`] holds the rules of the parts themselves: three of them, every nextest
//! run of a part (the legacy scheduler core and the js core) takes its third by the hash of the
//! test (`--partition=hash:$SHARD/$SHARDS`, so that adding a test moves no other), and the
//! matrix lists exactly the parts the count names, so no part is left out and no test goes
//! unrun. [`union_wiring`] holds the order of merge and gate: the one report is made from all
//! the parts' profiles, and the `test` job gates that tracefile, not a part's own.

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

/// The number of parts the fast tests are split in.
const PARTS: u32 = 3;

/// The value of the entry `key` of the workflow's top-level `env:` (an entry at two spaces).
fn top_env(workflow: &str, key: &str) -> Option<String> {
    let at = workflow.find(&format!("\n  {key}: "))? + key.len() + 5;
    Some(workflow[at..].lines().next()?.trim().to_owned())
}

/// The numbers of the line `shard: [1, 2, 3]` of the matrix.
fn matrix_parts(workflow: &str) -> Result<Vec<u32>, String> {
    let at = workflow
        .find("shard: [")
        .ok_or("the job `test-shard` has no matrix `shard: [...]`")?
        + "shard: [".len();
    let list = workflow[at..].split(']').next().unwrap_or_default();
    list.split(',')
        .map(|n| {
            n.trim().parse().map_err(|_| {
                format!(
                    "the matrix `shard` has `{}`, which is not a number",
                    n.trim()
                )
            })
        })
        .collect()
}

/// The parts of `workflow` are `PARTS` and each takes its share of every nextest run by hash.
fn partition_wiring(workflow: &str) -> Result<(), String> {
    let count = top_env(workflow, "SHARDS").ok_or("the workflow sets no top-level `SHARDS`")?;
    if count != PARTS.to_string() {
        return Err(format!(
            "`SHARDS` is {count}, not {PARTS}: the fast tests run in {PARTS} parts"
        ));
    }
    let parts = matrix_parts(workflow)?;
    if parts != (1..=PARTS).collect::<Vec<_>>() {
        return Err(format!(
            "the matrix lists the parts {parts:?}, not 1 to {PARTS}, so a share of the tests would not run"
        ));
    }
    let env = workflows::job_env(workflow, SHARD)?;
    if !env
        .iter()
        .any(|(k, v)| k == "SHARD" && v.contains("matrix.shard"))
    {
        return Err("the job `test-shard` does not set `SHARD` to `matrix.shard`".to_owned());
    }
    let steps = workflows::job_steps(workflow, SHARD)?;
    let runs: Vec<_> = steps
        .iter()
        .filter(|s| run_of(s).contains("llvm-cov nextest"))
        .collect();
    if runs.is_empty() {
        return Err("the job `test-shard` runs no `llvm-cov nextest`".to_owned());
    }
    for run in runs {
        if !words(run_of(run)).contains(&"--partition=hash:$SHARD/$SHARDS") {
            return Err(
                "a nextest run of `test-shard` does not pass `--partition=hash:$SHARD/$SHARDS`, so every part would run all of its tests"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// The tracefile the gate reads is the one report made from all the parts' profiles.
fn union_wiring(workflow: &str, job: &str) -> Result<(), String> {
    let shard = workflows::job_steps(workflow, SHARD)?;
    let report = shard
        .iter()
        .find(|s| run_of(s).contains("llvm-cov report"))
        .ok_or("no step of `test-shard` runs `llvm-cov report`")?;
    let out = flag(&words(run_of(report)), "--output-path")
        .ok_or("the report step has no `--output-path`")?
        .to_owned();
    let upload = shard
        .iter()
        .find(|s| input(s, "name") == Some("coverage"))
        .ok_or("no step of `test-shard` uploads the artifact `coverage`")?;
    if input(upload, "path") != Some(out.as_str()) {
        return Err(format!(
            "the upload of `coverage` does not take the report's `{out}`"
        ));
    }
    if upload.condition != report.condition {
        return Err("the upload of `coverage` has another `if:` than the report step, so a part that made no report would upload one".to_owned());
    }
    let steps = workflows::job_steps(workflow, job)?;
    let at = |pred: &dyn Fn(&workflows::Step) -> bool| steps.iter().position(pred);
    let download = at(&|s| input(s, "name") == Some("coverage"))
        .ok_or_else(|| format!("the job `{job}` does not download the artifact `coverage`"))?;
    let gate =
        at(&|s| run_of(s).contains("-p kbf-coverage ") && !run_of(s).contains("no-lowering"))
            .ok_or_else(|| format!("the job `{job}` has no gate step"))?;
    if download >= gate {
        return Err("the gate step comes before the download of `coverage`".to_owned());
    }
    let dir = input(&steps[download], "path").unwrap_or_default();
    let name = out.rsplit('/').next().unwrap_or_default();
    let lcov = flag(&words(run_of(&steps[gate])), "--lcov").unwrap_or_default();
    if lcov != format!("{dir}/{name}") {
        return Err(format!(
            "the gate reads `{lcov}`, not the downloaded `{dir}/{name}`"
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
            && s.env.iter().any(|(k, v)| {
                let var = format!("${k}");
                v.contains("needs.")
                    && v.contains(".result")
                    && words(run_of(s)) == ["test", var.as_str(), "=", "success"]
            })
    });
    if !checks {
        return Err("no step before the gate fails the job unless a part's `needs.<job>.result` is `success` (`test \"$VAR\" = success`)".to_owned());
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
fn the_fast_tests_run_in_three_hash_partitions_and_the_gate_reads_their_union() {
    // Catches: a part count or matrix that leaves a share of the tests unrun, a nextest run
    // (either scheduler core) that every part runs whole, and a gate that reads a tracefile
    // other than the report made from all the parts' profiles.
    let text = std::fs::read_to_string(super::repo_root().join(CI)).expect(CI);
    assert_eq!(partition_wiring(&text), Ok(()), "{CI}");
    assert_eq!(union_wiring(&text, JOB), Ok(()), "{CI}");
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
                "no step before the gate fails the job unless a part's `needs.<job>.result` is `success` (`test \"$VAR\" = success`)",
            ),
            (
                TEST.replace("test \"$R\" = success", "echo \"$R\""),
                "no step before the gate fails the job unless a part's `needs.<job>.result` is `success` (`test \"$VAR\" = success`)",
            ),
            (
                TEST.replace("= success", "= failure"),
                "no step before the gate fails the job unless a part's `needs.<job>.result` is `success` (`test \"$VAR\" = success`)",
            ),
            (
                TEST.replace("      - env:", "      - if: ${{ always() }}\n        env:"),
                "no step before the gate fails the job unless a part's `needs.<job>.result` is `success` (`test \"$VAR\" = success`)",
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

/// The rules [`partition_wiring`] and [`union_wiring`] read the workflow by.
mod partition_rules {
    use super::*;

    const TOP: &str = "on: push\npermissions: {}\nenv:\n  CARGO_INCREMENTAL: 0\n  SHARDS: 3\n";
    const HEAD: &str = "jobs:\n  test-shard:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        shard: [1, 2, 3]\n    env:\n      SHARD: ${{ matrix.shard }}\n    steps:\n";
    const LEGACY: &str = "      - run: cargo llvm-cov nextest --workspace --profile pr \"--partition=hash:$SHARD/$SHARDS\"\n";
    const JS: &str = "      - run: cargo llvm-cov nextest -p kbf-server --features kbf-server/farm-core-js --profile pr \"--partition=hash:$SHARD/$SHARDS\"\n";
    const LAST: &str = "${{ format('{0}', matrix.shard) == env.SHARDS }}";
    const REPORT: &str = "      - if: ${{ format('{0}', matrix.shard) == env.SHARDS }}\n        run: cargo llvm-cov report --lcov --output-path target/coverage/lcov.info\n";
    const UPLOAD: &str = "      - if: ${{ format('{0}', matrix.shard) == env.SHARDS }}\n        uses: actions/upload-artifact@abc # v7\n        with:\n          name: coverage\n          path: target/coverage/lcov.info\n";
    const NOT_PARTITIONED: &str = "a nextest run of `test-shard` does not pass `--partition=hash:$SHARD/$SHARDS`, so every part would run all of its tests";
    const TEST: &str = "  test:\n    needs: test-shard\n    steps:\n      - uses: actions/download-artifact@abc # v8\n        with:\n          name: coverage\n          path: target/coverage\n      - run: cargo run -p kbf-coverage --locked -- --lcov target/coverage/lcov.info --root r\n";

    fn workflow(top: &str, head: &str, steps: &str, test: &str) -> String {
        format!("{top}{head}{steps}{test}")
    }

    fn good_steps() -> String {
        format!("{LEGACY}{JS}{REPORT}{UPLOAD}")
    }

    #[test]
    fn the_shape_ci_uses_passes() {
        let w = workflow(TOP, HEAD, &good_steps(), TEST);
        assert_eq!(partition_wiring(&w), Ok(()));
        assert_eq!(union_wiring(&w, "test"), Ok(()));
    }

    #[test]
    fn each_drift_of_the_partitions_is_refused() {
        // Catches: a part count other than three, a matrix that leaves a part out or adds one the
        // count does not know, a nextest run (either core) that is not partitioned or is dealt by
        // `count` (which moves every later test when one is added), and a part that does not know
        // its number.
        let steps = good_steps();
        let cases = [
            (
                TOP.replace("SHARDS: 3", "SHARDS: 2"),
                HEAD.to_owned(),
                steps.clone(),
                "`SHARDS` is 2, not 3: the fast tests run in 3 parts",
            ),
            (
                TOP.replace("  SHARDS: 3\n", ""),
                HEAD.to_owned(),
                steps.clone(),
                "the workflow sets no top-level `SHARDS`",
            ),
            (
                TOP.to_owned(),
                HEAD.replace("[1, 2, 3]", "[1, 2]"),
                steps.clone(),
                "the matrix lists the parts [1, 2], not 1 to 3, so a share of the tests would not run",
            ),
            (
                TOP.to_owned(),
                HEAD.replace("[1, 2, 3]", "[1, 2, 3, 4]"),
                steps.clone(),
                "the matrix lists the parts [1, 2, 3, 4], not 1 to 3, so a share of the tests would not run",
            ),
            (
                TOP.to_owned(),
                HEAD.replace("SHARD: ${{ matrix.shard }}", "SHARD: 1"),
                steps.clone(),
                "the job `test-shard` does not set `SHARD` to `matrix.shard`",
            ),
            (
                TOP.to_owned(),
                HEAD.to_owned(),
                steps.replace(
                    LEGACY,
                    &LEGACY.replace(" \"--partition=hash:$SHARD/$SHARDS\"", ""),
                ),
                NOT_PARTITIONED,
            ),
            (
                TOP.to_owned(),
                HEAD.to_owned(),
                steps.replace(JS, &JS.replace(" \"--partition=hash:$SHARD/$SHARDS\"", "")),
                NOT_PARTITIONED,
            ),
            (
                TOP.to_owned(),
                HEAD.to_owned(),
                steps.replace("--partition=hash:", "--partition=count:"),
                NOT_PARTITIONED,
            ),
            (
                TOP.to_owned(),
                HEAD.to_owned(),
                format!("{REPORT}{UPLOAD}"),
                "the job `test-shard` runs no `llvm-cov nextest`",
            ),
        ];
        for (top, head, steps, want) in cases {
            assert_eq!(
                partition_wiring(&workflow(&top, &head, &steps, TEST)),
                Err(want.to_owned()),
                "{steps}"
            );
        }
    }

    #[test]
    fn each_drift_of_the_union_is_refused() {
        // Catches: a gate that reads a tracefile other than the one report made from all the
        // parts' profiles: a download after the gate, a gate on another path, an upload of
        // another file, and an upload by a part that made no report.
        let steps = good_steps();
        let cases = [
            (
                steps.replace("--output-path target/coverage/lcov.info", ""),
                TEST.to_owned(),
                "the report step has no `--output-path`",
            ),
            (
                steps.replace("name: coverage", "name: other"),
                TEST.to_owned(),
                "no step of `test-shard` uploads the artifact `coverage`",
            ),
            (
                steps.replace(
                    "path: target/coverage/lcov.info",
                    "path: target/elsewhere/lcov.info",
                ),
                TEST.to_owned(),
                "the upload of `coverage` does not take the report's `target/coverage/lcov.info`",
            ),
            (
                steps.replacen(
                    &format!("      - if: {LAST}\n        uses"),
                    "      - uses",
                    1,
                ),
                TEST.to_owned(),
                "the upload of `coverage` has another `if:` than the report step, so a part that made no report would upload one",
            ),
            (
                steps.clone(),
                TEST.replace("name: coverage", "name: other"),
                "the job `test` does not download the artifact `coverage`",
            ),
            (
                steps.clone(),
                TEST.replace(
                    "--lcov target/coverage/lcov.info",
                    "--lcov coverage/lcov.info",
                ),
                "the gate reads `coverage/lcov.info`, not the downloaded `target/coverage/lcov.info`",
            ),
            (
                steps.clone(),
                TEST.replace("path: target/coverage", "path: other"),
                "the gate reads `target/coverage/lcov.info`, not the downloaded `other/lcov.info`",
            ),
            (
                steps.clone(),
                TEST.replace("-p kbf-coverage ", "-p other "),
                "the job `test` has no gate step",
            ),
        ];
        for (steps, test, want) in cases {
            assert_eq!(
                union_wiring(&workflow(TOP, HEAD, &steps, &test), "test"),
                Err(want.to_owned()),
                "{steps}{test}"
            );
        }
        let (download, gate) = TEST.split_once("      - run:").expect("two steps");
        let gate_first = format!(
            "      - run:{gate}{}",
            download.replace("  test:\n    needs: test-shard\n    steps:\n", "")
        );
        let reordered = format!("  test:\n    needs: test-shard\n    steps:\n{gate_first}");
        assert_eq!(
            union_wiring(&workflow(TOP, HEAD, &steps, &reordered), "test"),
            Err("the gate step comes before the download of `coverage`".to_owned())
        );
    }
}
