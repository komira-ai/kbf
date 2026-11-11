//! The `workflow_run` trigger, allowed in one exact shape and refused in every other.
//!
//! A `workflow_run` workflow starts when another workflow completes, runs the default
//! branch's copy of the file whatever ref the other run was for, and holds this repository's
//! token and its environments' credentials. The release is the one use: it must outlive a push
//! that cancels the pipeline run it follows, so it is a workflow of its own started by the
//! success of the pipeline (`release.yml`; see its header); the rerun of a failed artifact job is the
//! other (`pipeline-rerun.yml`: the run it follows has completed, which a rerun needs). [`rules`] therefore refuses the
//! trigger in every form and in every workflow but this: `workflows: [pipeline]`, `types:
//! [completed]` and `branches: [main]`, nothing else. A repository test
//! (`only_the_release_workflow_follows_the_pipeline`) holds the trigger to `release.yml` and the
//! workflow named `pipeline` to a push to main alone. The message begins `line 0:` (the reader
//! gives no line).
//!
//! One more shape is allowed, in the one workflow named `hostkeeper-desired` (`hostkeeper-desired.yml`):
//! `workflows: [release]` with the same `types` and `branches`, which sets hostkeeper's desired
//! version after a release run. The lint holds it by the workflow's `name:`, and the repository
//! test `only_the_hostkeeper_workflow_is_named_so_and_follows_the_release` holds that name to the
//! file.

use super::*;

/// What a workflow with the trigger in any other shape is told.
const REFUSED: &str = "triggers on workflow_run, which no workflow may but the release and the rerun after the pipeline (workflows: [pipeline], types: [completed], branches: [main]) and, in hostkeeper-desired.yml, the setting of hostkeeper's desired version after the release (workflows: [release])";

