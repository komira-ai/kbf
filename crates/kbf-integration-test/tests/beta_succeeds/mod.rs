//! `beta.yml` succeeds only when every job did, and every job's success means its work
//! passed. `cargo xtask release gates` trusts beta's run conclusion for that: `kbf-beta`
//! exits 0 only on a PASSED verdict (`crates/kbf-harness/src/cli.rs`, `b0.rs`), so a run
//! that concluded `success` is a PASSED beta by construction. Three things would break
//! that without a red step, and this lint refuses each in `beta.yml`:
//!
//! - any `continue-on-error`, at a job or a step: a failing job or step would leave the run green;
//! - a job-level `if:`: a skipped job counts as passed in the run's conclusion;
//! - a step `if:` other than the two the file has today: `!cancelled()` (the artifact
//!   upload and the later conformance steps, which still run, and still fail the job, after
//!   an earlier step failed) and the cache save on `main`.
//!
//! A new condition is a reviewed change to [`STEP_CONDITIONS`] with the reason why it cannot
//! pass a failing job.

use super::workflows;

/// The workflow whose conclusion `release gates` reads.
const BETA: &str = ".github/workflows/beta.yml";

/// The other workflow whose jobs the release gate reads: the beta smoke, which tests the run's artifacts.
const SMOKE: &str = ".github/workflows/smoke.yml";

/// The only `if:` a step of `beta.yml` may carry.
const STEP_CONDITIONS: [&str; 2] = [
    "${{ !cancelled() }}",
    "${{ github.ref == 'refs/heads/main' && steps.cargo-cache.outputs.cache-hit != 'true' }}",
];

/// Every way `text` could let a run conclude `success` with a job or step not passed, one
/// sentence each; an error when the text does not parse.
fn problems(file: &str, text: &str) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("continue-on-error") {
            found.push(format!(
                "{file}:{}: continue-on-error lets a failing job or step pass",
                n + 1
            ));
        }
    }
    for job in workflows::job_names(text)? {
        if let Some(c) = workflows::job_condition(text, &job)? {
            found.push(format!(
                "{file}: job {job} has the condition {c:?}; a skipped job counts as passed"
            ));
        }
        for step in workflows::job_steps(text, &job)? {
            if let Some(c) = step.condition.as_deref()
                && !STEP_CONDITIONS.contains(&c)
            {
                found.push(format!(
                    "{file}: a step of job {job} has the condition {c:?}, not one of the two allowed"
                ));
            }
        }
    }
    Ok(found)
}

#[test]
fn a_green_beta_run_is_a_passed_beta() {
    // Catches: a `continue-on-error` or a skippable job added to beta.yml, which would let
    // `release gates` judge a beta green that did not pass.
    let root = super::repo_root();
    for file in [BETA, SMOKE] {
        let text = std::fs::read_to_string(root.join(file)).expect(file);
        assert_eq!(problems(file, &text), Ok(Vec::new()), "{file}");
    }
}

/// [`problems`] on smoke.yml and beta.yml with one defect added, to see each rule go red.
mod rules {
    use super::*;

    fn read(file: &str) -> String {
        std::fs::read_to_string(super::super::repo_root().join(file)).expect(file)
    }

    /// smoke.yml with the first `from` replaced by `to`; the plant must change the text.
    fn planted(from: &str, to: &str) -> String {
        edited_in(SMOKE, from, to)
    }

    fn edited_in(file: &str, from: &str, to: &str) -> String {
        let text = read(file);
        assert!(text.contains(from), "{from:?} not in {file}");
        text.replacen(from, to, 1)
    }

    #[test]
    fn continue_on_error_on_a_job_is_refused() {
        let text = planted(
            "  smoke:\n    runs-on: ubuntu-24.04\n",
            "  smoke:\n    continue-on-error: true\n    runs-on: ubuntu-24.04\n",
        );
        let found = problems(SMOKE, &text).unwrap();
        assert!(
            found.len() == 1 && found[0].contains("continue-on-error"),
            "{found:?}"
        );
    }

    #[test]
    fn continue_on_error_on_a_step_is_refused() {
        let text = planted(
            "      - name: B0, the farm under build load\n",
            "      - name: B0, the farm under build load\n        continue-on-error: true\n",
        );
        let found = problems(SMOKE, &text).unwrap();
        assert!(
            found.len() == 1 && found[0].contains("continue-on-error"),
            "{found:?}"
        );
    }

    #[test]
    fn a_job_condition_is_refused() {
        let text = planted(
            "  smoke:\n    runs-on: ubuntu-24.04\n",
            "  smoke:\n    if: ${{ github.event_name == 'push' }}\n    runs-on: ubuntu-24.04\n",
        );
        let found = problems(SMOKE, &text).unwrap();
        assert!(
            found.len() == 1 && found[0].contains("job smoke has the condition"),
            "{found:?}"
        );
    }

    #[test]
    fn a_new_step_condition_is_refused() {
        let text = planted(
            "      - name: B0, the farm under build load\n",
            "      - name: B0, the farm under build load\n        if: ${{ always() }}\n",
        );
        let found = problems(SMOKE, &text).unwrap();
        assert!(
            found.len() == 1 && found[0].contains("not one of the two allowed"),
            "{found:?}"
        );
    }

    #[test]
    fn the_same_rules_hold_for_the_jobs_of_beta_yml() {
        // Catches: a job of beta.yml that can fail without failing the run, once the suite no
        // longer shares a file with the smoke.
        let text = edited_in(
            BETA,
            "  test-x86:\n    runs-on: ubuntu-latest\n",
            "  test-x86:\n    continue-on-error: true\n    runs-on: ubuntu-latest\n",
        );
        let found = problems(BETA, &text).unwrap();
        assert!(
            found.len() == 1
                && found[0].starts_with(BETA)
                && found[0].contains("continue-on-error"),
            "{found:?}"
        );
    }
}
