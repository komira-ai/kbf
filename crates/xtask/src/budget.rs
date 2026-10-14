//! The interpreter budget file, [`BUDGET`]: the shell, Python, awk, sed, Perl and Ruby
//! this repository still holds, listed by path under a `[files]` or `[src-sh-c]` header,
//! and the test code that names an interpreter, listed by path under
//! `[test-interpreters]`; one `<key>\t<reason>` row each. A line starting with `#` and
//! a blank line are skipped. [`parse`] also reads a `[workflow]` section, which once
//! listed workflow `run:` steps with shell logic; the repository lints refuse any row
//! in it now, and it is still read so that `budget-diff` can compare against a base
//! list that has it.
//!
//! [`parse`] reads the file; the repository lints (`crates/kbf-it/tests/repo_lints.rs`)
//! hold it against what the tree contains. [`new_rows`] is `cargo xtask budget-diff`:
//! the list may only shrink, so a row a change adds is refused, whatever its file is.
//! The one exception is a section the base list has no header for: the change that
//! adds the header brings the section's first rows. A header the base has may not go
//! (an empty section keeps it), so a section cannot be dropped and brought back full.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// The budget file, relative to the repository root.
pub const BUDGET: &str = "tools/interpreter_budget.tsv";

/// One section of the budget file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Files,
    Workflow,
    SrcShC,
    TestInterpreters,
}

impl Section {
    const ALL: [Section; 4] = [
        Section::Files,
        Section::Workflow,
        Section::SrcShC,
        Section::TestInterpreters,
    ];

    pub fn header(self) -> &'static str {
        match self {
            Section::Files => "[files]",
            Section::Workflow => "[workflow]",
            Section::SrcShC => "[src-sh-c]",
            Section::TestInterpreters => "[test-interpreters]",
        }
    }
}

/// One row of the budget file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub section: Section,
    pub key: String,
    /// The 1-based line of the budget file that holds the row.
    pub line: usize,
}

/// The rows of a budget text, and one problem per line that is not a well-formed row,
/// header, comment or blank line, or that repeats a key of its section.
pub fn parse(text: &str) -> (Vec<Row>, Vec<String>) {
    let mut rows: Vec<Row> = Vec::new();
    let mut problems = Vec::new();
    let mut section = None;
    let mut seen_headers = Vec::new();
    let mut first_at: BTreeMap<(Section, String), usize> = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        let n = at + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            match Section::ALL.into_iter().find(|s| s.header() == line) {
                Some(s) if seen_headers.contains(&s) => {
                    problems.push(format!("line {n}: the section {line} appears twice"));
                }
                Some(s) => {
                    seen_headers.push(s);
                    section = Some(s);
                }
                None => problems.push(format!("line {n}: unknown section `{line}`")),
            }
            continue;
        }
        let Some(s) = section else {
            problems.push(format!("line {n}: a row before any section header"));
            continue;
        };
        let fields: Vec<&str> = line.split('\t').collect();
        let [key, reason] = fields[..] else {
            problems.push(format!(
                "line {n}: a row needs two tab-separated fields (<key>, <reason>), found {}",
                fields.len()
            ));
            continue;
        };
        if [key, reason].iter().any(|f| f.is_empty() || f.trim() != *f) {
            problems.push(format!(
                "line {n}: a field is empty or has blanks around it"
            ));
            continue;
        }
        match first_at.get(&(s, key.to_owned())) {
            Some(first) => problems.push(format!(
                "line {n}: a second {} row for `{key}` (the first is line {first})",
                s.header()
            )),
            None => {
                first_at.insert((s, key.to_owned()), n);
                rows.push(Row {
                    section: s,
                    key: key.to_owned(),
                    line: n,
                });
            }
        }
    }
    (rows, problems)
}

/// The sections whose header line `text` holds.
fn headers(text: &str) -> BTreeSet<Section> {
    let lines: BTreeSet<&str> = text.split('\n').collect();
    Section::ALL
        .into_iter()
        .filter(|s| lines.contains(s.header()))
        .collect()
}

