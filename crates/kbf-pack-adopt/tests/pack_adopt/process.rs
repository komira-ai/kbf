//! The tool as a process: help, version, a usage error and the refusals that need no store or
//! database end as documented.

use std::process::{Command, Output};

use crate::support::says;

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-pack-adopt"))
        .args(args)
        .output()
        .expect("runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Every flag the tool takes.
const FLAGS: [&str; 15] = [
    "--source-store-endpoint",
    "--source-store-bucket",
    "--source-prefix",
    "--source-store-region",
    "--source-store-credentials-file",
    "--pack-index",
    "--mode",
    "--pack-store-bucket",
    "--checkpoint-file",
    "--segments-per-second",
    "--footer-concurrency",
    "--limit-segments",
    "--verify-sample",
    "--report-json",
    "--fail-on-conflict",
];

/// The defaults the help states.
const DEFAULTS: [&str; 4] = [
    "[default: dry-run]",
    "[default: kbf/]",
    "[default: us-east-1]",
    "[default: 4]",
];

/// Asserts that `--help` ends with status 0 and prints to the standard output the usage, every
/// flag, the defaults (a dry run, the prefix `kbf/`, the region `us-east-1`, four footer
/// reads) and the exit statuses, and nothing to the standard error.
#[test]
fn the_help_names_every_flag_and_default_and_ends_well_on_the_standard_output() {
    let out = run(&["--help"]);

    assert_eq!(out.status.code(), Some(0));
    let help = stdout(&out);
    says(&help, "Usage: kbf-pack-adopt");
    for flag in FLAGS {
        says(&help, flag);
    }
    for default in DEFAULTS {
        says(&help, default);
    }
    says(&help, "Exit status: 0 done");
    assert!(out.stderr.is_empty());
}

/// Asserts that `--version` ends with status 0 and names the program.
#[test]
fn the_version_names_the_program() {
    let out = run(&["--version"]);

    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).starts_with("kbf-pack-adopt "));
}

/// Asserts that a command line without the flags the tool needs ends with status 2, names a
/// missing flag on the standard error and prints nothing to the standard output.
#[test]
fn a_command_line_without_the_required_flags_ends_with_status_2() {
    let out = run(&[]);

    assert_eq!(out.status.code(), Some(2));
    assert!(stdout(&out).is_empty());
    says(&stderr(&out), "--source-store-endpoint");
}

/// Asserts that a `file://` source and an apply without the pack bucket each end with status 2
/// and one line on the standard error, and that nothing is printed to the standard output.
#[test]
fn a_file_source_and_an_apply_without_the_pack_bucket_end_with_status_2_and_one_line() {
    let rest = [
        "--source-store-bucket",
        "kbf-fake",
        "--source-store-credentials-file",
        "absent.json",
        "--pack-index",
        "postgres:///kbf?host=/run/postgresql",
    ];
    let mut file_source = vec!["--source-store-endpoint", "file:///var/lib/members"];
    file_source.extend(rest);
    let mut apply = vec!["--source-store-endpoint", "http://127.0.0.1:1"];
    apply.extend(["--mode", "apply"]);
    apply.extend(rest);

    for args in [file_source, apply] {
        let out = run(&args);

        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(stdout(&out).is_empty(), "{args:?}");
        let said = stderr(&out);
        assert_eq!(said.lines().count(), 1, "{said}");
        says(&said, "kbf-pack-adopt: refused: ");
    }
}
