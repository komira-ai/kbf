//! Tests of the rules for `farm.yml` ([`super`]): the one job that holds `id-token: write` on a
//! pull request, only under its exact `if:`, in the one workflow named `farm`. Each rule is shown
//! to refuse its defect by putting it into the committed workflow and reading the exact problems.

use super::{
    BUILD_PROBLEM, CONFIG_PROBLEM, EARLY_BUILD_PROBLEM, IF_PROBLEM, INPUTS_PROBLEM, JOB_IF, NAME,
    ON_PROBLEM, ONE_JOIN_PROBLEM, PERMISSIONS_PROBLEM, PIN_PROBLEM, RUNNER_PROBLEM,
    SECRETS_PROBLEM, TOP_PERMISSIONS_PROBLEM, problems,
};
use crate::workflows::{Repo, scan, yaml};

/// The file's path below the repository root.
const PATH: &str = ".github/workflows/farm.yml";

/// What the general workflow lint says to a token held under another `if:` or elsewhere.
const GENERIC: &str = "`id-token: write` is allowed only in a job whose `if:` is exactly";

fn real() -> String {
    std::fs::read_to_string(crate::repo_root().join(PATH)).expect(PATH)
}

/// The committed workflow with its one occurrence of `from` replaced by `to`.
fn with(from: &str, to: &str) -> String {
    let text = real();
    assert_eq!(text.matches(from).count(), 1, "{from:?} once in farm.yml");
    text.replace(from, to)
}

/// The general lint's problems with `text`.
fn lint(text: &str) -> Vec<String> {
    scan(text, &Repo::default())
}

/// Asserts that the general lint refuses `text` with a token problem.
fn token_refused(text: &str) {
    let found = lint(text);
    assert!(
        found.iter().any(|p| p.contains(GENERIC)),
        "want `{GENERIC}`: {found:#?}"
    );
}

/// The `uses:` line of the join step in the committed file, without its indent.
fn join_line() -> String {
    real()
        .lines()
        .find(|l| l.contains("uses: tailscale/github-action@"))
        .expect("the join step")
        .trim()
        .to_owned()
}

#[test]
fn the_committed_farm_yml_passes_every_rule() {
    // Catches: rules that refuse the file they were written for, or a general lint that refuses
    // the one job it must allow.
    assert_eq!(problems(&real()), Vec::<String>::new());
    assert_eq!(lint(&real()), Vec::<String>::new());
}

#[test]
fn only_farm_yml_is_named_farm() {
    // Catches: the exception, which is keyed to the name `farm`, reachable from a workflow of
    // another file.
    let dir = crate::repo_root().join(".github/workflows");
    let mut named = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("workflow dir") {
        let path = entry.expect("entry").path();
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        if !(file.ends_with(".yml") || file.ends_with(".yaml")) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("workflow text");
        if yaml::load(&text).is_ok_and(|root| super::is_named(&root)) {
            named.push(file);
        }
    }
    assert_eq!(named, ["farm.yml"], "the workflows named {NAME}");
}

#[test]
fn the_token_is_held_only_under_the_jobs_exact_if() {
    // Catches: the job's `if:` loosened (a fork's pull request let in, Dependabot's branches let
    // in, the condition dropped or always true) while it still holds `id-token: write`.
    let line = format!("    if: {JOB_IF}\n");
    let none = with(&line, "");
    assert_eq!(problems(&none), [IF_PROBLEM]);
    token_refused(&none);
    for condition in [
        "${{ github.event_name == 'push' || github.event_name == 'pull_request' }}",
        "${{ github.event_name == 'push' || github.event.pull_request.head.repo.full_name == github.repository }}",
        "${{ always() }}",
    ] {
        let text = with(JOB_IF, condition);
        assert_eq!(problems(&text), [IF_PROBLEM], "{condition}");
        token_refused(&text);
    }
}

