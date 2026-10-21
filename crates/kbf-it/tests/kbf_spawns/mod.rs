//! The `--config` rule for test code that starts the `kbf` binary.
//!
//! `kbf` reads `/etc/kbf/kbf.toml` when it is given no `--config` and the file exists, so
//! a test that starts it bare passes or fails by the host it runs on (a host with kbf
//! installed has the file, owned by the service user and unreadable to the test's).
//! Every site that starts the binary (`CARGO_BIN_EXE_kbf`, a `bin/kbf` path given to
//! `Command::new` or a link called `kbf`) in test code is in one of two explicit lists:
//!
//! - [`FLAGGED`]: the helpers that add `kbf_testkit::config::flag(..)` to what they start
//!   (the harness is handed the binary's path, so it need not name it). The file must still
//!   call it, so a helper that loses the flag is refused.
//! - [`ALLOWANCE`]: the files that start `kbf` without it today, listed and counted. The
//!   list only shrinks: a file not on it is refused, a file on it that no longer names the
//!   binary is refused as stale, and the count is pinned by the test.
//!
//! The scan is textual (a comment line is skipped), so a spawn that builds the path from
//! pieces is not seen; the rule catches the way every site is written today.

/// Helpers that start `kbf` with the test's own settings file. Key: the path.
pub const FLAGGED: &[&str] = &[
    "crates/kbf-cli/tests/storage_process.rs",
    "crates/kbf-harness/src/farm.rs",
    "crates/kbf-it/tests/proxy_support/mod.rs",
];

/// Test files that start `kbf` with no settings file of their own: the process tests of
/// `kbf-cli`, each with its own runner and several of them testing the default path or a
/// named `--config` on purpose. Each is to be ported to the flag, one file at a time.
pub const ALLOWANCE: &[&str] = &[
    "crates/kbf-cli/tests/all_in_one.rs",
    "crates/kbf-cli/tests/beta_b0.rs",
    "crates/kbf-cli/tests/beta_b0_killed.rs",
    "crates/kbf-cli/tests/beta_b0_red.rs",
    "crates/kbf-cli/tests/beta_support/mod.rs",
    "crates/kbf-cli/tests/binary.rs",
    "crates/kbf-cli/tests/cluster_support/mod.rs",
    "crates/kbf-cli/tests/config_process.rs",
    "crates/kbf-cli/tests/frontend_handover.rs",
    "crates/kbf-cli/tests/frontend_process.rs",
    "crates/kbf-cli/tests/help_golden.rs",
    "crates/kbf-cli/tests/helpers.rs",
    "crates/kbf-cli/tests/macos_daemon.rs",
    "crates/kbf-cli/tests/node_process.rs",
    "crates/kbf-cli/tests/preflight.rs",
    "crates/kbf-cli/tests/rehearsal_in_a_box.rs",
    "crates/kbf-cli/tests/storage_and_daemon.rs",
];

/// The call a [`FLAGGED`] file must keep.
const FLAG_CALL: &str = "config::flag(";

/// The directory of this rule's own planted sources, which name the markers.
const OWN: &str = "crates/kbf-it/tests/kbf_spawns/";

/// Whether `path` is test code the rule reads: a file under a crate's `tests/`, or the
/// harness and test kit sources (which start `kbf` for tests), except this rule's own.
fn is_read(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let rust = path.ends_with(".rs") && !path.starts_with(OWN);
    let crate_dir = parts.first() == Some(&"crates") && parts.len() > 3;
    let tests = crate_dir && parts[2] == "tests";
    let support =
        crate_dir && parts[2] == "src" && matches!(parts[1], "kbf-harness" | "kbf-testkit");
    rust && (tests || support)
}

