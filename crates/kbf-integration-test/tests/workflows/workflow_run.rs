//! The `workflow_run` trigger the workflow lint refuses.
//!
//! A `workflow_run` workflow starts when another workflow completes, runs the default
//! branch's copy of the file whatever ref the other run was for, and holds this repository's
//! token and its environments' credentials. The one workflow that had one, `gamma.yml`, is
//! deleted (gamma moves to kbf-updater), so [`rules`] refuses the trigger in every form and
//! in every workflow. The message begins `line 0:` (the reader gives no line).

use super::*;

/// What a workflow with the trigger is told.
const REFUSED: &str = "triggers on workflow_run, which no workflow may";

pub(super) fn is_workflow_run(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case("workflow_run")
}

/// The problems with a workflow that triggers on `workflow_run`: one. Nothing for a workflow
/// that does not trigger on it, or does not parse (the lint reports that).
pub(super) fn rules(text: &str) -> Vec<String> {
    let follows = events(text).is_ok_and(|on| on.iter().any(|e| is_workflow_run(e)));
    if follows {
        vec![format!("line 0: {REFUSED}")]
    } else {
        Vec::new()
    }
}

/// The verbs of the retired gamma offer path: `gamma.yml` was their only caller, and they are
/// deleted in the cleanup that follows. No workflow may run them again.
#[cfg(test)]
const RETIRED_VERBS: &[&str] = &["release stage-offer", "xtask gamma"];

/// Catches: a workflow (a re-added `gamma.yml`, or a job in another file) that runs the retired
/// offer path, which a release now fires no part of. Mutant: a job running `cargo xtask release
/// stage-offer` added to a workflow file.
#[cfg(test)]
fn retired_verbs_in(name: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    if name == "gamma.yml" {
        found.push("gamma.yml is deleted: gamma moves to kbf-updater".to_owned());
    }
    for verb in RETIRED_VERBS.iter().filter(|v| text.contains(**v)) {
        found.push(format!("{name} runs `{verb}`, a retired verb"));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_workflow_runs_the_retired_offer_path() {
        let dir = crate::repo_root().join(".github/workflows");
        let mut found = Vec::new();
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).expect("workflow dir") {
            let path = entry.expect("entry").path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !(name.ends_with(".yml") || name.ends_with(".yaml")) {
                continue;
            }
            seen += 1;
            let text = std::fs::read_to_string(&path).expect("workflow text");
            found.extend(retired_verbs_in(&name, &text));
        }
        assert!(seen > 0, "no workflow file read");
        assert_eq!(found, Vec::<String>::new());
    }

    #[test]
    fn a_job_that_runs_stage_offer_is_refused() {
        let job = "  offer:\n    steps:\n      - run: cargo xtask release stage-offer --x\n";
        assert_eq!(retired_verbs_in("ci.yml", job).len(), 1);
        assert_eq!(retired_verbs_in("gamma.yml", "name: gamma\n").len(), 1);
        assert!(retired_verbs_in("ci.yml", "name: ci\n").is_empty());
    }
}
