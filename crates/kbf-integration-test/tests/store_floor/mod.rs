//! A test that opens a `file://` cold store names its free-space floor.
//!
//! A cold store on a file system refuses a write once the free space would fall under its
//! floor, by default 5% of the file system's size. On a CI runner the free space falls while
//! the instrumented coverage build fills the disk, so a test that writes through a store at
//! the default floor passes or fails with the state of the runner's disk. A rig that is not
//! testing the floor passes `--cold-store-floor-bytes` (1 is enough), or opens its `FsStore`
//! with `Floor::Bytes`; the rule is per test file (under a `tests/` directory, or named
//! `*tests.rs`) in any crate: a file whose code names `--cold-store` and a `file://{..}`
//! URI built from a directory must name `cold-store-floor-bytes` or `Floor::Bytes` at least as often as it has
//! `--cold-store=file://{..}` rigs (a rig that passes its URI as a separate `"--cold-store"`
//! argument counts once per file). A floor mention on the first line of an `if` or `else`
//! block does not count: that rig sets its floor only sometimes.
//!
//! What it catches: a new test rig that builds a cold store directory and never sets a
//! floor, or sets it only under a condition. What it misses: a floor mention that is not on a
//! rig while another rig lacks one, a condition written some other way (a `match` arm, a
//! later line of the block), harness code
//! outside test files (`kbf-integration-test/src`, `kbf-harness/src`, which build the
//! cells' and units' flags), and a file that builds its flags some other way (`--config`,
//! a variable URI scheme).

/// Files that name a cold store directory and no floor, and why that is fine.
const ALLOWED: &[(&str, &str)] = &[
    (
        "crates/kbf-server/tests/scheduler_core_flag.rs",
        "its cold store is a path that does not exist and the test only parses the flag set",
    ),
    (
        "crates/kbf-cli/src/node_tests.rs",
        "its cold store is an s3 URI; the file:// URI is the hot store's, and nothing is started",
    ),
    (
        "crates/kbf-cli/src/preflight/traverse_tests.rs",
        "paths that do not exist, checked by the preflight and never opened",
    ),
    (
        "crates/kbf-cli/tests/preflight.rs",
        "its cold store is an s3 URI; the file:// URI is the hot store's, and nothing writes",
    ),
    (
        "crates/kbf-cli/tests/config_process.rs",
        "starts a node from a settings file and stops it, with no blob written; the file's values are strings",
    ),
    (
        "crates/kbf-integration-test/tests/m2.rs",
        "compares the cell's flag list with an exact expected line; the cell runs in its own job",
    ),
];

/// Whether a path is test code: under a `tests/` directory, or a file named `*tests.rs`.
fn is_test_file(path: &str) -> bool {
    path.starts_with("crates/")
        && path.ends_with(".rs")
        && (path.contains("/tests/") || path.ends_with("tests.rs"))
        && !path.starts_with("crates/kbf-integration-test/tests/store_floor/")
}

/// The scanned text of a file: its comment lines are blank.
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

/// The `--cold-store=file://{..}` rigs of a file, and its floor mentions. A file that
/// builds its flags as a separate `"--cold-store"` argument counts as one rig.
fn rigs_and_floors(text: &str) -> (usize, usize) {
    let code = code_of(text);
    if !code.contains("file://{") || !code.contains("--cold-store") {
        return (0, 0);
    }
    let inline = code.matches("--cold-store=file://{").count();
    let mut floors = 0;
    let mut previous = "";
    for line in code.lines().filter(|l| !l.trim().is_empty()) {
        if !opens_a_condition(previous) {
            floors += line.matches("cold-store-floor-bytes").count()
                + line.matches("Floor::Bytes").count();
        }
        previous = line;
    }
    (inline.max(1), floors)
}

/// Whether a code line opens the block of an `if` or an `else`: a floor set on the next
/// line is set only sometimes, so the rig it belongs to may run at the default floor.
fn opens_a_condition(line: &str) -> bool {
    let line = line.trim();
    line.ends_with('{')
        && (line.starts_with("if ") || line.starts_with("} else") || line.starts_with("else"))
}

/// Whether the file has more cold store rigs than floor mentions.
fn lacks_floor(text: &str) -> bool {
    let (rigs, floors) = rigs_and_floors(text);
    floors < rigs
}

