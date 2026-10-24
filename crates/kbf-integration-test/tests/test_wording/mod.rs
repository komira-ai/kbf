//! Test code says what each test proves and which defect it catches.
//!
//! That is the maintainers' rule: a comment or a name states the defect a test guards
//! against ("Catches: the check dropped"), and does not narrate how the test was checked.
//! So no `.rs` file under `crates/` may hold any form in `FORBIDDEN` below, in any letter
//! case: the word for a changed copy of the code, the phrase for a failure seen when
//! checking a test, or the phrase for a defect set up for that purpose. A hit is reported as
//! `path:line: <the line>`. Fixture wording is unaffected: "a planted zombie" describes a
//! test's input.
//!
//! This module's own file is the one exemption, by exact path: it has to spell the
//! forbidden forms to test them.

/// The forms no line may hold (compared in lower case).
const FORBIDDEN: &[&str] = &["mutant", "seen red", "planted defect"];

/// This file, the only exemption.
const OWN_FILE: &str = "crates/kbf-integration-test/tests/test_wording/mod.rs";

/// One line per forbidden form found in a `.rs` file under `crates/` (but not in
/// [`OWN_FILE`]), as `path:line: <the line>`.
fn problems(files: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut out = Vec::new();
    for (path, bytes) in files {
        if !path.starts_with("crates/") || !path.ends_with(".rs") || path == OWN_FILE {
            continue;
        }
        let text = String::from_utf8_lossy(bytes);
        for (i, line) in text.lines().enumerate() {
            let lower = line.to_lowercase();
            if FORBIDDEN.iter().any(|w| lower.contains(w)) {
                out.push(format!("{path}:{}: {line}", i + 1));
            }
        }
    }
    out
}

/// Catches test wording that narrates how a test was checked instead of the defect it
/// catches, and the scan passing on an empty file list.
#[test]
fn no_rust_file_narrates_how_a_test_was_checked() {
    let root = super::repo_root();
    let files: Vec<(String, Vec<u8>)> = super::listed_files(&root)
        .into_iter()
        .filter(|f| f.starts_with("crates/") && f.ends_with(".rs"))
        .filter_map(|f| super::read_bytes(&root.join(&f)).map(|b| (f, b)))
        .collect();
    assert!(
        files.len() > 100,
        "the scan read only {} Rust files under crates/",
        files.len()
    );
    assert_eq!(problems(&files), Vec::<String>::new());
}

fn file(path: &str, text: &str) -> Vec<(String, Vec<u8>)> {
    vec![(path.to_owned(), text.as_bytes().to_vec())]
}

/// Catches a scan that misses a form: each forbidden form, in any letter case and in a
/// longer word, is reported with its path and its 1-based line.
#[test]
fn each_forbidden_form_is_reported_with_its_path_and_line() {
    for (text, line) in [
        (
            "/// Mutant: the arm dropped.",
            "/// Mutant: the arm dropped.",
        ),
        ("// the MUTANTS are listed", "// the MUTANTS are listed"),
        ("let mutant = 1;", "let mutant = 1;"),
        (
            "/// Seen red: the check dropped.",
            "/// Seen red: the check dropped.",
        ),
        ("// it was seen red once", "// it was seen red once"),
        ("// A Planted Defect here", "// A Planted Defect here"),
    ] {
        let files = file("crates/kbf-x/src/lib.rs", &format!("fn f() {{}}\n{text}\n"));
        assert_eq!(
            problems(&files),
            vec![format!("crates/kbf-x/src/lib.rs:2: {line}")],
            "{text}"
        );
    }
}

/// Catches a scan that flags fixture wording: a test input that is "planted" is allowed.
#[test]
fn fixture_wording_passes() {
    let files = file(
        "crates/kbf-x/src/lib.rs",
        "// a planted zombie\nfn planted_zombie_is_seen() {} // Room to plant\n// seen once\n",
    );
    assert_eq!(problems(&files), Vec::<String>::new());
}

/// Catches a scan that reads what it should not: a file that is not Rust, and a file
/// outside `crates/`, are not checked.
#[test]
fn only_rust_files_under_crates_are_checked() {
    for path in [
        "crates/kbf-x/notes.md",
        "crates/kbf-x/Cargo.toml",
        "docs/x.rs",
        "xtask.rs",
    ] {
        assert_eq!(
            problems(&file(path, "a mutant, seen red, a planted defect")),
            Vec::<String>::new(),
            "{path}"
        );
    }
}

/// Catches an exemption wider than this one file: the same text in the module's own
/// path is skipped, and in a sibling path is reported.
#[test]
fn only_this_modules_own_file_is_exempt() {
    let text = "// a mutant";
    assert_eq!(problems(&file(OWN_FILE, text)), Vec::<String>::new());
    let sibling = "crates/kbf-integration-test/tests/test_wording/other.rs";
    assert_eq!(problems(&file(sibling, text)).len(), 1);
}
