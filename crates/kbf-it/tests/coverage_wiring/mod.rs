//! The coverage job's ratchet step prints a command that downloads this run's
//! artifact (`kbf-coverage --run-id ... --artifact ...`), so the step's flags must
//! name what the job uploads: the run is `$GITHUB_RUN_ID` (the id `gh run download`
//! takes; `$GITHUB_RUN_NUMBER` is another number), the repository is
//! `$GITHUB_REPOSITORY`, and `--artifact` is the `name:` of an `actions/upload-artifact`
//! step after it whose `path:` lists the `--write-baseline` file. Nothing else fails
//! when they drift: the printed command just stops working, and only a contributor
//! whose coverage improved finds out. [`wiring`] reads them from the workflow.
//!
//! Not covered: whether the upload step runs after a failed ratchet step (its `if:`
//! value is not read here).

use super::workflows;

/// The workflow and job that run the ratchet.
const CI: &str = ".github/workflows/ci.yml";
const JOB: &str = "coverage";

/// The `with:` input `key` of `step`.
fn input<'a>(step: &'a workflows::Step, key: &str) -> Option<&'a str> {
    step.with
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// The `kbf-coverage` step of `job` agrees with the upload step after it; an error
/// says how it does not.
fn wiring(workflow: &str, job: &str) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, job)?;
    let (at, run) = steps
        .iter()
        .enumerate()
        .find_map(|(i, s)| {
            Some((i, s.run.as_deref()?)).filter(|(_, r)| r.contains("-p kbf-coverage "))
        })
        .ok_or_else(|| format!("the job `{job}` has no step running `-p kbf-coverage`"))?;
    let words: Vec<&str> = run.split_whitespace().collect();
    let flag = |f: &str| {
        let at = words.iter().position(|w| *w == f)?;
        words.get(at + 1).map(|v| v.trim_matches('"'))
    };
    let need = |f: &str| flag(f).ok_or_else(|| format!("the kbf-coverage step has no `{f}`"));
    for (f, want) in [
        ("--run-id", "$GITHUB_RUN_ID"),
        ("--repo", "$GITHUB_REPOSITORY"),
    ] {
        let got = need(f)?;
        if got != want {
            return Err(format!(
                "the kbf-coverage step passes `{f} {got}`, not `{f} \"{want}\"`"
            ));
        }
    }
    let artifact = need("--artifact")?;
    let written = need("--write-baseline")?;
    let upload = steps[at + 1..]
        .iter()
        .filter(|s| {
            s.uses
                .as_deref()
                .is_some_and(|u| u.starts_with("actions/upload-artifact@"))
        })
        .find(|s| input(s, "name") == Some(artifact))
        .ok_or_else(|| {
            format!(
                "no actions/upload-artifact step after the kbf-coverage step is named `{artifact}`"
            )
        })?;
    let paths = input(upload, "path").unwrap_or_default();
    if !paths.lines().any(|p| p.trim() == written) {
        return Err(format!(
            "the `{artifact}` upload does not list `{written}` in its path"
        ));
    }
    Ok(())
}

#[test]
fn the_coverage_command_downloads_what_the_job_uploads() {
    // Catches: the artifact renamed on one side only, the run named by another
    // variable (`$GITHUB_RUN_NUMBER`), or the measured file dropped from the upload,
    // each of which leaves a printed `gh run download` command that cannot work.
    let text = std::fs::read_to_string(super::repo_root().join(CI)).expect(CI);
    assert_eq!(wiring(&text, JOB), Ok(()), "{CI}");
}

/// The rules [`wiring`] reads the workflow by, on planted workflows.
mod rules {
    use super::*;

    const PIN: &str = "043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1";

    /// A coverage job: the ratchet step with `flags`, then `after`.
    fn workflow(flags: &str, after: &str) -> String {
        format!(
            "on: push\npermissions: {{}}\njobs:\n  coverage:\n    runs-on: ubuntu-latest\n    steps:\n      - name: Per-crate table and ratchet\n        run: cargo run -p kbf-coverage --locked -- --lcov l --root r --baseline b --write-baseline out/m {flags}\n{after}"
        )
    }

