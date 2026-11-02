//! Tests of the gamma CI/CD job-table lint. The real doc and workflow are the control (no
//! problem); each other test spoils one copy and asserts the exact problem it reports.

use super::*;

fn read(path: &str) -> String {
    std::fs::read_to_string(crate::repo_root().join(path)).expect(path)
}

/// A small workflow with three jobs, for the shape tests.
const WORKFLOW: &str = "\
name: release
on:
  workflow_run:
    workflows: [pipeline]
    types: [completed]
permissions: {}
jobs:
  plan:
    runs-on: ubuntu-24.04
    steps:
      - run: true
  publish:
    needs: [plan]
    runs-on: ubuntu-24.04
    steps:
      - run: true
  gamma:
    needs: [publish]
    runs-on: ubuntu-24.04
    steps:
      - run: true
";

/// A doc whose job table has one row per id of `jobs`.
fn doc(jobs: &[&str]) -> String {
    let mut d = format!("# Title\n\nProse.\n\n{BEGIN}\n| job | what it gates |\n|---|---|\n");
    for j in jobs {
        d.push_str(&format!("| `{j}` | something |\n"));
    }
    d.push_str(&format!("{END}\n\nMore prose.\n"));
    d
}

#[test]
fn the_real_doc_lists_exactly_the_release_jobs() {
    assert_eq!(problems(&read(DOC), &read(RELEASE)), Vec::<String>::new());
}

#[test]
fn a_matching_doc_passes() {
    assert_eq!(
        problems(&doc(&["plan", "publish", "gamma"]), WORKFLOW),
        Vec::<String>::new()
    );
}

#[test]
fn a_job_in_another_order_still_passes() {
    assert_eq!(
        problems(&doc(&["gamma", "plan", "publish"]), WORKFLOW),
        Vec::<String>::new()
    );
}

#[test]
fn a_wrong_job_name_in_the_doc_fails() {
    // `deploy` is not a job of the workflow, and `gamma` is then missing.
    assert_eq!(
        problems(&doc(&["plan", "publish", "deploy"]), WORKFLOW),
        vec![
            format!("{DOC}: the job table names job `deploy`, which {RELEASE} does not have"),
            format!("{DOC}: {RELEASE} has job `gamma`, which the job table omits"),
        ]
    );
}

#[test]
fn the_real_doc_with_a_job_renamed_fails() {
    let real = read(DOC);
    let spoiled = real.replacen("| `rehearse-agent` |", "| `rehearse-agents` |", 1);
    assert_ne!(spoiled, real, "the real doc has a `rehearse-agent` row");
    assert_eq!(
        problems(&spoiled, &read(RELEASE)),
        vec![
            format!(
                "{DOC}: the job table names job `rehearse-agents`, which {RELEASE} does not have"
            ),
            format!("{DOC}: {RELEASE} has job `rehearse-agent`, which the job table omits"),
        ]
    );
}

#[test]
fn a_workflow_job_the_doc_omits_fails() {
    assert_eq!(
        problems(&doc(&["plan", "publish"]), WORKFLOW),
        vec![format!(
            "{DOC}: {RELEASE} has job `gamma`, which the job table omits"
        )]
    );
}

#[test]
fn a_job_added_to_the_real_workflow_fails() {
    let real = read(RELEASE);
    let spoiled = real.replacen(
        "\njobs:\n",
        "\njobs:\n  prod:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: true\n",
        1,
    );
    assert_ne!(spoiled, real, "release.yml has a `jobs:` line");
    assert_eq!(
        problems(&read(DOC), &spoiled),
        vec![format!(
            "{DOC}: {RELEASE} has job `prod`, which the job table omits"
        )]
    );
}

#[test]
fn a_job_listed_twice_fails() {
    assert_eq!(
        problems(&doc(&["plan", "publish", "gamma", "plan"]), WORKFLOW),
        vec![format!("{DOC}: job `plan` is listed twice")]
    );
}

#[test]
fn an_empty_list_fails() {
    assert_eq!(
        problems(&doc(&[]), WORKFLOW),
        vec![
            format!("{DOC}: the job table has no rows"),
            format!("{DOC}: {RELEASE} has job `plan`, which the job table omits"),
            format!("{DOC}: {RELEASE} has job `publish`, which the job table omits"),
            format!("{DOC}: {RELEASE} has job `gamma`, which the job table omits"),
        ]
    );
}

#[test]
fn a_row_without_a_backticked_job_fails() {
    let d = doc(&["plan", "publish", "gamma"]).replace("| `publish` |", "| publish |");
    // Line 9: title, blank, prose, blank, begin, header, separator, plan, publish.
    assert_eq!(
        problems(&d, WORKFLOW),
        vec![format!(
            "{DOC}:9: a job row must start with one job id in backticks, not \"publish\""
        )]
    );
}

#[test]
fn a_row_with_two_ids_in_its_first_cell_fails() {
    let d = doc(&["plan", "publish", "gamma"]).replace("| `publish` |", "| `publish` `gamma` |");
    assert_eq!(
        problems(&d, WORKFLOW),
        vec![format!(
            "{DOC}:9: a job row must start with one job id in backticks, not \"`publish` `gamma`\""
        )]
    );
}

