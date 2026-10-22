//! The `workflow_run` trigger, allowed in one exact shape and refused in every other.
//!
//! A `workflow_run` workflow starts when another workflow completes, runs the default
//! branch's copy of the file whatever ref the other run was for, and holds this repository's
//! token and its environments' credentials. The release is the one use: it must outlive a push
//! that cancels the pipeline run it follows, so it is a workflow of its own started by the
//! success of the pipeline (`release.yml`; see its header). [`rules`] therefore refuses the
//! trigger in every form and in every workflow but this: `workflows: [pipeline]`, `types:
//! [completed]` and `branches: [main]`, nothing else. A repository test
//! (`only_the_release_workflow_follows_the_pipeline`) holds the trigger to `release.yml` and the
//! workflow named `pipeline` to a push to main alone. The message begins `line 0:` (the reader
//! gives no line).

use super::*;

/// What a workflow with the trigger in any other shape is told.
const REFUSED: &str = "triggers on workflow_run, which no workflow may but the release after the pipeline (workflows: [pipeline], types: [completed], branches: [main])";

/// The filters of the one allowed shape, by key.
const ALLOWED: [(&str, &str); 3] = [
    ("branches", "main"),
    ("types", "completed"),
    ("workflows", "pipeline"),
];

pub(super) fn is_workflow_run(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case("workflow_run")
}

/// Whether the workflow's `workflow_run` filters are exactly [`ALLOWED`].
fn is_the_allowed_shape(text: &str) -> bool {
    let Ok(mut filters) = trigger_filters(text, "workflow_run") else {
        return false;
    };
    filters.sort();
    filters.len() == ALLOWED.len()
        && filters
            .iter()
            .zip(ALLOWED)
            .all(|((key, values), (want_key, want))| {
                key == want_key && values.as_slice() == [want.to_owned()]
            })
}

/// The problems with a workflow that triggers on `workflow_run`: one unless it is the allowed
/// shape. Nothing for a workflow that does not trigger on it, or does not parse (the lint
/// reports that).
pub(super) fn rules(text: &str) -> Vec<String> {
    let follows = events(text).is_ok_and(|on| on.iter().any(|e| is_workflow_run(e)));
    if follows && !is_the_allowed_shape(text) {
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

    /// The `name:` of a workflow text, or empty.
    fn name_of(text: &str) -> String {
        text.lines()
            .find_map(|l| l.strip_prefix("name: "))
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    #[test]
    fn only_the_release_workflow_follows_the_pipeline() {
        let dir = crate::repo_root().join(".github/workflows");
        let mut followers = Vec::new();
        let mut pipelines = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("workflow dir") {
            let path = entry.expect("entry").path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !(name.ends_with(".yml") || name.ends_with(".yaml")) {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("workflow text");
            if events(&text).is_ok_and(|on| on.iter().any(|e| is_workflow_run(e))) {
                followers.push(name.clone());
            }
            if name_of(&text) == "pipeline" {
                pipelines.push(name);
            }
        }
        assert_eq!(
            followers,
            ["release.yml"],
            "the workflows that follow another"
        );
        assert_eq!(
            pipelines,
            ["pipeline.yml"],
            "the workflows named `pipeline`"
        );
    }

    #[test]
    fn the_allowed_shape_is_accepted_and_every_other_is_refused() {
        let on =
            |body: &str| format!("on:\n  workflow_run:\n{body}\npermissions: {{}}\njobs: {{}}\n");
        let good = "    workflows: [pipeline]\n    types: [completed]\n    branches: [main]";
        assert!(rules(&on(good)).is_empty());
        for bad in [
            "    workflows: [pipeline, ci]\n    types: [completed]\n    branches: [main]",
            "    workflows: [ci]\n    types: [completed]\n    branches: [main]",
            "    workflows: [pipeline]\n    types: [requested]\n    branches: [main]",
            "    workflows: [pipeline]\n    types: [completed]",
            "    workflows: [pipeline]\n    types: [completed]\n    branches: ['**']",
            "    workflows: [pipeline]\n    types: [completed]\n    branches: [main]\n    branches-ignore: [x]",
        ] {
            assert_eq!(rules(&on(bad)).len(), 1, "{bad}");
        }
    }

    #[test]
    fn a_job_that_runs_stage_offer_is_refused() {
        let job = "  offer:\n    steps:\n      - run: cargo xtask release stage-offer --x\n";
        assert_eq!(retired_verbs_in("ci.yml", job).len(), 1);
        assert_eq!(retired_verbs_in("gamma.yml", "name: gamma\n").len(), 1);
        assert!(retired_verbs_in("ci.yml", "name: ci\n").is_empty());
    }
}
