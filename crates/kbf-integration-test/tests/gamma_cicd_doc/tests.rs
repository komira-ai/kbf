//! Tests of the gamma CI/CD job-table lint. The real doc and workflow are the control (no
//! problem); each other test spoils one copy and asserts the exact problem it reports.

use super::*;

fn read(path: &str) -> String {
    std::fs::read_to_string(crate::repo_root().join(path)).expect(path)
}

/// The jobs of the real `release.yml`, in file order.
fn real_jobs() -> Vec<String> {
    real_jobs_of(&read(RELEASE))
}

fn real_jobs_of(workflow: &str) -> Vec<String> {
    workflows::job_names(workflow).expect("the workflow parses")
}

/// `preferred`, or it with `-new` added until it names no job of `taken`.
fn fresh(taken: &[String], preferred: &str) -> String {
    let mut name = preferred.to_owned();
    while taken.contains(&name) {
        name.push_str("-new");
    }
    name
}

/// `doc` with `edit` applied to the text between the markers only.
fn in_table(doc: &str, edit: impl FnOnce(&str) -> String) -> String {
    let (head, rest) = doc.split_once(BEGIN).expect("the doc has the begin marker");
    let (table, tail) = rest.split_once(END).expect("the doc has the end marker");
    format!("{head}{BEGIN}{}{END}{tail}", edit(table))
}

/// `doc` with the job table's row of `from` naming `to`; the table has that row once.
fn rename_row(doc: &str, from: &str, to: &str) -> String {
    let row = format!("\n| `{from}` |");
    in_table(doc, |t| {
        assert_eq!(t.matches(&row).count(), 1, "one row of `{from}`");
        t.replacen(&row, &format!("\n| `{to}` |"), 1)
    })
}

/// `doc` without the job table's row of `job`.
fn remove_row(doc: &str, job: &str) -> String {
    let row = format!("| `{job}` |");
    in_table(doc, |t| {
        assert_eq!(
            t.matches(&format!("\n{row}")).count(),
            1,
            "one row of `{job}`"
        );
        t.split_inclusive('\n')
            .filter(|l| !l.starts_with(&row))
            .collect()
    })
}

/// The 1-based line the key of job `job` of `workflow` starts on, from the YAML parser, so
/// the helpers below do not depend on how the key or the `jobs:` line are spelt.
fn job_line(workflow: &str, job: &str) -> usize {
    let lines = workflows::job_lines(workflow).expect("the workflow parses");
    let at: Vec<usize> = lines
        .iter()
        .filter(|(n, _)| n == job)
        .map(|(_, l)| *l)
        .collect();
    assert_eq!(at.len(), 1, "one job `{job}`");
    at[0]
}

/// `workflow` with `edit` applied to its 1-based line `line` (without its line end).
fn edit_line(workflow: &str, line: usize, edit: impl FnOnce(&str) -> String) -> String {
    let mut edit = Some(edit);
    let out: String = workflow
        .split_inclusive('\n')
        .enumerate()
        .map(|(n, l)| match edit.take_if(|_| n + 1 == line) {
            Some(edit) => {
                let body = l.strip_suffix('\n').unwrap_or(l);
                format!("{}{}", edit(body), &l[body.len()..])
            }
            None => l.to_owned(),
        })
        .collect();
    assert!(edit.is_none(), "the workflow has a line {line}");
    out
}

/// The key of job `name` on its key line `line`: the indentation before it, the key as
/// written (plain, or in double or single quotes) and the rest of the line from the colon on.
fn split_key<'l>(line: &'l str, name: &str) -> (&'l str, &'l str, &'l str) {
    let text = line.trim_start();
    let indent = &line[..line.len() - text.len()];
    let key_len = [format!("\"{name}\""), format!("'{name}'"), name.to_owned()]
        .iter()
        .find(|k| {
            text.strip_prefix(k.as_str())
                .is_some_and(|r| r.trim_start().starts_with(':'))
        })
        .map(String::len)
        .unwrap_or_else(|| panic!("no key `{name}` at the start of: {line}"));
    (indent, &text[..key_len], &text[key_len..])
}

/// `workflow` with the job `from` renamed `to`: only its key token changes, spelt as before
/// (plain or quoted), so a comment on the line that names the job is left as it is.
fn rename_job(workflow: &str, from: &str, to: &str) -> String {
    edit_line(workflow, job_line(workflow, from), |l| {
        let (indent, key, rest) = split_key(l, from);
        format!("{indent}{}{rest}", key.replacen(from, to, 1))
    })
}

