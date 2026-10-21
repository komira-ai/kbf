//! The `[test-interpreters]` rule over planted trees. Test code (a file under
//! `crates/*/tests/`, or `#[cfg(test)]` code under `crates/*/src/`) that names an
//! interpreter needs a row for its file; a row whose file names none is stale; a
//! malformed or repeated row is refused. Each planted source is a whole Rust file in
//! one raw string, so no literal of this file is an interpreter's name itself.

use std::collections::BTreeMap;

use super::{budget_problems, interpreter_scan};

const PROBE: &str = "crates/k/tests/probe.rs";
const PROBE_RS: &str =
    r#"#[test] fn probe() { std::process::Command::new("/bin/sh").args(["-c", "kill -0 $$"]); }"#;
const LIB: &str = "crates/k/src/lib.rs";
const LIB_RS: &str = r##"pub fn run() {}
#[cfg(test)]
mod tests {
    #[test]
    fn script() { std::fs::write("x", b"#!/usr/bin/perl\nprint 1;\n").unwrap() }
}"##;
const BUDGET: &str = "[files]\n[src-sh-c]\n[test-interpreters]\n\
                      crates/k/src/lib.rs\tunit test writes a script\n\
                      crates/k/tests/probe.rs\tprocess probe\n";

/// The control tree: one listed file per kind of test code, a test file that names no
/// interpreter, and production code that names one (not this rule's).
fn tree() -> BTreeMap<String, Vec<u8>> {
    [
        (PROBE, PROBE_RS),
        (LIB, LIB_RS),
        ("crates/k/tests/plain.rs", r#"#[test] fn t() { g("echo") }"#),
        (
            "crates/k/src/prod.rs",
            r#"pub fn f() { std::process::Command::new("perl"); }"#,
        ),
    ]
    .into_iter()
    .map(|(p, t)| (p.to_owned(), t.as_bytes().to_vec()))
    .collect()
}

fn problems(tree: &BTreeMap<String, Vec<u8>>, budget: &str) -> Vec<String> {
    let files: Vec<String> = tree.keys().cloned().collect();
    budget_problems(interpreter_scan(&files, |f| tree.get(f).cloned()), budget)
}

/// The refusal of an unlisted file whose test code names `what`.
fn unlisted(path: &str, what: &str) -> String {
    format!(
        "`{path}`: its test code names {what}, and tools/interpreter_budget.tsv has no \
         [test-interpreters] row for it; port it to Rust, or add the row `{path}\\t<reason>`"
    )
}

#[test]
fn the_control_tree_passes() {
    // Catches: the control refused (a listed test file, a listed unit test module, a
    // test file without an interpreter, or production code naming `perl` with no
    // `-c`), so every planted defect below would go red for the wrong reason.
    assert_eq!(problems(&tree(), BUDGET), Vec::<String>::new());
}

#[test]
fn a_planted_spawn_in_an_unlisted_test_file_is_refused() {
    // Catches (planted defect): `Command::new("perl")` in a new file under
    // `crates/*/tests/`, a support module there, and a script with a `#!/bin/bash`
    // line a test writes, none of them listed: a lint that skips `tests/`, reads only
    // program literals or misses the `#!` form passes them.
    let plants = [
        (
            "crates/k/tests/new.rs",
            r#"#[test] fn spawn() { std::process::Command::new("perl").args(["-e", "1"]); }"#,
            "`perl` (in `spawn`)",
        ),
        (
            "crates/k/tests/support/mod.rs",
            r#"pub fn run() { Spec::argv(&["/usr/bin/awk", "1"]) }"#,
            "`awk` (in `run`)",
        ),
        (
            "crates/k/tests/script.rs",
            r##"#[test] fn write() { std::fs::write("x", "#!/bin/bash\nexit 0\n").unwrap() }"##,
            "`bash` (in `write`)",
        ),
    ];
    for (path, text, what) in plants {
        let mut t = tree();
        t.insert(path.to_owned(), text.as_bytes().to_vec());
        assert_eq!(problems(&t, BUDGET), vec![unlisted(path, what)], "{path}");
    }
}

#[test]
fn a_planted_spawn_in_unlisted_unit_test_code_is_refused() {
    // Catches (planted defect): a `#[cfg(test)]` module under `crates/*/src/` that
    // spawns `/usr/bin/python3`, in a file with no row: a lint that reads only
    // `tests/` passes it. The file's production code names nothing.
    let mut t = tree();
    let text = r#"pub fn f() {}
#[cfg(test)]
mod tests { #[test] fn t() { std::process::Command::new("/usr/bin/python3"); } }"#;
    t.insert("crates/k/src/other.rs".to_owned(), text.as_bytes().to_vec());
    assert_eq!(
        problems(&t, BUDGET),
        vec![unlisted(
            "crates/k/src/other.rs",
            "`python3` (in `tests::t`)"
        )]
    );
}

#[test]
fn a_listed_file_may_change_its_spawns() {
    // Catches: the rule tied to which interpreter or how many spawns a listed file
    // has. A row is per file and says it exists, as `[files]` rows do; a second
    // interpreter in a listed file passes.
    let mut t = tree();
    let grown = format!("{PROBE_RS}\nfn more() {{ g(\"zsh\"); g(\"sed\") }}\n");
    t.insert(PROBE.to_owned(), grown.into_bytes());
    assert_eq!(problems(&t, BUDGET), Vec::<String>::new());
}

#[test]
fn a_stale_row_is_refused() {
    // Catches (planted defect): a row left for a test file that is gone, or that no
    // longer names an interpreter, so the list stops shrinking with the code.
    let want = vec![
        "line 5: the [test-interpreters] row for `crates/k/tests/probe.rs` names nothing \
         the scan found (deleted, renamed, or no longer counted); remove the row"
            .to_owned(),
    ];
    let mut gone = tree();
    gone.remove(PROBE);
    assert_eq!(problems(&gone, BUDGET), want);
    let mut ported = tree();
    ported.insert(
        PROBE.to_owned(),
        br#"#[test] fn probe() { g("echo") }"#.to_vec(),
    );
    assert_eq!(problems(&ported, BUDGET), want);
}

#[test]
fn a_malformed_or_repeated_row_is_refused() {
    // Catches (planted defect): a row with a blank for its tab (so the parser drops
    // it and its file is unlisted, both said), and a second row for one file, which
    // would let a stale copy hide.
    let blank = BUDGET.replace("probe.rs\tprocess", "probe.rs process");
    assert_eq!(
        problems(&tree(), &blank),
        vec![
            "line 5: a row needs two tab-separated fields (<key>, <reason>), found 1".to_owned(),
            unlisted(PROBE, "`sh` (in `probe`)"),
        ]
    );
    let twice = format!("{BUDGET}{PROBE}\tagain\n");
    assert_eq!(
        problems(&tree(), &twice),
        vec![format!(
            "line 6: a second [test-interpreters] row for `{PROBE}` (the first is line 5)"
        )]
    );
}