/// Whether the code of `text` (comment lines left out) starts or links the `kbf` binary:
/// its cargo path anywhere, or a `bin/kbf` path or a `join("kbf")` within two lines of a
/// `Command::new(` or `symlink(` (a fixture that only writes or copies a file called `kbf`
/// is not a start).
fn names_kbf(text: &str) -> bool {
    let q = '"';
    let (cargo, bin, link) = (
        format!("CARGO_BIN_EXE_kbf{q}"),
        format!("{q}bin/kbf{q}"),
        format!("join({q}kbf{q})"),
    );
    let lines: Vec<&str> = text
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("//") {
                ""
            } else {
                l
            }
        })
        .collect();
    lines.iter().enumerate().any(|(i, l)| {
        l.contains(&cargo)
            || ((l.contains(&bin) || l.contains(&link))
                && lines[i.saturating_sub(2)..=i]
                    .iter()
                    .any(|w| w.contains("Command::new(") || w.contains("symlink(")))
    })
}

/// The files read that name the binary, and the problems found: a [`FLAGGED`] file that does
/// not call the flag (it need not name the binary: the harness is handed the path), a file on
/// neither list, an [`ALLOWANCE`] file naming nothing (stale), and a file on both lists.
pub fn scan(
    files: &[String],
    read: impl Fn(&str) -> Option<String>,
    (flagged_list, allowance): (&[&str], &[&str]),
) -> (Vec<String>, Vec<String>) {
    let mut naming = Vec::new();
    let mut problems = Vec::new();
    let mut seen = 0;
    for f in files.iter().filter(|f| is_read(f)) {
        let Some(text) = read(f) else { continue };
        seen += 1;
        if names_kbf(&text) {
            naming.push(f.clone());
        }
    }
    if seen == 0 {
        problems.push("no test file was read; the scan reads too little".to_owned());
    }
    for f in flagged_list {
        if allowance.contains(f) {
            problems.push(format!("`{f}` is on both lists; remove it from ALLOWANCE"));
        } else if !read(f).is_some_and(|t| t.contains(FLAG_CALL)) {
            problems.push(format!(
                "`{f}` is a flagged helper but no longer calls `{FLAG_CALL}..)` (or is gone); \
                 the child it starts would read the host's /etc/kbf/kbf.toml"
            ));
        }
    }
    for f in &naming {
        if !flagged_list.contains(&f.as_str()) && !allowance.contains(&f.as_str()) {
            problems.push(format!(
                "`{f}` starts the `kbf` binary and is on neither list: pass `--config` with \
                 `kbf_testkit::config::flag(dir)` where it starts the child (a helper that does \
                 goes on FLAGGED), never on ALLOWANCE"
            ));
        }
    }
    for f in allowance {
        if !naming.iter().any(|n| n == f) {
            problems.push(format!(
                "`{f}` is on ALLOWANCE but starts no `kbf` binary (deleted, renamed, or ported \
                 off it); remove the row"
            ));
        }
    }
    (naming, problems)
}

/// The host's own paths: a test that opens, lists, writes or runs one of these reads whatever
/// the box it runs on has there (a host with kbf installed has `/etc/kbf/kbf.toml` and a
/// `/var/lib/kbf-installer` it cannot read), so it passes or fails by the host.
pub const HOST_PATHS: &[&str] = &["/var/lib/kbf-installer", "/etc/kbf"];

/// What on a line makes a host-path literal a use of the path (a file operation, a path
/// built from it, a child started with it) rather than text a test compares with.
const PATH_USE: &[&str] = &[
    "fs::",
    "File::",
    "Path::new(",
    "PathBuf::from(",
    "read_to_string",
    "read_dir",
    "metadata(",
    ".exists()",
    ".is_dir()",
    ".is_file()",
    "canonicalize",
    ".join(",
    "Command",
    ".arg(",
];

/// Test files that use a host path on purpose, with how many lines do (the count is pinned,
/// so a second use in the same file is refused until it is argued here). Key: path, count,
/// reason.
pub const HOST_PATH_ALLOWANCE: &[(&str, usize, &str)] = &[(
    "crates/kbf-cli/tests/deploy_units.rs",
    2,
    "asserts the default of `api-token-file` as a `Path` value; nothing is opened",
)];

/// Lines of `text` (comments left out) that name a [`HOST_PATHS`] literal in a path use.
fn host_path_uses(text: &str) -> usize {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| HOST_PATHS.iter().any(|h| l.contains(&format!("\"{h}"))))
        .filter(|l| PATH_USE.iter().any(|u| l.contains(u)))
        .count()
}

