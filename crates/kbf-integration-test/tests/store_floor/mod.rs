//! A `kbf-server` integration test that opens a `file://` cold store names its free-space floor.
//!
//! A cold store on a file system refuses a write once the free space would fall under its
//! floor, by default 5% of the file system's size. On a CI runner the free space falls while
//! the instrumented coverage build fills the disk, so a test that writes through a store at
//! the default floor passes or fails with the state of the runner's disk. A rig that is not
//! testing the floor passes `--cold-store-floor-bytes` (1 is enough); the rule is per file
//! under `crates/kbf-server/tests/`: a file whose code names `--cold-store` and a
//! `file://{..}` URI built from a directory must also name `cold-store-floor-bytes`.
//!
//! What it catches: a new rig in that directory that builds a cold store directory and
//! never sets a floor. What it misses: a floor set in one rig of a file and not another,
//! a floor set in a file that has a second rig without one, rigs outside that directory,
//! and a file that builds its flags some other way (`--config`, a variable URI scheme).

/// Files that name a cold store directory and no floor, and why that is fine.
const ALLOWED: &[(&str, &str)] = &[(
    "crates/kbf-server/tests/scheduler_core_flag.rs",
    "its cold store is a path that does not exist and the test only parses the flag set",
)];

const DIR: &str = "crates/kbf-server/tests/";

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

/// Whether the file builds a cold store directory without naming the floor flag.
fn lacks_floor(text: &str) -> bool {
    let code = code_of(text);
    code.contains("--cold-store")
        && code.contains("file://{")
        && !code.contains("cold-store-floor-bytes")
}

/// What is wrong with the files, one line each.
fn problems(files: &[(String, Vec<u8>)], allowed: &[(&str, &str)]) -> Vec<String> {
    let mut out = Vec::new();
    let mut hit_allowed = Vec::new();
    for (path, bytes) in files {
        if !path.starts_with(DIR) || !path.ends_with(".rs") {
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
        .filter(|f| f.starts_with(DIR) && f.ends_with(".rs"))
        .filter_map(|f| super::read_bytes(&root.join(&f)).map(|b| (f, b)))
        .collect();
    assert!(
        files.len() > 20,
        "the scan read only {} files under {DIR}",
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
    for (p, text) in [
        (
            path,
            "\"--cold-store-floor-bytes=1\", format!(\"--cold-store=file://{}\", d)",
        ),
        (path, "// \"--cold-store\" file://{}"),
        (path, "format!(\"--cold-store=s3://b\")"),
        ("crates/kbf-cli/tests/x.rs", bare),
    ] {
        assert_eq!(
            problems(&file(p, text), &[]),
            Vec::<String>::new(),
            "{text}"
        );
    }
}