/// What is wrong with the files, one line each.
fn problems(files: &[(String, Vec<u8>)], allowed: &[(&str, &str)]) -> Vec<String> {
    let mut out = Vec::new();
    let mut hit_allowed = Vec::new();
    for (path, bytes) in files {
        if !is_test_file(path) {
            continue;
        }
        if !lacks_floor(&String::from_utf8_lossy(bytes)) {
            continue;
        }
        if allowed.iter().any(|(p, _)| p == path) {
            hit_allowed.push(path.as_str());
        } else {
            out.push(format!(
                "{path}: opens a file cold store at the default free-space floor; \
                 pass --cold-store-floor-bytes"
            ));
        }
    }
    for (path, _) in allowed {
        if !hit_allowed.contains(path) {
            out.push(format!(
                "{path}: listed and no longer needs it; remove the entry"
            ));
        }
    }
    out
}

/// Catches a server test rig whose outcome depends on the runner's free disk, and the
/// scan passing on an empty file list.
#[test]
fn server_test_rigs_set_the_cold_store_floor() {
    let root = super::repo_root();
    let files: Vec<(String, Vec<u8>)> = super::listed_files(&root)
        .into_iter()
        .filter(|f| is_test_file(f))
        .filter_map(|f| super::read_bytes(&root.join(&f)).map(|b| (f, b)))
        .collect();
    assert!(
        files.len() > 100,
        "the scan read only {} test files",
        files.len()
    );
    assert_eq!(problems(&files, ALLOWED), Vec::<String>::new());
}

/// Catches a lint that cannot fail: a rig with no floor is reported, one with a floor, a
/// comment naming the pattern, a file elsewhere and a stale entry are handled.
#[test]
fn the_scan_finds_a_rig_without_a_floor() {
    let file = |path: &str, text: &str| vec![(path.to_owned(), text.as_bytes().to_vec())];
    let bare = "format!(\"--cold-store=file://{}\", d.display())";
    let path = "crates/kbf-server/tests/x.rs";
    assert_eq!(problems(&file(path, bare), &[]).len(), 1);
    assert_eq!(
        problems(&file(path, bare), &[(path, "why")]),
        Vec::<String>::new()
    );
    assert_eq!(problems(&[], &[(path, "why")]).len(), 1, "a stale entry");
    let floor = "\"--cold-store-floor-bytes=1\"";
    let two = format!("{bare}; {bare}");
    assert_eq!(
        problems(&file(path, &format!("{floor}; {two}")), &[]).len(),
        1,
        "two rigs and one floor"
    );
    assert_eq!(
        problems(&file(path, &format!("{floor}; {floor}; {two}")), &[]),
        Vec::<String>::new(),
        "two rigs and two floors"
    );
    let separate = "\"--cold-store\", format!(\"file://{}\", d.display())";
    assert_eq!(
        problems(&file(path, separate), &[]).len(),
        1,
        "a separate argument is one rig"
    );
    assert_eq!(
        problems(&file(path, &format!("{floor}; {separate}")), &[]),
        Vec::<String>::new(),
        "a separate argument with one floor"
    );
    let optional = "let mut argv = vec![format!(\"--cold-store=file://{}\", d)];\n\
                    if let Some(floor) = floor {\n    \
                    argv.push(format!(\"--cold-store-floor-bytes={floor}\"));\n}";
    assert_eq!(
        problems(&file(path, optional), &[]).len(),
        1,
        "a floor set only when one is given"
    );
    let otherwise = "let argv = if x {\n    vec![]\n} else {\n    \
                     vec![\"--cold-store-floor-bytes=1\"]\n};\n\
                     format!(\"--cold-store=file://{}\", d)";
    assert_eq!(
        problems(&file(path, otherwise), &[]).len(),
        1,
        "a floor set only in an else block"
    );
    let after_if = "if x {\n    y();\n}\nargv.push(\"--cold-store-floor-bytes=1\");\n\
                    format!(\"--cold-store=file://{}\", d)";
    assert_eq!(
        problems(&file(path, after_if), &[]),
        Vec::<String>::new(),
        "a floor on the line after a closed if block counts"
    );
    for (p, text) in [
        (
            path,
            "\"--cold-store-floor-bytes=1\", format!(\"--cold-store=file://{}\", d)",
        ),
        (path, "// \"--cold-store\" file://{}"),
        (path, "format!(\"--cold-store=s3://b\")"),
        ("crates/kbf-cli/src/lib.rs", bare),
        (
            path,
            "Floor::Bytes(1); format!(\"--cold-store=file://{}\", d)",
        ),
    ] {
        assert_eq!(
            problems(&file(p, text), &[]),
            Vec::<String>::new(),
            "{text}"
        );
    }
}
