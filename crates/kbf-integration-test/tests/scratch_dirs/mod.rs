//! Test scratch directories under the system temporary directory are `TestDir`s.
//!
//! A test that builds its own `<temp dir>/<name>-<pid>` leaves it behind: the dev box's
//! `/tmp` held thousands of such directories, one per test per run. `kbf_testkit::dir::TestDir`
//! is unique per test and removed on drop, so a path made by joining a name onto the system
//! temporary directory, in any `.rs` file under `crates/`, must be in `ALLOWED` with the
//! reason it does not leak. The count per file is exact: a new hit in a listed file fails, and
//! so does a listed hit that is gone (the list shrinks as sites move to `TestDir`).
//!
//! What it catches: a new `temp_dir().join(..)` (on one line or split over several, behind
//! `std::env::` or imported) in a file that is not listed. What it misses: a path made some
//! other way (`PathBuf::from("/tmp/..")`, a `temp_dir()` bound to a variable and joined
//! later, `CARGO_TARGET_TMPDIR`, which is under `target/` and not `/tmp`), a `TestDir` whose
//! drop order outlives a spawned process, and a listed file whose hit stops removing its
//! directory: the reasons below are a reviewer's claim, not a check.

/// Files with a `temp_dir().join(..)`, how many, and why the directory does not outlive
/// the test or the process that makes it.
const ALLOWED: &[(&str, usize, &str)] = &[
    (
        "crates/kbf-deploy/src/cli.rs",
        1,
        "product code: a tool's private state directory, removed by the Drop of its guard",
    ),
    (
        "crates/kbf-driver-container/tests/support/real.rs",
        1,
        "the image store every cell shares, kept between runs on purpose (subordinate-id owned, read-only images)",
    ),
    (
        "crates/kbf-harness/src/lib.rs",
        1,
        "one root whose per-run directories the next run sweeps (`sweep_dead`, tested)",
    ),
    (
        "crates/kbf-server/src/serve_remote_tests.rs",
        1,
        "a secret file removed in the same function, before any assertion can fail",
    ),
    (
        "crates/update-register/src/tests.rs",
        1,
        "a socket directory removed by the Drop of its guard; the crate may link no TestDir (layering.rs)",
    ),
    (
        "crates/xtask/src/darwin/mac.rs",
        1,
        "product code: a call's directory, removed by the Drop of its guard",
    ),
    (
        "crates/xtask/src/lib.rs",
        3,
        "product code: the release, verify and identity scratch, each removed after its run whatever its result",
    ),
    (
        "crates/xtask/src/release/bump.rs",
        1,
        "product code: the bump verb's clone when no --workdir is given, removed after the run whatever its result",
    ),
    (
        "crates/xtask/src/release/farm_version.rs",
        1,
        "product code: the farm-version verb's download directory, removed after the run whatever its result",
    ),
    (
        "crates/xtask/src/release/fetch_previous.rs",
        1,
        "product code: the fetch-previous verb's scratch, removed after the run whatever its result",
    ),
];

/// The scanned text of a file: its comment lines (a doc line that names the pattern) are blank.
fn code_of(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim_start().starts_with("//") {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How many times `text` joins onto the temporary directory: `temp_dir()`, optional white
/// space (a method chain split over lines), then `.join(`.
fn joins(text: &str) -> usize {
    const CALL: &str = "temp_dir()";
    let code = code_of(text);
    let mut count = 0;
    let mut rest = code.as_str();
    while let Some(at) = rest.find(CALL) {
        rest = &rest[at + CALL.len()..];
        if rest.trim_start().starts_with(".join(") {
            count += 1;
        }
    }
    count
}

/// What is wrong with the files' hits against `allowed`, one line each.
fn problems(files: &[(String, Vec<u8>)], allowed: &[(&str, usize, &str)]) -> Vec<String> {
    let mut found: Vec<(&str, usize)> = Vec::new();
    for (path, bytes) in files {
        if !path.starts_with("crates/")
            || !path.ends_with(".rs")
            || path.starts_with("crates/kbf-integration-test/tests/scratch_dirs/")
        {
            continue;
        }
        let n = joins(&String::from_utf8_lossy(bytes));
        if n > 0 {
            found.push((path, n));
        }
    }
    let mut out = Vec::new();
    for &(path, n) in &found {
        match allowed.iter().find(|(p, _, _)| *p == path) {
            None => out.push(format!(
                "{path}: {n} join(s) onto the temp dir; use kbf_testkit::dir::TestDir"
            )),
            Some(&(_, want, _)) if want != n => out.push(format!(
                "{path}: {n} join(s) onto the temp dir, the list allows {want}"
            )),
            Some(_) => {}
        }
    }
    for &(path, want, _) in allowed {
        if !found.iter().any(|(p, _)| *p == path) {
            out.push(format!(
                "{path}: listed for {want} join(s) and has none; remove the entry"
            ));
        }
    }
    out
}

/// Catches a scratch directory made under the system temporary directory without a
/// `TestDir`, and the scan passing on an empty file list.
#[test]
fn no_test_joins_a_name_onto_the_temp_dir_outside_the_list() {
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
    assert_eq!(problems(&files, ALLOWED), Vec::<String>::new());
}

/// Catches a lint that cannot fail: each way of joining onto the temp directory is found
/// (one line, a split chain, an imported call), the forms the scan must not flag are not,
/// and a stale or too-small entry is reported.
#[test]
fn the_scan_finds_a_planted_join() {
    let file = |path: &str, text: &str| vec![(path.to_owned(), text.as_bytes().to_vec())];
    for text in [
        "let d = std::env::temp_dir().join(format!(\"x-{}\", 1));",
        "let d = std::env::temp_dir()\n        .join(\"x\");",
        "use std::env::temp_dir;\nlet d = temp_dir().join(\"x\");",
    ] {
        assert_eq!(joins(text), 1, "{text}");
    }
    let hit = file(
        "crates/kbf-x/src/lib.rs",
        "fn f() { std::env::temp_dir().join(\"a\"); std::env::temp_dir().join(\"b\"); }",
    );
    assert_eq!(problems(&hit, &[]).len(), 1);
    assert_eq!(
        problems(&hit, &[("crates/kbf-x/src/lib.rs", 2, "why")]),
        Vec::<String>::new()
    );
    assert_eq!(
        problems(&hit, &[("crates/kbf-x/src/lib.rs", 1, "why")]).len(),
        1,
        "a second join in a listed file is reported"
    );
    assert_eq!(
        problems(&[], &[("crates/kbf-x/src/lib.rs", 1, "why")]).len(),
        1,
        "an entry with no hit is reported"
    );
    for (path, text) in [
        (
            "crates/kbf-x/src/lib.rs",
            "// std::env::temp_dir().join(\"a\")",
        ),
        (
            "crates/kbf-x/src/lib.rs",
            "/// `std::env::temp_dir().join(..)`",
        ),
        (
            "crates/kbf-x/src/lib.rs",
            "TestDir::under(std::env::temp_dir(), \"a\"); canonicalize(std::env::temp_dir())",
        ),
        ("docs/notes.md", "std::env::temp_dir().join(\"a\")"),
        (
            "crates/kbf-x/Cargo.toml",
            "std::env::temp_dir().join(\"a\")",
        ),
        (
            "crates/kbf-integration-test/tests/scratch_dirs/mod.rs",
            "std::env::temp_dir().join(\"a\")",
        ),
    ] {
        assert_eq!(
            problems(&file(path, text), &[]),
            Vec::<String>::new(),
            "{path}"
        );
    }
}
