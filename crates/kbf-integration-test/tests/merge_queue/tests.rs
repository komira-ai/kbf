//! [`problems`] on the real files with one change made, so each rule refuses it, and [`expr`]
//! on the expressions it reads.

use super::expr::{Run, Val, condition, eval, flag, template};
use super::*;

/// The problems of the real workflows with the first `from` in the workflow `file` replaced by
/// `to`. The change must alter the text.
fn changed_to(file: &str, from: &str, to: &str) -> Vec<String> {
    let mut workflows = real_files();
    let text = &mut workflows
        .iter_mut()
        .find(|(n, _)| n == file)
        .unwrap_or_else(|| panic!("no {file}"))
        .1;
    let changed = text.replacen(from, to, 1);
    assert_ne!(&changed, text, "{file}: `{from}` is not in the file");
    *text = changed;
    problems(&workflows)
}

/// The problems of the real workflows with `job:`'s first key line preceded by `lines`.
fn job_gains(file: &str, job: &str, lines: &str) -> Vec<String> {
    let head = format!("\n  {job}:\n");
    changed_to(file, &head, &format!("{head}{lines}"))
}

/// `found` is exactly `want`.
fn is(found: &[String], want: &[&str]) {
    assert_eq!(found, want, "the problems found");
}

/// Some line of `found` holds `part`.
fn has_part(found: &[String], part: &str) {
    assert!(
        found.iter().any(|f| f.contains(part)),
        "want a line with {part:?} in {found:#?}"
    );
}

const QUEUE_IF: &str = "${{ github.event_name == 'pull_request' }}";

#[test]
fn a_required_check_whose_workflow_has_no_merge_group_trigger_is_refused() {
    // Catches: ci.yml losing `merge_group`, so `lint` and `test` never report for a queue entry.
    let found = changed_to("ci.yml", "  merge_group:\n", "");
    is(
        &found,
        &[
            "ci.yml does not run on `merge_group`, so the required check `lint` never reports \
             for a merge queue entry, which waits until the queue drops it",
            "ci.yml does not run on `merge_group`, so the required check `test` never reports \
             for a merge queue entry, which waits until the queue drops it",
        ],
    );
    let found = changed_to("macos-pr.yml", "  merge_group:\n", "");
    has_part(&found, "the required check `macos-device` never reports");
}

#[test]
fn a_filtered_merge_group_trigger_is_refused() {
    // Catches: a `types:` (or any filter) under `merge_group`, under which a run may not start.
    let found = changed_to(
        "ci.yml",
        "  merge_group:\n",
        "  merge_group:\n    types: [checks_requested]\n",
    );
    has_part(
        &found,
        "ci.yml filters its `merge_group` trigger ([(\"types\", [\"checks_requested\"])]): a run \
         that never starts leaves the required check `lint` unreported",
    );
}

#[test]
fn a_required_workflow_or_job_that_is_gone_or_renamed_is_refused() {
    // Catches: the workflow of a required check deleted, its job renamed, or the check given
    // another name by `name:` or a matrix.
    let mut workflows = real_files();
    workflows.retain(|(n, _)| n != "macos-pr.yml");
    is(
        &problems(&workflows),
        &[
            "there is no .github/workflows/macos-pr.yml, whose `macos-test` main requires",
            "there is no .github/workflows/macos-pr.yml, whose `macos-device` main requires",
        ],
    );
    let found = changed_to("ci.yml", "\n  lint:\n", "\n  lints:\n");
    is(&found, &["ci.yml has no job `lint`, which main requires"]);
    let found = job_gains("ci.yml", "lint", "    name: Lint\n");
    is(
        &found,
        &["ci.yml#lint reports as `Lint`, not as the required check `lint`"],
    );
    let found = job_gains(
        "ci.yml",
        "test",
        "    strategy:\n      matrix:\n        part: [1, 2]\n",
    );
    is(
        &found,
        &["ci.yml#test has a `strategy:`: a matrix names a check per entry, and none is `test`"],
    );
}

