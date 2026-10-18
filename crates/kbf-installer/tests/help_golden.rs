//! `kbf-installer --help` and each verb's `--help` against `tests/golden/help.txt`, byte for
//! byte: the command line is a contract with scripts, units and the package, so a verb, a
//! flag or a sentence of help changing moves the file in the same change, and the reviewer
//! sees it. (The CLI is defined in one file, `src/cli.rs`; if the verb form changes to
//! `--action=<verb>` this golden shows every line that moves.)
//!
//! To rewrite the golden after an intended change, run the test: it leaves the text it saw
//! in `target/help.actual` (next to the test binary's `deps` directory) and says where.

use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-installer");

/// The visible verbs, and the nested `envoy` ones, in the order the help lists them.
const VERBS: [&[&str]; 15] = [
    &[],
    &["setup"],
    &["status"],
    &["run"],
    &["update"],
    &["confirm"],
    &["revert"],
    &["rollback"],
    &["gc"],
    &["boot-check"],
    &["quiesce"],
    &["envoy"],
    &["envoy", "fetch"],
    &["envoy", "list"],
    &["check"],
];

fn help(args: &[&str]) -> String {
    let out = Command::new(BIN)
        .args(args)
        .arg("--help")
        .output()
        .expect("runs");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("text")
}

#[test]
fn the_help_matches_the_golden_file() {
    let mut actual = String::new();
    for verb in VERBS {
        actual.push_str(&format!(
            "=== {} --help ===\n",
            format!("kbf-installer {}", verb.join(" ")).trim_end()
        ));
        actual.push_str(&help(verb));
        actual.push('\n');
    }
    let golden = include_str!("golden/help.txt");
    if actual != golden {
        let exe = std::env::current_exe().expect("path");
        let target: PathBuf = exe.ancestors().nth(3).expect("target dir").to_path_buf();
        let at = target.join("help.actual");
        std::fs::write(&at, &actual).expect("write");
        panic!(
            "the help changed; if that is intended, copy {} over crates/kbf-installer/tests/golden/help.txt",
            at.display()
        );
    }
}