/// `workflow` without its last job `job`: everything from the line of its key on.
fn remove_last_job(workflow: &str, job: &str) -> String {
    let jobs = workflows::job_lines(workflow).expect("the workflow parses");
    assert_eq!(
        jobs.last().map(|(n, _)| n.as_str()),
        Some(job),
        "`{job}` is the last job"
    );
    let line = job_line(workflow, job);
    workflow.split_inclusive('\n').take(line - 1).collect()
}

/// `workflow` with a job `job` added before its first job, at that job's indentation.
fn add_job(workflow: &str, job: &str) -> String {
    let jobs = workflows::job_lines(workflow).expect("the workflow parses");
    let first = jobs.first().expect("the workflow has a job").1;
    edit_line(workflow, first, |l| {
        let indent = &l[..l.len() - l.trim_start().len()];
        format!(
            "{indent}{job}:\n{indent}  runs-on: ubuntu-24.04\n{indent}  steps:\n{indent}    - run: true\n{l}"
        )
    })
}

/// `workflow` spelt another way with the same jobs: a comment on its own line before the first
/// job, every job key in double quotes (a key already quoted stays so), and a comment after each
/// key that names the job again.
fn respelt(workflow: &str) -> String {
    let jobs = workflows::job_lines(workflow).expect("the workflow parses");
    let mut out = workflow.to_owned();
    for (name, line) in &jobs {
        out = edit_line(&out, *line, |l| {
            let (indent, _, rest) = split_key(l, name);
            format!("{indent}\"{name}\"{rest} # {name} is a job of the release")
        });
    }
    let first = jobs.first().expect("the workflow has a job").1;
    edit_line(&out, first, |l| format!("  # the jobs of the release\n{l}"))
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
fn the_real_doc_with_any_job_renamed_fails() {
    let (real, jobs) = (read(DOC), real_jobs());
    for job in &jobs {
        let other = fresh(&jobs, &format!("{job}-renamed"));
        assert_eq!(
            problems(&rename_row(&real, job, &other), &read(RELEASE)),
            vec![
                format!("{DOC}: the job table names job `{other}`, which {RELEASE} does not have"),
                format!("{DOC}: {RELEASE} has job `{job}`, which the job table omits"),
            ],
            "renaming the row of `{job}`"
        );
    }
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
    let jobs = real_jobs();
    let added = fresh(&jobs, "prod");
    assert_eq!(
        problems(&read(DOC), &add_job(&read(RELEASE), &added)),
        vec![format!(
            "{DOC}: {RELEASE} has job `{added}`, which the job table omits"
        )]
    );
}

#[test]
fn the_workflow_spelt_another_way_gives_the_same_answers() {
    let real = read(RELEASE);
    let once = respelt(&real);
    // Respelling a workflow whose keys are already quoted, and whose key lines already carry a
    // comment naming the job, must work too.
    let twice = respelt(&once);
    let single = {
        let jobs = workflows::job_lines(&real).expect("release.yml parses");
        let (name, line) = jobs.last().expect("release.yml has jobs").clone();
        edit_line(&real, line, |l| {
            let (indent, _, rest) = split_key(l, &name);
            format!("{indent}'{name}'{rest} # '{name}' runs last")
        })
    };
    for other in [&once, &twice, &single] {
        assert_ne!(other, &real);
        same_answers_as_the_real_workflow(other);
    }
}

/// The lint answers on `other` as on the real `release.yml`: the same jobs, no problem with the
/// real doc, and the same answers for a rename and a removal made to both files and for an
/// added job.
fn same_answers_as_the_real_workflow(other: &str) {
    let jobs = real_jobs();
    assert_eq!(real_jobs_of(other), jobs);
    assert_eq!(problems(&read(DOC), other), Vec::<String>::new());
    let last = jobs.last().expect("release.yml has jobs");
    let renamed = fresh(&jobs, &format!("{last}-renamed"));
    let workflow = rename_job(other, last, &renamed);
    assert_eq!(
        real_jobs_of(&workflow).last(),
        Some(&renamed),
        "only the key is renamed"
    );
    assert_eq!(
        problems(&rename_row(&read(DOC), last, &renamed), &workflow),
        Vec::<String>::new()
    );
    assert_eq!(
        problems(&remove_row(&read(DOC), last), &remove_last_job(other, last)),
        Vec::<String>::new()
    );
    let added = fresh(&jobs, "prod");
    assert_eq!(
        problems(&read(DOC), &add_job(other, &added)),
        vec![format!(
            "{DOC}: {RELEASE} has job `{added}`, which the job table omits"
        )]
    );
}

#[test]
fn a_comment_naming_the_job_is_left_by_a_rename() {
    let jobs = real_jobs();
    let last = jobs.last().expect("release.yml has jobs");
    let line = job_line(&read(RELEASE), last);
    let commented = edit_line(&read(RELEASE), line, |l| {
        format!("{l} # {last} handles deployment")
    });
    let renamed = rename_job(&commented, last, "renamed");
    let kept: Vec<&str> = renamed
        .lines()
        .filter(|l| l.contains(&format!("# {last} handles deployment")))
        .collect();
    assert_eq!(kept.len(), 1, "the comment is kept");
    assert!(kept[0].trim_start().starts_with("renamed:"), "{}", kept[0]);
}

#[test]
fn a_job_renamed_in_both_the_workflow_and_the_doc_passes() {
    let jobs = real_jobs();
    let last = jobs.last().expect("release.yml has jobs");
    let other = fresh(&jobs, &format!("{last}-renamed"));
    let workflow = rename_job(&read(RELEASE), last, &other);
    assert_eq!(
        problems(&rename_row(&read(DOC), last, &other), &workflow),
        Vec::<String>::new()
    );
}

#[test]
fn a_job_removed_from_both_the_workflow_and_the_doc_passes() {
    let jobs = real_jobs();
    let last = jobs.last().expect("release.yml has jobs");
    let workflow = remove_last_job(&read(RELEASE), last);
    assert_eq!(real_jobs_of(&workflow), jobs[..jobs.len() - 1].to_vec());
    assert_eq!(
        problems(&remove_row(&read(DOC), last), &workflow),
        Vec::<String>::new()
    );
}

#[test]
fn a_job_removed_from_the_workflow_alone_fails() {
    let jobs = real_jobs();
    let last = jobs.last().expect("release.yml has jobs");
    assert_eq!(
        problems(&read(DOC), &remove_last_job(&read(RELEASE), last)),
        vec![format!(
            "{DOC}: the job table names job `{last}`, which {RELEASE} does not have"
        )]
    );
}

#[test]
fn a_fresh_name_is_never_one_already_taken() {
    let taken = ["prod".to_owned(), "prod-new".to_owned()];
    assert_eq!(fresh(&taken, "prod"), "prod-new-new");
    assert_eq!(fresh(&taken, "beta"), "beta");
    let jobs = real_jobs();
    for job in &jobs {
        let name = fresh(&jobs, job);
        assert!(!jobs.contains(&name), "`{name}` is a job of release.yml");
    }
}

#[test]
fn a_row_inside_a_code_fence_does_not_count() {
    for fence in ["```", "~~~", "```markdown"] {
        let d = doc(&["plan", "publish", "gamma"])
            .replace("| `gamma` |", &format!("{fence}\n| `gamma` |"))
            .replace(&format!("{END}\n"), &format!("{}\n{END}\n", &fence[..3]));
        assert_eq!(
            problems(&d, WORKFLOW),
            vec![
                format!(
                    "{DOC}:10: a code fence inside the job table: a row there would not render, so the table holds none"
                ),
                format!(
                    "{DOC}:12: a code fence inside the job table: a row there would not render, so the table holds none"
                ),
            ],
            "fence {fence}"
        );
    }
}

#[test]
fn a_row_inside_an_html_comment_does_not_count() {
    let one_line =
        doc(&["plan", "publish", "gamma"]).replace("| `gamma` |", "<!-- | `gamma` | -->");
    assert_eq!(
        problems(&one_line, WORKFLOW),
        vec![format!(
            "{DOC}:10: an HTML comment inside the job table: a row there would not render, so the table holds none"
        )]
    );
    let spanning =
        doc(&["plan", "publish", "gamma"]).replace("| `gamma` |", "<!--\n| `gamma` |\n-->");
    assert_eq!(
        problems(&spanning, WORKFLOW),
        vec![
            format!(
                "{DOC}:10: an HTML comment inside the job table: a row there would not render, so the table holds none"
            ),
            format!(
                "{DOC}:12: an HTML comment inside the job table: a row there would not render, so the table holds none"
            ),
        ]
    );
}

#[test]
fn a_row_indented_as_code_does_not_count() {
    for indent in ["    ", "\t", "  \t", "        "] {
        let d = doc(&["plan", "publish", "gamma"])
            .replace("| `gamma` |", &format!("{indent}| `gamma` |"));
        assert_eq!(
            problems(&d, WORKFLOW),
            vec![format!(
                "{DOC}:10: an indented code block inside the job table: a row there would not render, so the table holds none"
            )],
            "indent {indent:?}"
        );
    }
}

#[test]
fn a_row_indented_less_than_code_is_a_row() {
    let d = doc(&["plan", "publish", "gamma"]).replace("| `gamma` |", "   | `gamma` |");
    assert_eq!(problems(&d, WORKFLOW), Vec::<String>::new());
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