#[test]
fn an_empty_backticked_cell_fails() {
    let d = doc(&["plan", "publish", "gamma"]).replace("| `publish` |", "| `` |");
    assert_eq!(
        problems(&d, WORKFLOW),
        vec![format!(
            "{DOC}:9: a job row must start with one job id in backticks, not \"``\""
        )]
    );
}

#[test]
fn rows_outside_the_markers_are_not_read() {
    let d = format!(
        "| `deploy` | outside |\n{}| `prod` | after |\n",
        doc(&["plan", "publish", "gamma"])
    );
    assert_eq!(problems(&d, WORKFLOW), Vec::<String>::new());
}

#[test]
fn missing_markers_fail() {
    let d = doc(&["plan", "publish", "gamma"]).replace(BEGIN, "");
    assert_eq!(
        problems(&d, WORKFLOW),
        vec![format!(
            "{DOC}: the job table must sit between one `{BEGIN}` line and one `{END}` line (found 0 and 1)"
        )]
    );
}

#[test]
fn a_repeated_marker_fails() {
    let d = format!("{}{END}\n", doc(&["plan", "publish", "gamma"]));
    assert_eq!(
        problems(&d, WORKFLOW),
        vec![format!(
            "{DOC}: the job table must sit between one `{BEGIN}` line and one `{END}` line (found 1 and 2)"
        )]
    );
}

#[test]
fn markers_in_the_wrong_order_fail() {
    let d = doc(&["plan", "publish", "gamma"])
        .replace(BEGIN, "@@B@@")
        .replace(END, BEGIN)
        .replace("@@B@@", END);
    assert_eq!(
        problems(&d, WORKFLOW),
        vec![format!("{DOC}: `{END}` comes before `{BEGIN}`")]
    );
}

#[test]
fn a_workflow_that_does_not_parse_fails() {
    let bad = "jobs: [unclosed\n";
    let parser = workflows::job_names(bad).expect_err("the text does not parse");
    assert_eq!(
        problems(&doc(&["plan"]), bad),
        vec![format!("{RELEASE}: {parser}")]
    );
}

#[test]
fn a_workflow_without_jobs_fails() {
    assert_eq!(
        problems(&doc(&["plan"]), "name: release\npermissions: {}\n"),
        vec![format!("{RELEASE}: the workflow has no `jobs` mapping")]
    );
}

#[test]
fn a_missing_end_marker_fails() {
    let d = doc(&["plan", "publish", "gamma"]).replace(END, "");
    assert_eq!(
        problems(&d, WORKFLOW),
        vec![format!(
            "{DOC}: the job table must sit between one `{BEGIN}` line and one `{END}` line (found 1 and 0)"
        )]
    );
}

#[test]
fn markers_with_surrounding_spaces_are_read() {
    let d = doc(&["plan", "publish", "gamma"])
        .replace(BEGIN, &format!("  {BEGIN} "))
        .replace(END, &format!("\t{END}"));
    assert_eq!(problems(&d, WORKFLOW), Vec::<String>::new());
}

#[test]
fn prose_and_blank_lines_between_the_markers_are_not_rows() {
    let d = doc(&["plan", "publish", "gamma"]).replace(
        "| `publish` |",
        "\nA sentence about `deploy`, not a row.\n\n| `publish` |",
    );
    assert_eq!(problems(&d, WORKFLOW), Vec::<String>::new());
}

#[test]
fn an_aligned_separator_row_is_not_a_job() {
    let d = doc(&["plan", "publish", "gamma"]).replace("|---|---|", "|:--:|---:|");
    assert_eq!(problems(&d, WORKFLOW), Vec::<String>::new());
}

#[test]
fn an_empty_first_cell_fails() {
    let d = doc(&["plan", "publish", "gamma"]).replace("| `publish` |", "| |");
    assert_eq!(
        problems(&d, WORKFLOW),
        vec![format!(
            "{DOC}:9: a job row must start with one job id in backticks, not \"\""
        )]
    );
}

#[test]
fn a_backticked_cell_with_a_space_fails() {
    let d = doc(&["plan", "publish", "gamma"]).replace("| `publish` |", "| `pub lish` |");
    assert_eq!(
        problems(&d, WORKFLOW),
        vec![format!(
            "{DOC}:9: a job row must start with one job id in backticks, not \"`pub lish`\""
        )]
    );
}

#[test]
fn every_bad_row_is_reported_and_no_job_check_runs() {
    let d = doc(&["plan", "publish", "gamma"])
        .replace("| `plan` |", "| plan |")
        .replace("| `gamma` |", "| gamma |");
    assert_eq!(
        problems(&d, WORKFLOW),
        vec![
            format!("{DOC}:8: a job row must start with one job id in backticks, not \"plan\""),
            format!("{DOC}:10: a job row must start with one job id in backticks, not \"gamma\""),
        ]
    );
}

#[test]
fn an_unknown_job_and_a_repeated_one_are_both_reported() {
    assert_eq!(
        problems(
            &doc(&["plan", "publish", "gamma", "plan", "deploy"]),
            WORKFLOW
        ),
        vec![
            format!("{DOC}: job `plan` is listed twice"),
            format!("{DOC}: the job table names job `deploy`, which {RELEASE} does not have"),
        ]
    );
}

#[test]
fn a_second_header_row_is_not_a_job() {
    let d = doc(&["plan", "publish", "gamma"])
        .replace("| `publish` |", "| job | again |\n| `publish` |");
    assert_eq!(problems(&d, WORKFLOW), Vec::<String>::new());
}
