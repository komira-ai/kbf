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

/// The only `if:` a step of `beta.yml` may carry.
const STEP_CONDITIONS: [&str; 2] = [
    "${{ !cancelled() }}",
    "${{ github.ref == 'refs/heads/main' && steps.cargo-cache.outputs.cache-hit != 'true' }}",
];

/// Every way `text` could let a run conclude `success` with a job or step not passed, one
/// sentence each; an error when the text does not parse.
fn problems(text: &str) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("continue-on-error") {
            found.push(format!(
                "{BETA}:{}: continue-on-error lets a failing job or step pass",
                n + 1
            ));
        }
    }
    for job in workflows::job_names(text)? {
        if let Some(c) = workflows::job_condition(text, &job)? {
            found.push(format!(
                "{BETA}: job {job} has the condition {c:?}; a skipped job counts as passed"
            ));
        }
        for step in workflows::job_steps(text, &job)? {
            if let Some(c) = step.condition.as_deref()
                && !STEP_CONDITIONS.contains(&c)
            {
                found.push(format!(
                    "{BETA}: a step of job {job} has the condition {c:?}, not one of the two allowed"
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
    let text = std::fs::read_to_string(root.join(BETA)).expect(BETA);
    assert_eq!(problems(&text), Ok(Vec::new()));
}

/// [`problems`] on beta.yml with a defect planted, to see each rule go red.
mod rules {
    use super::*;

    fn beta() -> String {
        std::fs::read_to_string(super::super::repo_root().join(BETA)).expect(BETA)
    }

    /// `beta()` with the first `from` replaced by `to`; the plant must change the text.
    fn planted(from: &str, to: &str) -> String {
        let text = beta();
        assert!(text.contains(from), "{from:?} not in {BETA}");
        text.replacen(from, to, 1)
    }

    #[test]
    fn continue_on_error_on_a_job_is_refused() {
        let text = planted(
            "  smoke:\n    runs-on: ubuntu-24.04\n",
            "  smoke:\n    continue-on-error: true\n    runs-on: ubuntu-24.04\n",
        );
        let found = problems(&text).unwrap();
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
        let found = problems(&text).unwrap();
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
        let found = problems(&text).unwrap();
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
        let found = problems(&text).unwrap();
        assert!(
            found.len() == 1 && found[0].contains("not one of the two allowed"),
            "{found:?}"
        );
    }
}