/// The problems of the host-path rule over the files read: a use not allowed, a count that
/// differs from the row, a row naming a file that is gone or has no use any more.
pub fn scan_host_paths(
    files: &[String],
    read: impl Fn(&str) -> Option<String>,
    allowance: &[(&str, usize, &str)],
) -> Vec<String> {
    let mut problems = Vec::new();
    for f in files.iter().filter(|f| is_read(f)) {
        let Some(text) = read(f) else { continue };
        let n = host_path_uses(&text);
        let allowed = allowance.iter().find(|(p, ..)| p == f).map_or(0, |r| r.1);
        if n != allowed {
            problems.push(format!(
                "`{f}` uses a host path ({n} lines naming {HOST_PATHS:?}, {allowed} allowed): \
                 root the test in a `TestDir` and pass it by the binary's root flag or build the \
                 paths under it; a test that asserts a default on purpose gets a row in \
                 HOST_PATH_ALLOWANCE with its reason"
            ));
        }
    }
    for (p, _, why) in allowance {
        if why.is_empty() || !files.iter().any(|f| f == p) {
            problems.push(format!(
                "`{p}` is on HOST_PATH_ALLOWANCE without a reason or a file"
            ));
        }
    }
    problems
}

/// The scan over planted trees: each defect the rule refuses, against a clean control.
#[cfg(test)]
mod planted {
    use super::{names_kbf, scan};
    use std::collections::BTreeMap;

    const HELPER: &str = "crates/k/tests/support.rs";
    const OLD: &str = "crates/k/tests/old.rs";
    const LISTS: (&[&str], &[&str]) = (&[HELPER], &[OLD]);

    fn spawn(extra: &str) -> String {
        format!("fn start() {{ Command::new(env!(\"CARGO_BIN_EXE_kbf\")){extra}; }}")
    }

    fn tree() -> BTreeMap<String, String> {
        [
            (HELPER, spawn(".arg(kbf_testkit::config::flag(d))")),
            (OLD, spawn("")),
            ("crates/k/tests/plain.rs", "fn t() { g(\"echo\") }".to_owned()),
            ("crates/k/tests/other.rs", spawn_act()),
            (
                "crates/k/tests/fixture.rs",
                "fn f() { fs::write(dir.join(\"kbf\"), b\"x\"); fs::copy(a, r.join(\"bin/kbf\")); }"
                    .to_owned(),
            ),
        ]
        .into_iter()
        .map(|(p, t)| (p.to_owned(), t))
        .collect()
    }

    /// Another binary of the workspace, whose name starts with `kbf`.
    fn spawn_act() -> String {
        "const A: &str = env!(\"CARGO_BIN_EXE_kbf-act\");".to_owned()
    }

    fn problems(t: &BTreeMap<String, String>) -> Vec<String> {
        let files: Vec<String> = t.keys().cloned().collect();
        scan(&files, |f| t.get(f).cloned(), LISTS).1
    }

    #[test]
    fn the_control_tree_passes() {
        // Catches: the control refused (a flagged helper, a listed allowance, a file
        // naming `kbf-act`, which is another binary), so each defect below would go red
        // for the wrong reason.
        assert_eq!(problems(&tree()), Vec::<String>::new());
    }

    #[test]
    fn a_new_unflagged_spawn_is_refused_in_every_spelling() {
        // Catches (planted defect): a test or harness file that starts `kbf` by the cargo
        // path, a `bin/kbf` path or a link, with no flag and no row, in a tests/ directory
        // and in the harness sources.
        let q = '"';
        let plants = [
            (
                "crates/k/tests/new.rs",
                format!("fn f() {{ Command::new(env!({q}CARGO_BIN_EXE_kbf{q})); }}"),
            ),
            (
                "crates/k/tests/sub/mod.rs",
                format!("fn f() {{ Command::new(dir.join({q}bin/kbf{q})); }}"),
            ),
            (
                "crates/kbf-harness/src/other.rs",
                format!("fn f() {{ symlink(a, bins.join({q}kbf{q})); }}"),
            ),
        ];
        for (path, text) in plants {
            let mut t = tree();
            t.insert(path.to_owned(), text);
            let got = problems(&t);
            assert_eq!(got.len(), 1, "{path}: {got:?}");
            assert!(
                got[0].starts_with(&format!("`{path}` starts the `kbf` binary")),
                "{got:?}"
            );
        }
    }