#[test]
fn a_required_check_a_queue_run_skips_is_refused() {
    // Catches: a required job whose `if:` is false in the queue, one whose `if:` the lint cannot
    // read, and one behind a need (a skipped job's check passes).
    let found = job_gains("ci.yml", "lint", &format!("    if: {QUEUE_IF}\n"));
    is(
        &found,
        &[format!(
            "ci.yml#lint is skipped in the merge queue (`if: {QUEUE_IF}`), and a skipped \
             required check passes"
        )
        .as_str()],
    );
    let found = job_gains("ci.yml", "lint", "    if: github.event_name == 'push'\n");
    has_part(&found, "ci.yml#lint is skipped in the merge queue");
    let found = job_gains(
        "ci.yml",
        "lint",
        "    if: ${{ contains(github.event.pull_request.title, 'x') }}\n",
    );
    is(
        &found,
        &[
            "ci.yml#lint: its `if: ${{ contains(github.event.pull_request.title, 'x') }}` cannot \
           be evaluated for a merge queue run: the function `contains`, which this lint does \
           not read",
        ],
    );
    let found = job_gains("macos-pr.yml", "macos-device", "    needs: [macos-test]\n");
    has_part(
        &found,
        "macos-pr.yml#macos-device needs [\"macos-test\"]: a failed or skipped need skips it, \
         and a skipped required check passes",
    );
}

#[test]
fn a_required_check_whose_if_holds_in_the_queue_passes() {
    // Catches: the lint refusing an `if:` that is true for a merge queue run (the boundary of
    // the rule above), written with and without `${{ }}`.
    for cond in [
        "${{ github.event_name != 'pull_request' }}",
        "${{ !cancelled() }}",
        "always()",
        "github.event_name == 'MERGE_GROUP'",
    ] {
        let found = job_gains("ci.yml", "lint", &format!("    if: {cond}\n"));
        is(&found, &[]);
    }
}

#[test]
fn a_queue_run_that_another_can_cancel_is_refused() {
    // Catches: artifacts.yml's queue runs back in one shared group (a newer entry's run cancels
    // the pending one), a cancel-in-progress that holds in the queue, and a group the lint
    // cannot evaluate.
    let found = changed_to(
        "artifacts.yml",
        "|| github.run_id }}",
        "|| github.event_name == 'merge_group' && 'merge-queue' || github.run_id }}",
    );
    is(
        &found,
        &[
            "artifacts.yml: every merge queue run shares the concurrency group \
           `artifacts-merge-queue`: GitHub keeps one pending run per group and cancels the one a \
           newer run replaces",
        ],
    );
    let found = changed_to(
        "ci.yml",
        &format!("  cancel-in-progress: {QUEUE_IF}\n"),
        "  cancel-in-progress: true\n",
    );
    is(
        &found,
        &["ci.yml: `cancel-in-progress: true` cancels a merge queue run in progress"],
    );
    let found = changed_to(
        "macos-pr.yml",
        &format!("  cancel-in-progress: {QUEUE_IF}\n"),
        "  cancel-in-progress: ${{ github.event_name != 'pull_request' }}\n",
    );
    has_part(
        &found,
        "macos-pr.yml: `cancel-in-progress: ${{ github.event_name != 'pull_request' }}` cancels a merge queue run in progress",
    );
    let found = changed_to(
        "ci.yml",
        "  group: ci-${{",
        "  group: ci-${{ github.workflow }}-${{",
    );
    has_part(
        &found,
        "ci.yml: its concurrency group `ci-${{ github.workflow }}-${{ github.event_name == \
         'pull_request' && github.ref || github.run_id }}` cannot be evaluated for a merge queue \
         run: `github.workflow`, which this lint does not model for a merge queue run",
    );
    let found = changed_to(
        "ci.yml",
        &format!("  cancel-in-progress: {QUEUE_IF}\n"),
        "  cancel-in-progress: maybe\n",
    );
    is(
        &found,
        &[
            "ci.yml: its `cancel-in-progress: maybe` cannot be evaluated for a merge queue run: \
           `maybe` is neither a boolean nor an expression",
        ],
    );
    let found = job_gains("ci.yml", "lint", "    concurrency: lint\n");
    is(
        &found,
        &[
            "ci.yml#lint has a `concurrency:` of its own, which this lint does not read for a \
           merge queue run",
        ],
    );
}