    const FLAGS: &str = "--run-id \"$GITHUB_RUN_ID\" --repo \"$GITHUB_REPOSITORY\" --artifact cov";

    fn upload(name: &str, path: &str) -> String {
        format!(
            "      - uses: actions/upload-artifact@{PIN}\n        if: ${{{{ !cancelled() }}}}\n        with:\n          name: {name}\n          path: |\n            out/lcov.info\n            {path}\n"
        )
    }

    #[test]
    fn a_matching_step_and_upload_pass() {
        // Catches: the rule refusing the workflow shape CI uses (quoted values, a
        // multi-line path, another upload before the matching one).
        let other = upload("other", "x");
        let ok = format!("{other}{}", upload("cov", "out/m"));
        assert_eq!(wiring(&workflow(FLAGS, &ok), "coverage"), Ok(()));
    }

    #[test]
    fn each_drift_is_refused() {
        // Catches: each way the printed command stops working accepted.
        let ok = upload("cov", "out/m");
        for (flags, after, want) in [
            (
                FLAGS,
                upload("coverage", "out/m"),
                "no actions/upload-artifact step after the kbf-coverage step is named `cov`",
            ),
            (
                "--run-id \"$GITHUB_RUN_NUMBER\" --repo \"$GITHUB_REPOSITORY\" --artifact cov",
                ok.clone(),
                "the kbf-coverage step passes `--run-id $GITHUB_RUN_NUMBER`, not `--run-id \"$GITHUB_RUN_ID\"`",
            ),
            (
                "--run-id \"$GITHUB_RUN_ID\" --repo o/r --artifact cov",
                ok.clone(),
                "the kbf-coverage step passes `--repo o/r`, not `--repo \"$GITHUB_REPOSITORY\"`",
            ),
            (
                "--run-id \"$GITHUB_RUN_ID\" --repo \"$GITHUB_REPOSITORY\"",
                ok.clone(),
                "the kbf-coverage step has no `--artifact`",
            ),
            (
                "--repo \"$GITHUB_REPOSITORY\" --artifact cov",
                ok.clone(),
                "the kbf-coverage step has no `--run-id`",
            ),
            (
                FLAGS,
                upload("cov", "out/other"),
                "the `cov` upload does not list `out/m` in its path",
            ),
            (
                FLAGS,
                String::new(),
                "no actions/upload-artifact step after the kbf-coverage step is named `cov`",
            ),
        ] {
            assert_eq!(
                wiring(&workflow(flags, &after), "coverage"),
                Err(want.to_owned()),
                "{flags} {after}"
            );
        }
        // The upload must come after the step that writes the file.
        let before = format!(
            "on: push\npermissions: {{}}\njobs:\n  coverage:\n    runs-on: ubuntu-latest\n    steps:\n{ok}      - run: cargo run -p kbf-coverage -- --write-baseline out/m {FLAGS}\n"
        );
        assert_eq!(
            wiring(&before, "coverage"),
            Err(
                "no actions/upload-artifact step after the kbf-coverage step is named `cov`"
                    .to_owned()
            )
        );
        assert_eq!(
            wiring(
                &workflow(FLAGS, &ok).replace("-p kbf-coverage", "-p kbf-other"),
                "coverage"
            ),
            Err("the job `coverage` has no step running `-p kbf-coverage`".to_owned())
        );
        // An upload with no path lists nothing.
        let no_path = format!(
            "      - uses: actions/upload-artifact@{PIN}\n        with:\n          name: cov\n"
        );
        assert_eq!(
            wiring(&workflow(FLAGS, &no_path), "coverage"),
            Err("the `cov` upload does not list `out/m` in its path".to_owned())
        );
    }
}