    #[test]
    fn a_helper_that_loses_its_flag_is_refused() {
        // Catches (planted defect): a flagged helper edited to start the child bare.
        let mut t = tree();
        t.insert(HELPER.to_owned(), spawn(""));
        let got = problems(&t);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("no longer calls"), "{got:?}");
    }

    #[test]
    fn a_stale_row_is_refused() {
        // Catches (planted defect): a row for a file that is gone or no longer names the
        // binary, so the allowance stops shrinking with the code.
        let mut gone = tree();
        gone.remove(OLD);
        let mut ported = tree();
        ported.insert(OLD.to_owned(), "fn t() {}".to_owned());
        for t in [gone, ported] {
            let got = problems(&t);
            assert_eq!(got.len(), 1, "{got:?}");
            assert!(got[0].contains("starts no `kbf` binary"), "{got:?}");
        }
    }

    #[test]
    fn a_comment_does_not_count_and_an_empty_scan_is_refused() {
        // Catches: a doc comment that mentions the binary read as a spawn; a scan that
        // read no test file at all passing.
        assert!(!names_kbf(&format!(
            "/// the {{}}\n// CARGO_BIN_EXE_kbf{}",
            '"'
        )));
        let got = scan(&[], |_| None, (&[], &[])).1;
        assert_eq!(
            got,
            vec!["no test file was read; the scan reads too little".to_owned()]
        );
    }

    use super::scan_host_paths;

    const ALLOW: &[(&str, usize, &str)] =
        &[("crates/k/tests/default.rs", 1, "asserts the default")];

    fn host_problems(t: &BTreeMap<String, String>) -> Vec<String> {
        let files: Vec<String> = t.keys().cloned().collect();
        scan_host_paths(&files, |f| t.get(f).cloned(), ALLOW)
    }

    fn host_tree() -> BTreeMap<String, String> {
        [
            (
                "crates/k/tests/default.rs",
                "let p = Path::new(\"/etc/kbf/api-token\");",
            ),
            (
                "crates/k/tests/rooted.rs",
                "let p = root.join(\"var/lib/kbf-installer\"); // /etc/kbf in a comment\nassert_eq!(s, \"/etc/kbf/kbf.toml\");",
            ),
        ]
        .into_iter()
        .map(|(p, t)| (p.to_owned(), t.to_owned()))
        .collect()
    }

    #[test]
    fn host_paths_control_passes() {
        // Catches: the control refused (an allowed default, a rooted join, a comment and
        // a compared string), so the defects below would go red for the wrong reason.
        assert_eq!(host_problems(&host_tree()), Vec::<String>::new());
    }

    #[test]
    fn a_host_path_opened_by_a_test_is_refused_and_an_extra_allowed_use_too() {
        // Catches (planted defects): a test reading the host's real directory, in each
        // spelling, and a second use in a file with a one-use row.
        let plants = [
            (
                "crates/k/tests/new.rs",
                "let t = fs::read_to_string(\"/etc/kbf/kbf.toml\");",
            ),
            (
                "crates/k/tests/sub/mod.rs",
                "let d = PathBuf::from(\"/var/lib/kbf-installer\");",
            ),
            (
                "crates/k/tests/default.rs",
                "let p = Path::new(\"/etc/kbf/api-token\");\nlet q = Path::new(\"/etc/kbf/x\");",
            ),
        ];
        for (path, text) in plants {
            let mut t = host_tree();
            t.insert(path.to_owned(), text.to_owned());
            let got = host_problems(&t);
            assert_eq!(got.len(), 1, "{path}: {got:?}");
            assert!(
                got[0].starts_with(&format!("`{path}` uses a host path")),
                "{got:?}"
            );
        }
    }

    #[test]
    fn a_stale_host_path_row_is_refused() {
        // Catches (planted defect): a row for a file that is gone, which would let a
        // future file of that name use the host path.
        let mut t = host_tree();
        t.remove("crates/k/tests/default.rs");
        let got = host_problems(&t);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("without a reason or a file"), "{got:?}");
    }
}
