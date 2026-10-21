//! `kbf`'s version facts in the pinned release set (docs/artifacts.md): each build job runs
//! the `kbf` it built with `--version --json` and keeps what it printed as an asset of the
//! release, `kbf-<commit>-<platform>.version.json`. Only a runner of a platform can run its
//! binary, and nothing on the frontend records what the binary says, so a job that loses
//! the step leaves a platform whose facts nobody can state, and no pull request run notices
//! (the sums check does, but only after the build: this names the job on the pull request).
//!
//! [`rules`] reads the workflow and names a build job that:
//!
//! - does not run `cargo xtask release version-facts` exactly once;
//! - runs it for another binary than `kbf`, another commit than `"$GITHUB_SHA"`, another
//!   platform than the job's own or another directory than `dist` (the one it uploads);
//! - runs it under `if:` or `continue-on-error:`;
//! - runs it before `cargo xtask build-commit check --json` of the `kbf` (so before the
//!   build is known to name this commit and be a release build; on macOS that check is of
//!   the signed bytes; a job with no check at all is the identity rule's to name), or at or
//!   after the step that packages the `kbf`;
//! - records another `--binary` path than the one it packages (another build's `kbf` has
//!   the same file name).

use super::*;

/// The command that records the facts.
const VERB: &str = "cargo xtask release version-facts ";

/// The step number, the step and the words of every line of `steps` that runs [`VERB`].
fn recorded(steps: &[workflows::Step]) -> Vec<(usize, &workflows::Step, Vec<&str>)> {
    let mut found = Vec::new();
    for (at, step) in steps.iter().enumerate() {
        for line in lines(step) {
            if let Some(words) = args(line, VERB) {
                found.push((at, step, words));
            }
        }
    }
    found
}

/// Appends to `out` what is wrong with how each build job of `workflow` records the facts
/// of its `kbf`. An error when the workflow cannot be read.
pub fn rules(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    for job in BUILD_JOBS {
        let steps = workflows::job_steps(workflow, job)?;
        let runs = recorded(&steps);
        let [(at, step, words)] = runs.as_slice() else {
            out.push(format!(
                "`{job}` runs `release version-facts` {} times, not once: the facts of its kbf are recorded exactly once",
                runs.len()
            ));
            continue;
        };
        let binary = flag(words, "--binary").map(base);
        if binary != Some("kbf") {
            out.push(format!(
                "`{job}` records the facts of {binary:?}, not of kbf (--binary)"
            ));
        }
        let wants = [
            ("--commit", "$GITHUB_SHA"),
            ("--platform", *job),
            ("--out", "dist"),
        ];
        for (name, want) in wants {
            if flag(words, name) != Some(want) {
                out.push(format!(
                    "`{job}` records the facts with {name} {:?}, not {want:?}",
                    flag(words, name)
                ));
            }
        }
        if step.conditional {
            out.push(format!(
                "`{job}` records the facts of kbf under `if:` or `continue-on-error:`"
            ));
        }
        let checks = |l: &str| {
            args(l, "cargo xtask build-commit check --json ")
                .is_some_and(|a| a.first().is_some_and(|b| base(b) == "kbf"))
        };
        // A job with no check at all is the identity rule's to name (one problem, not two).
        if first(&steps, 0, checks).is_some_and(|(c, _)| c >= *at) {
            out.push(format!(
                "`{job}` records the facts of kbf before `build-commit check --json` of it: only a build that names this commit and is a release build is recorded"
            ));
        }
        let packaged = first(&steps, 0, |l| packaged_kbf(l).is_some());
        let before_package = packaged.is_some_and(|(p, _)| *at < p);
        if !before_package {
            out.push(format!(
                "`{job}` records the facts of kbf at or after the step that packages it, or packages none: they are the facts of the binary that ships"
            ));
        }
        // The same path, not the same file name: another build's `kbf` has one too.
        let shipped = packaged.and_then(|(_, s)| lines(s).find_map(packaged_kbf));
        if let (Some(shipped), Some(recorded)) = (shipped, flag(words, "--binary"))
            && shipped != recorded
        {
            out.push(format!(
                "`{job}` records the facts of {recorded} but packages {shipped}: they are the facts of the binary that ships"
            ));
        }
    }
    Ok(())
}