#[test]
fn a_queue_run_that_holds_a_secret_or_calls_a_workflow_is_refused() {
    // Catches: a secret in a job or the top-level `env:` of a workflow that runs in the queue,
    // a job calling a workflow the lint does not read, and the rules applied to a workflow that
    // gains `merge_group` (release.yml holds the release bot's key).
    let found = changed_to(
        "ci.yml",
        "      DENY_VERSION:",
        "      LEAK: ${{ secrets.TOKEN }}\n      DENY_VERSION:",
    );
    is(
        &found,
        &["ci.yml#lint names a secret, which a merge queue run would hold"],
    );
    let found = changed_to(
        "ci.yml",
        "env:\n  CARGO_TERM_COLOR: always\n",
        "env:\n  CARGO_TERM_COLOR: always\n  LEAK: ${{ secrets.TOKEN }}\n",
    );
    is(
        &found,
        &["ci.yml's top-level `env:` names a secret, which a merge queue run would hold"],
    );
    let found = changed_to(
        "macos-pr.yml",
        "\njobs:\n",
        "\njobs:\n  called:\n    uses: ./.github/workflows/beta.yml\n",
    );
    is(
        &found,
        &[
            "macos-pr.yml#called calls `./.github/workflows/beta.yml`, whose jobs this lint does \
           not read for a merge queue run",
        ],
    );
    let found = changed_to(
        "release.yml",
        "on:\n  workflow_run:\n",
        "on:\n  merge_group:\n  workflow_run:\n",
    );
    has_part(&found, "names a secret, which a merge queue run would hold");
}

/// A queue run for the expression tests.
const RUN: Run<'static> = ENTRIES[0];

#[test]
fn expressions_read_the_queue_run_contexts() {
    // Catches: a context of a queue run read wrongly (one shared value would pass a shared
    // group), or an unknown context read as empty instead of refused.
    let v = |s: &str| eval(s, &RUN);
    assert_eq!(v("github.event_name"), Ok(Val::Str("merge_group".into())));
    assert_eq!(v("GITHUB.REF"), Ok(Val::Str(ENTRIES[0].git_ref.into())));
    assert_eq!(v("github.run_id"), Ok(Val::Str("1001".into())));
    assert_eq!(v("github.sha"), Ok(Val::Str(ENTRIES[0].sha.into())));
    assert_eq!(v("github.base_ref"), Ok(Val::Str(String::new())));
    assert_eq!(v("github.event.pull_request.number"), Ok(Val::Null));
    assert_eq!(v("inputs.stage"), Ok(Val::Null));
    assert_eq!(
        v("secrets.X"),
        Err("`secrets.X`, which this lint does not model for a merge queue run".into())
    );
    assert_ne!(
        template("g-${{ github.ref }}", &ENTRIES[0]),
        template("g-${{ github.ref }}", &ENTRIES[1])
    );
}

#[test]
fn expressions_follow_githubs_operators() {
    // Catches: `&&`/`||` returning a boolean instead of an operand, an unknown context looked up
    // in an operand already decided, `==` comparing case or types as GitHub does not, and `!`.
    let v = |s: &str| eval(s, &RUN);
    assert_eq!(v("'a' && 'b'"), Ok(Val::Str("b".into())));
    assert_eq!(v("'' && 'b'"), Ok(Val::Str(String::new())));
    assert_eq!(v("'' || 'b'"), Ok(Val::Str("b".into())));
    assert_eq!(v("'a' || nope.x"), Ok(Val::Str("a".into())));
    assert_eq!(v("false && nope.x"), Ok(Val::Bool(false)));
    assert!(v("true && nope.x").is_err());
    assert_eq!(v("'Merge_Group' == github.event_name"), Ok(Val::Bool(true)));
    assert_eq!(v("'a' != 'A'"), Ok(Val::Bool(false)));
    assert_eq!(v("'' == 0"), Ok(Val::Bool(true)));
    assert_eq!(v("null == 0"), Ok(Val::Bool(true)));
    assert_eq!(v("' 12 ' == 12"), Ok(Val::Bool(true)));
    assert_eq!(v("true == 1"), Ok(Val::Bool(true)));
    assert_eq!(v("'x' == 0"), Ok(Val::Bool(false)));
    assert_eq!(v("!('x' == 0)"), Ok(Val::Bool(true)));
    assert_eq!(v("!!'x'"), Ok(Val::Bool(true)));
    assert_eq!(v("NULL"), Ok(Val::Null));
    assert_eq!(v("1.5"), Ok(Val::Num(1.5)));
    assert_eq!(v("'it''s'"), Ok(Val::Str("it's".into())));
    for falsy in [
        Val::Null,
        Val::Bool(false),
        Val::Num(0.0),
        Val::Num(f64::NAN),
    ] {
        assert!(!falsy.truthy(), "{falsy:?}");
    }
    assert!(Val::Num(2.0).truthy());
}

