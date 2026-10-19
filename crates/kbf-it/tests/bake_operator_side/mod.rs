//! The bake evaluator runs on the operator's side only (docs/design/kbf-cicd.md, section 3
//! and 7): it reads the farm's metrics and must never be an Actions job, where a hosted
//! runner on a public repository would reach the operator's farm. So nothing under `.github/`
//! (workflows, local actions, anything) may name `kbf-bake`. A workflow that builds the whole
//! workspace does not name it and is not affected; one that runs, packages or even lists the
//! crate is.

/// The names that mean the evaluator, in any case.
const NAMES: &[&str] = &["kbf-bake", "kbf_bake"];

/// `path:line` of every line of the files under `.github/` that names the evaluator.
fn offenders(files: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut found = Vec::new();
    for (path, bytes) in files {
        if !path.starts_with(".github/") {
            continue;
        }
        let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
        for (i, line) in text.lines().enumerate() {
            if NAMES.iter().any(|n| line.contains(n)) {
                found.push(format!("{path}:{}", i + 1));
            }
        }
    }
    found
}

/// Catches the evaluator wired into a workflow: the committed tree names it nowhere under
/// `.github/`, and the scan read workflows (it is not passing on an empty list).
#[test]
fn nothing_under_github_names_the_bake_evaluator() {
    let root = super::repo_root();
    let files: Vec<(String, Vec<u8>)> = super::listed_files(&root)
        .into_iter()
        .filter(|f| f.starts_with(".github/"))
        .filter_map(|f| super::read_bytes(&root.join(&f)).map(|b| (f, b)))
        .collect();
    assert!(
        files.iter().any(|(f, _)| super::is_workflow(f)),
        "the scan found no workflow under .github/"
    );
    assert_eq!(
        offenders(&files),
        Vec::<String>::new(),
        "kbf-bake is operator-side only"
    );
}

/// Catches a lint that cannot fail: each way of naming the evaluator in `.github/` is found,
/// and a mention elsewhere is not.
#[test]
fn the_scan_finds_a_planted_mention() {
    let planted = |path: &str, text: &str| vec![(path.to_owned(), text.as_bytes().to_vec())];
    for (path, text) in [
        (
            ".github/workflows/gamma.yml",
            "jobs:\n  bake:\n    steps:\n      - run: cargo run -p kbf-bake -- run\n",
        ),
        (
            ".github/workflows/ci.yml",
            "      - run: cargo test -p KBF-BAKE\n",
        ),
        (
            ".github/actions/bake/action.yml",
            "runs:\n  using: composite\n  # kbf_bake\n",
        ),
        (".github/CODEOWNERS", "crates/kbf-bake/ @owner\n"),
    ] {
        assert_eq!(offenders(&planted(path, text)).len(), 1, "{path}");
    }
    let found = offenders(&planted(
        ".github/workflows/gamma.yml",
        "a\nb\nuses kbf-bake\n",
    ));
    assert_eq!(found, vec![".github/workflows/gamma.yml:3".to_owned()]);
    assert!(
        offenders(&planted(
            "docs/design/kbf-cicd.md",
            "kbf-bake runs on the operator's side"
        ))
        .is_empty()
    );
    assert!(
        offenders(&planted(
            ".github/workflows/ci.yml",
            "      - run: cargo test --workspace\n"
        ))
        .is_empty()
    );
    // Bytes that are not UTF-8 are still scanned.
    let mut bytes = b"\xff\xfe kbf-bake".to_vec();
    bytes.push(b'\n');
    assert_eq!(offenders(&[(".github/x".to_owned(), bytes)]).len(), 1);
}