/// The rows of `head` whose section and key `base` does not hold: what a change adds
/// to the list. A reason edited, a row moved or a row removed is not a new row, and
/// neither is a row of a section whose header `base` lacks (the change adds the
/// section).
///
/// # Errors
/// Either text has a problem [`parse`] reports, each named `base` or `head`: a
/// comparison of a file that does not parse would compare too little. And `head`
/// lacks a header `base` has: dropping a header would let a later change bring the
/// section back as new, rows and all.
pub fn new_rows(base: &str, head: &str) -> Result<Vec<Row>, Vec<String>> {
    let (base_rows, base_problems) = parse(base);
    let (head_rows, head_problems) = parse(head);
    let base_headers = headers(base);
    let head_headers = headers(head);
    let problems: Vec<String> = base_problems
        .iter()
        .map(|p| format!("base {BUDGET}: {p}"))
        .chain(head_problems.iter().map(|p| format!("{BUDGET}: {p}")))
        .chain(base_headers.difference(&head_headers).map(|s| {
            format!(
                "{BUDGET}: the base has the section {}, and this change removes its \
                 header; an empty section keeps it",
                s.header()
            )
        }))
        .collect();
    if !problems.is_empty() {
        return Err(problems);
    }
    let listed: BTreeSet<(Section, &str)> = base_rows
        .iter()
        .map(|r| (r.section, r.key.as_str()))
        .collect();
    Ok(head_rows
        .into_iter()
        .filter(|r| base_headers.contains(&r.section))
        .filter(|r| !listed.contains(&(r.section, r.key.as_str())))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "# comment\n\n[files]\na.sh\tto port\nb.py\tto delete\n\
                        [workflow]\nci.yml#j/s\tto port\n[src-sh-c]\nk.rs\tkeeper\n";

    fn keys(rows: &[Row]) -> Vec<(Section, &str)> {
        rows.iter().map(|r| (r.section, r.key.as_str())).collect()
    }

    #[test]
    fn the_unchanged_list_adds_nothing() {
        // Control: catches a diff that refuses the list it was given (every row new).
        assert_eq!(new_rows(BASE, BASE), Ok(vec![]));
    }

    #[test]
    fn a_new_file_row_is_reported_with_its_line() {
        // Catches: a diff that only checks removals, or that compares row counts (one
        // row in, one out passes a count).
        let head = BASE.replace("b.py\tto delete\n", "c.sh\tnew\n");
        let rows = new_rows(BASE, &head).expect("both parse");
        assert_eq!(
            rows,
            vec![Row {
                section: Section::Files,
                key: "c.sh".into(),
                line: 5
            }]
        );
    }

    #[test]
    fn a_new_row_in_each_section_is_reported() {
        // Catches: a diff that reads only `[files]`, or that matches keys across
        // sections (a `[src-sh-c]` row for a path `[files]` already lists is new).
        let head = format!("{BASE}x.rs\tsecond keeper\n")
            .replace("[workflow]\n", "[workflow]\nci.yml#j/t\tnew step\n")
            .replace(
                "[src-sh-c]\n",
                "[src-sh-c]\na.sh\tsame key, other section\n",
            );
        let rows = new_rows(BASE, &head).expect("both parse");
        assert_eq!(
            keys(&rows),
            vec![
                (Section::Workflow, "ci.yml#j/t"),
                (Section::SrcShC, "a.sh"),
                (Section::SrcShC, "x.rs"),
            ]
        );
    }

    #[test]
    fn removed_reordered_and_reworded_rows_are_not_new() {
        // Catches: a diff that compares lines or reasons rather than keys, so a removal,
        // a reorder or a reworded reason would be refused. (The empty `[workflow]`
        // stays: a header the base has may not go.)
        let head = "[src-sh-c]\nk.rs\tkeeper, reworded\n[workflow]\n[files]\na.sh\tto port\n";
        assert_eq!(new_rows(BASE, head), Ok(vec![]));
    }

    #[test]
    fn a_section_the_base_lacks_brings_its_first_rows() {
        // Catches: budget-diff refusing the change that adds a section (every row of a
        // new section is new), and the exception reaching a section the base has: a new
        // row there is still refused when another section is new.
        let head = format!("{BASE}[test-interpreters]\nt.rs\tprobe\nu.rs\tprobe\n")
            .replace("[files]\n", "[files]\nc.sh\tnew\n");
        let rows = new_rows(BASE, &head).expect("both parse");
        assert_eq!(keys(&rows), vec![(Section::Files, "c.sh")]);
        // Once the base has the section, its rows only shrink like the others.
        let base = format!("{BASE}[test-interpreters]\nt.rs\tprobe\n");
        let rows = new_rows(&base, &head.replace("c.sh\tnew\n", "")).expect("both parse");
        assert_eq!(keys(&rows), vec![(Section::TestInterpreters, "u.rs")]);
        // An empty section in the base counts as having it.
        let base = format!("{BASE}[test-interpreters]\n");
        let rows = new_rows(&base, &format!("{base}t.rs\tprobe\n")).expect("both parse");
        assert_eq!(keys(&rows), vec![(Section::TestInterpreters, "t.rs")]);
    }

    #[test]
    fn a_header_the_base_has_may_not_go() {
        // Catches: a change that drops an emptied section's header, after which the
        // next change could add the section back with rows as if it were new; and the
        // refusal naming the wrong section.
        let base = format!("{BASE}[test-interpreters]\n");
        assert_eq!(
            new_rows(&base, BASE),
            Err(vec![
                "tools/interpreter_budget.tsv: the base has the section \
                 [test-interpreters], and this change removes its header; an empty \
                 section keeps it"
                    .to_owned()
            ])
        );
    }

    #[test]
    fn a_problem_on_either_side_is_refused_and_named() {
        // Catches: a diff that skips a row it cannot parse (an old three-field row would
        // then add nothing) or that reports a base problem as the change's own.
        let bad = "[files]\n3\ta.sh\tcounted\n";
        let base = new_rows(bad, BASE).expect_err("bad base");
        assert_eq!(base.len(), 1);
        assert!(base[0].starts_with("base tools/interpreter_budget.tsv: line 2: "));
        let head = new_rows(BASE, bad).expect_err("bad head");
        assert!(head[0].starts_with("tools/interpreter_budget.tsv: line 2: "));
    }

    #[test]
    fn parse_reports_each_kind_of_bad_line() {
        // Catches: a parser that accepts a repeated or unknown header, a row before any
        // header, a row with blanks or missing fields, or a repeated key.
        let text = "x\tbefore\n[files]\n[nope]\na\tr\na\tagain\n b\tr\nc\t\nd\n[files]\n";
        let (rows, problems) = parse(text);
        assert_eq!(keys(&rows), vec![(Section::Files, "a")]);
        assert_eq!(
            problems,
            vec![
                "line 1: a row before any section header",
                "line 3: unknown section `[nope]`",
                "line 5: a second [files] row for `a` (the first is line 4)",
                "line 6: a field is empty or has blanks around it",
                "line 7: a field is empty or has blanks around it",
                "line 8: a row needs two tab-separated fields (<key>, <reason>), found 1",
                "line 9: the section [files] appears twice",
            ]
        );
    }
}