/// The filters of the allowed shape, by key, following `workflow`.
fn allowed(workflow: &str) -> [(&'static str, String); 3] {
    [
        ("branches", "main".to_owned()),
        ("types", "completed".to_owned()),
        ("workflows", workflow.to_owned()),
    ]
}

pub(super) fn is_workflow_run(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case("workflow_run")
}

/// Whether the workflow's `workflow_run` filters are exactly [`allowed`] after the workflow
/// `followed`.
fn follows_exactly(text: &str, followed: &str) -> bool {
    let Ok(mut filters) = trigger_filters(text, "workflow_run") else {
        return false;
    };
    filters.sort();
    let want = allowed(followed);
    filters.len() == want.len()
        && filters
            .iter()
            .zip(want)
            .all(|((key, values), (want_key, want))| key == want_key && values.as_slice() == [want])
}

/// Whether the workflow's `workflow_run` trigger is the pipeline's shape, or the release's in the
/// workflow named `hostkeeper-desired`.
fn is_the_allowed_shape(text: &str) -> bool {
    follows_exactly(text, "pipeline")
        || (yaml::load(text).is_ok_and(|root| hostkeeper::is_named(&root))
            && follows_exactly(text, "release"))
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

/// The commands of the retired offer and gamma path: `gamma.yml` was their only caller. No
/// workflow may run them again. They are matched on normalised text ([`normalised`]), so a
/// command is found however its words are spaced, wrapped or separated by a lone `--`.
#[cfg(test)]
const RETIRED_VERBS: &[&str] = &["release stage-offer", "xtask gamma", "kbf-installer deploy"];

/// Workflow files that stay deleted.
#[cfg(test)]
const RETIRED_FILES: &[&str] = &[
    "gamma.yml",
    "gamma.yaml",
    "deploy-gamma.yml",
    "deploy-gamma.yaml",
];

/// `text` with every run of white space (a line wrap, a `\` continuation, tabs) as one space
/// and a lone `--` between words dropped, so `xtask -- gamma`, `xtask  gamma` and
/// `cargo run -p xtask -- gamma` all read `xtask gamma`.
#[cfg(test)]
fn normalised(text: &str) -> String {
    let words: Vec<&str> = text
        .split_whitespace()
        .filter(|w| *w != "\\" && *w != "--")
        .collect();
    words.join(" ")
}

/// Catches: a workflow (a re-added `gamma.yml`, or a job in another file) that runs the retired
/// offer or gamma path, which a release now fires no part of.
#[cfg(test)]
fn retired_verbs_in(name: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    if RETIRED_FILES.contains(&name) {
        found.push(format!("{name} is deleted: gamma moves to kbf-updater"));
    }
    let text = normalised(text);
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
    fn only_the_release_the_rerun_and_the_hostkeeper_workflows_follow_another() {
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
        followers.sort();
        assert_eq!(
            followers,
            [
                "hostkeeper-desired.yml",
                "pipeline-rerun.yml",
                "release.yml"
            ],
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
    fn only_the_hostkeeper_workflow_is_named_so_and_follows_the_release() {
        // Catches: the shape of `workflows: [release]` allowed for a file that is not
        // `hostkeeper-desired.yml` (the lint knows the workflow by its name, so the name is held
        // to the file here), and `hostkeeper-desired.yml` following another workflow.
        let dir = crate::repo_root().join(".github/workflows");
        let mut named = Vec::new();
        let mut follows_release = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("workflow dir") {
            let path = entry.expect("entry").path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !(name.ends_with(".yml") || name.ends_with(".yaml")) {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("workflow text");
            if name_of(&text) == hostkeeper::NAME {
                named.push(name.clone());
            }
            if follows_exactly(&text, "release") {
                follows_release.push(name);
            }
        }
        assert_eq!(named, ["hostkeeper-desired.yml"], "the workflow named so");
        assert_eq!(
            follows_release,
            ["hostkeeper-desired.yml"],
            "the workflows that follow the release"
        );
    }

    #[test]
    fn the_release_is_followed_only_by_the_hostkeeper_workflow() {
        // Catches: a workflow that follows the release without being the one that sets
        // hostkeeper's desired version, and that one following the pipeline or the release in
        // another shape.
        let file = |name: &str, body: &str| {
            format!("name: {name}\non:\n  workflow_run:\n{body}\npermissions: {{}}\njobs: {{}}\n")
        };
        let release = "    workflows: [release]\n    types: [completed]\n    branches: [main]";
        assert!(rules(&file("hostkeeper-desired", release)).is_empty());
        assert_eq!(rules(&file("other", release)).len(), 1);
        assert_eq!(rules(&file("hostkeeper-desired-2", release)).len(), 1);
        for bad in [
            "    workflows: [release, pipeline]\n    types: [completed]\n    branches: [main]",
            "    workflows: [release]\n    types: [requested]\n    branches: [main]",
            "    workflows: [release]\n    types: [completed]\n    branches: ['**']",
            "    workflows: [release]\n    types: [completed]",
            "    workflows: [ci]\n    types: [completed]\n    branches: [main]",
        ] {
            assert_eq!(rules(&file("hostkeeper-desired", bad)).len(), 1, "{bad}");
        }
    }

    #[test]
    fn a_job_that_runs_stage_offer_is_refused() {
        let job = "  offer:\n    steps:\n      - run: cargo xtask release stage-offer --x\n";
        assert_eq!(retired_verbs_in("ci.yml", job).len(), 1);
        assert_eq!(retired_verbs_in("gamma.yml", "name: gamma\n").len(), 1);
        assert!(retired_verbs_in("ci.yml", "name: ci\n").is_empty());
    }

    #[test]
    fn a_retired_command_is_found_however_it_is_spelled() {
        for run in [
            "cargo xtask gamma deploy",
            "cargo xtask -- gamma deploy",
            "cargo run -p xtask -- gamma evidence",
            "cargo   run -p xtask --   gamma  held-steps",
            "xtask\tgamma summary",
            "cargo xtask \\\n    gamma deploy",
            "cargo run -p xtask --\n  gamma deploy",
            "kbf-installer deploy --commit abc",
            "sudo kbf-installer   deploy",
            "kbf-installer \\\n  deploy --commit abc",
            "cargo xtask release   stage-offer",
        ] {
            let job = format!("    steps:\n      - run: {run}\n");
            assert_eq!(retired_verbs_in("ci.yml", &job).len(), 1, "{run:?}");
        }
    }

    #[test]
    fn a_retired_workflow_file_is_refused_by_name() {
        for name in [
            "gamma.yml",
            "gamma.yaml",
            "deploy-gamma.yml",
            "deploy-gamma.yaml",
        ] {
            assert_eq!(retired_verbs_in(name, "name: x\n").len(), 1, "{name}");
        }
        for name in ["ci.yml", "beta.yaml", "gamma-notes.yml"] {
            assert!(retired_verbs_in(name, "name: x\n").is_empty(), "{name}");
        }
    }

    #[test]
    fn commands_that_only_look_like_the_retired_ones_are_not_refused() {
        for run in [
            "cargo xtask release plan",
            "kbf-installer status",
            "kbf-installer setup --mode=daemon",
            "echo kbf-deploy host install",
        ] {
            let job = format!("      - run: {run}\n");
            assert!(retired_verbs_in("ci.yml", &job).is_empty(), "{run:?}");
        }
    }
}