/// The `--binary` of a line that packages the `kbf`.
fn packaged_kbf(line: &str) -> Option<&str> {
    let words = args(line, "cargo xtask package ")?;
    flag(&words, "--binary").filter(|b| base(b) == "kbf")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release_artifacts::BUILD_JOBS;

    const CHECK: &str =
        "cargo xtask build-commit check --json \"target/$TARGET/release/kbf\" \"$GITHUB_SHA\"";
    const PACKAGE: &str = "cargo xtask package --binary \"target/$TARGET/release/kbf\" --sbom s.json --platform x --commit \"$GITHUB_SHA\" --out dist";

    /// The command that records the facts of `kbf` for `job`, as the workflow has it.
    fn record(job: &str) -> String {
        format!(
            "cargo xtask release version-facts --binary \"target/$TARGET/release/kbf\" --commit \"$GITHUB_SHA\" --platform {job} --out dist"
        )
    }

    /// A step that runs `command`.
    fn run(command: &str) -> String {
        format!("      - run: {command}\n")
    }

    /// A workflow of the three build jobs, each with the steps `steps(job)` gives.
    fn workflow(steps: impl Fn(&str) -> Vec<String>) -> String {
        let mut w = String::from("on: push\njobs:\n");
        for job in BUILD_JOBS {
            w.push_str(&format!("  {job}:\n    runs-on: ubuntu-24.04\n    steps:\n"));
            for step in steps(job) {
                w.push_str(&step);
            }
        }
        w
    }

    /// The steps of a job that meets every rule.
    fn good(job: &str) -> Vec<String> {
        vec![run(CHECK), run(&record(job)), run(PACKAGE)]
    }

    /// A workflow whose jobs run the record command `edit` makes of the right one.
    fn recording(edit: impl Fn(String) -> String) -> String {
        workflow(|job| vec![run(CHECK), run(&edit(record(job))), run(PACKAGE)])
    }

    fn found(w: &str) -> Vec<String> {
        let mut out = Vec::new();
        rules(w, &mut out).expect("readable");
        out
    }

    /// Asserts that `w` has a problem containing `wanted`.
    fn names(w: &str, wanted: &str) {
        let got = found(w);
        assert!(
            got.iter().any(|p| p.contains(wanted)),
            "wanted a problem containing {wanted:?}, got {got:#?}"
        );
    }

    #[test]
    fn the_real_workflow_records_the_facts_of_kbf_in_every_build_job() {
        // Catches: a build job of artifacts.yml that does not record its kbf's facts (or
        // records another binary's, another commit's, another platform's), the control
        // for the plants below, which would otherwise go red for their own reasons.
        let path = crate::ARTIFACTS_WORKFLOW;
        let text = std::fs::read_to_string(crate::repo_root().join(path)).expect(path);
        assert_eq!(found(&text), Vec::<String>::new());
    }

    #[test]
    fn a_job_that_records_the_facts_between_the_check_and_the_package_passes() {
        // Catches: a rule that misreads a correct job (quoted words, a path with a
        // directory part), so every real change is refused.
        assert_eq!(found(&workflow(good)), Vec::<String>::new());
    }

    #[test]
    fn a_job_that_records_nothing_or_twice_is_named() {
        // Catches: the step lost from a job (the sums check would be the only thing to
        // notice, after the build), and run twice (the second would refuse to overwrite).
        let none = workflow(|_| vec![run(CHECK), run(PACKAGE)]);
        names(&none, "runs `release version-facts` 0 times");
        let twice = workflow(|job| {
            let step = run(&record(job));
            vec![run(CHECK), step.clone(), step, run(PACKAGE)]
        });
        names(&twice, "runs `release version-facts` 2 times");
    }

    #[test]
    fn a_job_that_records_the_wrong_thing_is_named() {
        // Catches: facts of another binary, of another commit, of another platform than
        // the job's (the file would be named for one platform and say another's build), or
        // written where the job does not upload.
        let deploy = recording(|c| c.replace("release/kbf\"", "release/kbf-deploy\""));
        names(&deploy, "of Some(\"kbf-deploy\")");
        let commit = recording(|c| c.replace("\"$GITHUB_SHA\" --platform", "abc --platform"));
        names(&commit, "--commit");
        let platform = recording(|c| c.replace("--platform ", "--platform x-"));
        names(&platform, "--platform");
        let out = recording(|c| c.replace("--out dist", "--out target/dist"));
        names(&out, "--out");
    }

    #[test]
    fn a_job_that_records_another_build_of_kbf_than_it_packages_is_named() {
        // Catches: the facts of a `kbf` of another build directory (a debug build, an
        // older target) that has the file name of the one that ships; the facts would be
        // true of a binary nobody installs. Mutant: the comparison dropped from `rules`.
        let w = workflow(|job| {
            let other = record(job).replace("target/$TARGET/release", "other/release");
            vec![run(CHECK), run(&other), run(PACKAGE)]
        });
        names(&w, "but packages target/$TARGET/release/kbf");
    }

    #[test]
    fn a_job_that_may_skip_the_step_is_named() {
        // Catches: the step under `if:` (or `continue-on-error:`), skipped while the job
        // passes.
        for condition in ["if: always()", "continue-on-error: true"] {
            let w = workflow(|job| {
                let step = format!("      - {condition}\n        run: {}\n", record(job));
                vec![run(CHECK), step, run(PACKAGE)]
            });
            names(&w, "under `if:` or `continue-on-error:`");
        }
    }

    #[test]
    fn a_job_that_records_out_of_order_is_named() {
        // Catches: the facts taken before the build is known to name the commit and be a
        // release build, and after the tarball is made or with none. A job with no check
        // at all is not named here: the identity rule names it, and one defect is one
        // problem (the test of that rule counts them).
        let before = workflow(|job| vec![run(&record(job)), run(CHECK), run(PACKAGE)]);
        names(&before, "before `build-commit check --json`");
        let unchecked = workflow(|job| vec![run(&record(job)), run(PACKAGE)]);
        assert_eq!(found(&unchecked), Vec::<String>::new());
        let after = workflow(|job| vec![run(CHECK), run(PACKAGE), run(&record(job))]);
        names(&after, "at or after the step that packages it");
        let unpackaged = workflow(|job| vec![run(CHECK), run(&record(job))]);
        names(&unpackaged, "at or after the step that packages it");
    }
}