#[test]
fn expressions_call_the_functions_a_condition_or_group_uses() {
    // Catches: a status function read as false in a run nothing has failed in, and `format`
    // writing its arguments wrongly.
    let v = |s: &str| eval(s, &RUN);
    assert_eq!(v("always() && success()"), Ok(Val::Bool(true)));
    assert_eq!(v("failure() || cancelled()"), Ok(Val::Bool(false)));
    assert_eq!(
        v("format('{0}-{1}-{{x}}', 'a', github.run_id)"),
        Ok(Val::Str("a-1001-{x}".into()))
    );
    assert_eq!(v("format('{0}', null, true)"), Ok(Val::Str(String::new())));
    assert_eq!(v("format('{0}', true)"), Ok(Val::Str("true".into())));
    assert_eq!(v("format('{0}', 3)"), Ok(Val::Str("3".into())));
    assert_eq!(v("format('none')"), Ok(Val::Str("none".into())));
}

#[test]
fn an_expression_the_lint_does_not_read_is_refused() {
    // Catches: an expression outside the grammar evaluated as something, instead of named.
    let v = |s: &str| eval(s, &RUN);
    let refused = [
        ("always(1)", "`always` takes no argument"),
        ("format(1)", "`format` takes a string first"),
        ("format()", "`format` takes a string first"),
        ("format('{1}', 'a')", "`{1}` names no argument of `{1}`"),
        ("format('{x', 'a')", "an unclosed `{` in `{x`"),
        (
            "toJSON(github)",
            "the function `toJSON`, which this lint does not read",
        ),
        ("'abc", "an unterminated string in `'abc`"),
        ("1 < 2", "`<` in `1 < 2`, which this lint does not read"),
        ("1.2.3", "the number `1.2.3` in `1.2.3`"),
        ("(true", "`(true` is not a whole expression"),
        ("true true", "`true true` is not a whole expression"),
        (
            "format('a' 'b')",
            "`format('a' 'b')` is not a whole expression",
        ),
        (")", "`)` is not a whole expression"),
        ("", "`` ends where a value is expected"),
    ];
    for (src, want) in refused {
        assert_eq!(v(src), Err(want.to_owned()), "{src}");
    }
}

#[test]
fn templates_conditions_and_flags_take_their_written_forms() {
    // Catches: text around `${{ }}` dropped, a `}}` in a string read as the end, an unclosed
    // `${{` passed, a condition that mixes text and an expression read as true, and a
    // `cancel-in-progress` spelled other than GitHub reads it.
    assert_eq!(
        template("a-${{ 'x}}y' }}-${{ github.run_id }}", &RUN),
        Ok("a-x}}y-1001".to_owned())
    );
    assert_eq!(template("plain", &RUN), Ok("plain".to_owned()));
    assert_eq!(
        template("a-${{ 'x' ", &RUN),
        Err("an unclosed `${{` in `a-${{ 'x' `".to_owned())
    );
    assert_eq!(condition("  ${{ always() }} ", &RUN), Ok(true));
    assert_eq!(condition("github.ref == ''", &RUN), Ok(false));
    assert_eq!(
        condition("${{ true }} && false", &RUN),
        Err("`${{ true }} && false` mixes text and `${{ }}`".to_owned())
    );
    assert_eq!(flag("(absent)", &RUN), Ok(false));
    assert_eq!(flag("false", &RUN), Ok(false));
    assert_eq!(flag("true", &RUN), Ok(true));
    assert_eq!(
        flag("${{ github.event_name == 'merge_group' }}", &RUN),
        Ok(true)
    );
    assert_eq!(
        flag("yes", &RUN),
        Err("`yes` is neither a boolean nor an expression".to_owned())
    );
}
