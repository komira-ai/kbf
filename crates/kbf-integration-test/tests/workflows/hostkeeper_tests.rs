//! Tests of the rules for the workflow that sets hostkeeper's desired version
//! ([`super::hostkeeper`]) and for the apt jobs of the release that hold `id-token: write`
//! ([`super::release_ifs`]).

use super::*;

const PATH: &str = ".github/workflows/hostkeeper-desired.yml";
const CALL: &str = "komira-ai/hostkeeper/.github/workflows/set-desired.yml@318bcc93c9bcd009d40597c45227278742acc622 # v0.1.242";

fn real() -> String {
    std::fs::read_to_string(crate::repo_root().join(PATH)).expect(PATH)
}

fn problems(text: &str) -> Vec<String> {
    scan(text, &Repo::default())
}

/// The problems of the real workflow with `from` (which it holds once) replaced by `to`.
fn plant(from: &str, to: &str) -> Vec<String> {
    let text = real();
    assert_eq!(text.matches(from).count(), 1, "`{from}` is not unique");
    problems(&text.replace(from, to))
}

fn red(found: &[String], want: &str) {
    assert!(
        found.iter().any(|p| p.contains(want)),
        "want a problem with `{want}`: {found:#?}"
    );
}

#[test]
fn the_hostkeeper_workflow_as_written_is_accepted() {
    // Catches: the one workflow that follows the release, calls hostkeeper's reusable workflow
    // and holds `id-token: write` in its `set-desired` job being refused by the lint, and the
    // control for the plants below.
    assert_eq!(problems(&real()), Vec::<String>::new());
    assert!(real().contains(&format!("uses: {CALL}")));
    assert!(real().contains(&format!("if: {}", hostkeeper::SET_IF)));
}

#[test]
fn another_reusable_workflow_is_refused() {
    // Catches: a call to another repository's workflow, or to another workflow of hostkeeper's
    // repository, in the workflow that may call `set-desired.yml` alone.
    red(
        &plant(
            "komira-ai/hostkeeper/.github/workflows/set-desired.yml@",
            "someone/else/.github/workflows/set-desired.yml@",
        ),
        "is not `komira-ai/hostkeeper/.github/workflows/set-desired.yml` pinned",
    );
    red(
        &plant(
            "komira-ai/hostkeeper/.github/workflows/set-desired.yml@",
            "komira-ai/hostkeeper/.github/workflows/other.yml@",
        ),
        "is not `komira-ai/hostkeeper/.github/workflows/set-desired.yml` pinned",
    );
}

#[test]
fn the_call_is_pinned_by_full_sha_with_a_version_comment() {
    // Catches: the same path called by a tag or branch, by an abbreviated SHA, or with no
    // version comment.
    for pin in [
        "@v0.1.242 # v0.1.242",
        "@main # v0.1.242",
        "@318bcc93c9bcd009d40597c45227278742acc6 # v0.1.242",
        "@318BCC93C9BCD009D40597C45227278742ACC622 # v0.1.242",
    ] {
        let to = format!("komira-ai/hostkeeper/.github/workflows/set-desired.yml{pin}");
        red(&plant(CALL, &to), "pinned to a full commit SHA");
    }
    for comment in ["", " # latest", " # 0.1.242", " # v0.1"] {
        let to = CALL.replace(" # v0.1.242", comment);
        red(
            &plant(CALL, &to),
            "needs a plain scalar and a version comment",
        );
    }
}

#[test]
fn the_call_is_allowed_in_that_workflow_and_that_job_only() {
    // Catches: the call in a workflow of another name (the lint knows the workflow by its
    // name, which a repository test holds to the file), and in another job of that workflow.
    red(
        &plant("name: hostkeeper-desired\n", "name: other\n"),
        "a reusable workflow outside this repository",
    );
    red(
        &plant("  set-desired:\n", "  call:\n"),
        "a reusable workflow outside this repository",
    );
}

#[test]
fn id_token_is_held_by_set_desired_alone_under_its_exact_condition() {
    // Catches: `id-token: write` in the `check` job (which runs this repository's code), in
    // `set-desired` under another condition, with the switch dropped, and in a workflow of another
    // name under the same condition.
    red(
        &plant(
            "      contents: read\n      actions: read\n",
            "      contents: read\n      id-token: write\n",
        ),
        "`id-token: write` is allowed only in a job whose `if:` is exactly",
    );
    red(
        &plant(
            hostkeeper::SET_IF,
            "${{ github.ref == 'refs/heads/main' && needs.check.outputs.set == 'true' }}",
        ),
        "`id-token: write` is allowed only in a job whose `if:` is exactly",
    );
    red(
        &plant(
            hostkeeper::SET_IF,
            "${{ vars.KBF_SETS_DESIRED == 'true' && needs.check.outputs.set == 'true' }}",
        ),
        "`id-token: write` is allowed only in a job whose `if:` is exactly",
    );
    red(
        &plant("name: hostkeeper-desired\n", "name: other\n"),
        "`id-token: write` is allowed only in a job whose `if:` is exactly",
    );
    red(
        &plant("  set-desired:\n", "  call:\n"),
        "`id-token: write` is allowed only in a job whose `if:` is exactly",
    );
}

/// A workflow with one job that holds `id-token: write` under `condition`.
fn release_like(condition: &str) -> String {
    format!(
        "name: release\non: push\npermissions: {{}}\njobs:\n  a:\n    if: {condition}\n    runs-on: ubuntu-24.04\n    permissions:\n      contents: read\n      id-token: write\n    steps:\n      - run: true\n"
    )
}

#[test]
fn the_apt_read_and_write_jobs_may_hold_id_token_under_their_exact_conditions() {
    // Catches: an apt job's condition widened or narrowed (a clause dropped from a retry
    // attempt, a promotion that does not wait for both checks), and the token granted to a job
    // under the condition of one that does not hold it.
    assert_eq!(release_ifs::plain(), RELEASE_PUSH_ONLY);
    let all = release_ifs::id_token_conditions();
    assert_eq!(all.len(), 11, "{all:#?}");
    for condition in &all {
        assert_eq!(problems(&release_like(condition)), Vec::<String>::new());
    }
    for condition in &all {
        for clause in ["!cancelled() && !failure() && ", "needs.", " && needs.plan.outputs.code == 'true'"] {
            if condition.matches(clause).count() == 1 {
                let found = problems(&release_like(&condition.replacen(clause, "", 1)));
                red(&found, "`id-token: write` is allowed only in a job whose `if:` is exactly");
            }
        }
    }
    let skipped = release_ifs::condition("testing", release_ifs::Kind::Index, 2);
    red(
        &problems(&release_like(&skipped)),
        "`id-token: write` is allowed only in a job whose `if:` is exactly",
    );
    let check = release_ifs::check_condition();
    red(
        &problems(&release_like(&check)),
        "`id-token: write` is allowed only in a job whose `if:` is exactly",
    );
}