#[test]
fn the_same_if_in_another_workflow_or_job_is_refused() {
    // Catches: the exception reached from a workflow of another name, from a renamed job, or from
    // a second job added beside `build` under the same `if:`.
    let other = with("name: farm\n", "name: other\n");
    token_refused(&other);
    let renamed = with("\njobs:\n  build:\n", "\njobs:\n  lane:\n");
    assert_eq!(
        problems(&renamed),
        [
            "farm.yml: no job build".to_owned(),
            "job lane is not a job of farm.yml".to_owned(),
        ]
    );
    token_refused(&renamed);
    let second = with(
        "\njobs:\n",
        &format!(
            "\njobs:\n  other:\n    if: {JOB_IF}\n    runs-on: ubuntu-24.04\n    permissions:\n      id-token: write\n    steps:\n      - run: true\n"
        ),
    );
    assert_eq!(problems(&second), ["job other is not a job of farm.yml"]);
    token_refused(&second);
}

#[test]
fn no_other_scope_is_held_by_the_job_or_the_file() {
    // Catches: a second scope beside the token, the token's companion widened, every scope granted
    // at once, the token dropped, and a scope given to every job at the top of the file.
    let wide = [
        ("      contents: read\n", "      contents: write\n"),
        (
            "      contents: read\n",
            "      contents: read\n      packages: read\n",
        ),
        ("      contents: read\n", ""),
    ];
    for (from, to) in wide {
        assert_eq!(problems(&with(from, to)), [PERMISSIONS_PROBLEM], "{to:?}");
    }
    assert_eq!(
        problems(&with(
            "    permissions:\n      contents: read\n      # For the tailnet join alone: the OIDC token the farm's network trusts.\n      id-token: write\n",
            "    permissions: write-all\n",
        )),
        [PERMISSIONS_PROBLEM]
    );
    assert_eq!(
        problems(&with(
            "\npermissions: {}\n",
            "\npermissions:\n  contents: read\n"
        )),
        [TOP_PERMISSIONS_PROBLEM]
    );
}

#[test]
fn the_triggers_are_a_pull_request_and_a_push_to_main_alone() {
    // Catches: the lane started in the merge queue, by a manual run, by a filtered or widened pull
    // request, or by a push to any other branch or a tag.
    let on = "on:\n  pull_request:\n  push:\n    branches: [main]\n";
    for changed in [
        "on:\n  merge_group:\n  pull_request:\n  push:\n    branches: [main]\n",
        "on:\n  workflow_dispatch:\n  pull_request:\n  push:\n    branches: [main]\n",
        "on:\n  pull_request:\n    branches: [main]\n  push:\n    branches: [main]\n",
        "on:\n  pull_request:\n  push:\n    branches: [main, dev]\n",
        "on:\n  pull_request:\n  push:\n    branches: [main]\n    tags: ['v*']\n",
        "on:\n  pull_request:\n  push:\n",
        "on:\n  push:\n    branches: [main]\n",
        "on: pull_request\n",
    ] {
        assert_eq!(problems(&with(on, changed)), [ON_PROBLEM], "{changed}");
    }
}

#[test]
fn the_job_runs_on_the_one_hosted_runner() {
    // Catches: the job moved to another runner label.
    for runner in ["ubuntu-latest", "macos-15", "ubuntu-24.04-arm"] {
        assert_eq!(
            problems(&with(
                "runs-on: ubuntu-24.04\n",
                &format!("runs-on: {runner}\n")
            )),
            [RUNNER_PROBLEM],
            "{runner}"
        );
    }
}

#[test]
fn the_join_is_pinned_by_a_full_lowercase_commit() {
    // Catches: the action named by a branch or tag, an abbreviated SHA or capitals.
    let line = join_line();
    let (action, _) = line.split_once('@').expect("pinned");
    let sha = &line["uses: tailscale/github-action@".len()..][..40];
    for pin in [
        "main".to_owned(),
        "v4.2.0".to_owned(),
        sha[..12].to_owned(),
        sha.to_ascii_uppercase(),
    ] {
        let to = format!("{action}@{pin} # v4.2.0");
        assert_eq!(problems(&with(&line, &to)), [PIN_PROBLEM], "{pin}");
    }
}

#[test]
fn exactly_one_step_joins_the_network() {
    // Catches: a second join step, and the join removed.
    let text = real();
    let step = text
        .split("      - name: Join the farm's Tailscale network\n")
        .nth(1)
        .and_then(|rest| rest.split("      # One command").next())
        .expect("the join step");
    let step = format!("      - name: Join the farm's Tailscale network\n{step}");
    assert_eq!(
        problems(&with(&step, &format!("{step}{step}"))),
        [ONE_JOIN_PROBLEM]
    );
    // With no join, no step comes after it: the config and the build are missing too.
    let after = [ONE_JOIN_PROBLEM, CONFIG_PROBLEM, BUILD_PROBLEM];
    assert_eq!(problems(&with(&step, "")), after);
    // An action whose name only starts like the join's is not the join.
    let look_alike = join_line().replace("tailscale/github-action@", "tailscale/github-actionx@");
    assert_eq!(problems(&with(&join_line(), &look_alike)), after);
}

#[test]
fn a_file_that_is_not_a_workflow_mapping_is_refused() {
    // Catches: a file that does not parse, or whose root is not a mapping, read as a farm.yml with
    // nothing wrong, or the name check accepting a root that is not a mapping.
    let broken = "a: [";
    let why = yaml::load(broken).expect_err("does not parse");
    assert_eq!(problems(broken), [why]);
    assert_eq!(problems("- x\n"), [ON_PROBLEM]);
    assert!(!super::is_named(&yaml::load("- farm\n").expect("a list")));
}

#[test]
fn push_must_name_main_as_a_list() {
    // Catches: `branches: main` (a scalar) read as the list `[main]`.
    assert_eq!(
        problems(&with("branches: [main]", "branches: main")),
        [ON_PROBLEM]
    );
}

#[test]
fn the_join_takes_exactly_its_inputs() {
    // Catches: the tailnet identity from a secret or a literal, another tag, a missing input and
    // an extra one.
    for (from, to) in [
        ("          tags: tag:ci\n", "          tags: tag:prod\n"),
        (
            "          tags: tag:ci\n",
            "          tags: tag:ci\n          ephemeral: false\n",
        ),
        ("          audience: ${{ vars.TS_AUDIENCE }}\n", ""),
        (
            "          oauth-client-id: ${{ vars.TS_CLIENT_ID }}\n",
            "          oauth-client-id: abc\n",
        ),
    ] {
        assert_eq!(problems(&with(from, to)), [INPUTS_PROBLEM], "{to:?}");
    }
    let secret = problems(&with(
        "oauth-client-id: ${{ vars.TS_CLIENT_ID }}",
        "oauth-client-id: ${{ secrets.TS_CLIENT_ID }}",
    ));
    assert_eq!(secret, [INPUTS_PROBLEM, SECRETS_PROBLEM]);
}

#[test]
fn the_build_comes_after_the_join_and_the_config_and_names_both_targets() {
    // Catches: no build step, a build of another target or of only one target, a build with no
    // farm-config before it, and a build that runs before the join.
    let build = "run: cargo xtask farm-build --buck2";
    assert_eq!(
        problems(&with(build, "run: cargo xtask other --buck2")),
        [BUILD_PROBLEM]
    );
    assert_eq!(
        problems(&with("--target //:ci ", "--target //:other ")),
        [BUILD_PROBLEM]
    );
    assert_eq!(
        problems(&with("--target //:sync_check ", "")),
        [BUILD_PROBLEM]
    );
    assert_eq!(
        problems(&with(
            "run: cargo xtask farm-config",
            "run: cargo xtask other"
        )),
        [CONFIG_PROBLEM, BUILD_PROBLEM]
    );
    let early = with(
        "      - name: Join the farm's Tailscale network\n",
        "      - run: cargo xtask farm-build --target //:ci --target //:sync_check\n      - name: Join the farm's Tailscale network\n",
    );
    assert_eq!(problems(&early), [EARLY_BUILD_PROBLEM]);
}

#[test]
fn the_job_names_no_secret() {
    // Catches: a secret passed to any step of the job, as an environment variable.
    let text = with(
        "      - run: rustup toolchain install\n",
        "      - run: rustup toolchain install\n        env:\n          T: ${{ secrets.T }}\n",
    );
    assert_eq!(problems(&text), [SECRETS_PROBLEM]);
}
