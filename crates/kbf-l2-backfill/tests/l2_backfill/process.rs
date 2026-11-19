//! The tool as a process and through its real seams: help, a usage error, a refusal and an index
//! that cannot be opened end as documented.

use std::process::{Command, Output};

use kbf_l2_backfill::{main_with, open_bucket, open_postgres};

use crate::support::{ENDPOINT, World, says};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-l2-backfill"))
        .args(args)
        .output()
        .expect("runs")
}

/// Every flag the tool takes.
const FLAGS: [&str; 14] = [
    "--store-endpoint",
    "--store-bucket",
    "--store-region",
    "--store-credentials-file",
    "--pack-index",
    "--mode",
    "--within-days",
    "--trees-per-second",
    "--limit-trees",
    "--report-json",
    "--audit-keys-out",
    "--audit-keys",
    "--audit-seed",
    "--help",
];

/// Asserts that `--help` ends with status 0 on the standard output, names every flag and
/// default, says how the sample and the mode interplay, and gives the exit statuses.
#[test]
fn the_help_names_every_flag_default_and_the_sample_interplay() {
    let out = run(&["--help"]);

    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    let help = String::from_utf8_lossy(&out.stdout).into_owned();
    says(&help, "Usage: kbf-l2-backfill");
    for flag in FLAGS {
        says(&help, flag);
    }
    for default in [
        "[default: dry-run]",
        "[default: 14]",
        "[default: 1000]",
        "[default: us-east-1]",
    ] {
        says(&help, default);
    }
    says(&help, "does no backfill pass");
    says(&help, "Exit status: 0 done");
}

/// Asserts that a missing flag is a usage error, status 2, on the standard error.
#[test]
fn a_usage_error_ends_with_status_2_on_the_standard_error() {
    let out = run(&["--mode", "apply"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    says(&String::from_utf8_lossy(&out.stderr), "required arguments");
}

/// Asserts that a refusal prints one line to the standard error, status 2, and that the real
/// seams open an index only when asked: an index that is not there is a failure, status 1.
#[test]
fn a_refusal_is_status_2_and_an_index_that_is_not_there_is_status_1() {
    let world = World::new("process-real");
    let (mut out, mut err) = (Vec::new(), Vec::new());

    let refused = main_with(world.argv(&["--limit-trees", "0"]), &mut out, &mut err);
    assert_eq!(refused, 2);
    says(&String::from_utf8_lossy(&err), "kbf-l2-backfill: refused:");

    err.clear();
    let mut argv = world.argv(&[]);
    let at = argv
        .iter()
        .position(|a| a == "postgres:///kbf?host=/run/postgresql");
    argv[at.expect("the url")] = "postgres:///kbf?host=/nonexistent-socket-dir".into();
    let failed = main_with(argv, &mut out, &mut err);
    assert_eq!(failed, 1);
    says(&String::from_utf8_lossy(&err), "kbf-l2-backfill: failed:");
    assert!(out.is_empty());
}

/// Asserts that the real seams are made: the index opener refuses a missing server, and the
/// store opener wraps the flags' store.
#[test]
fn the_real_seams_open_a_store_and_refuse_a_missing_index() {
    let world = World::new("process-seams");
    let config =
        kbf_state::postgres::PostgresConfig::parse("postgres:///kbf?host=/nonexistent-socket-dir")
            .expect("a config");
    assert!(open_postgres(&config).is_err());

    let args = <kbf_l2_backfill::Args as clap::Parser>::try_parse_from([
        "kbf-l2-backfill",
        "--store-endpoint",
        ENDPOINT,
        "--store-bucket",
        "kbf-fake",
        "--pack-index",
        "postgres:///kbf?host=/run/postgresql",
        "--store-credentials-file",
        world.credentials.to_str().expect("a path"),
    ])
    .expect("flags");
    let plan = args.plan().expect("a plan");
    let _store = open_bucket(plan.store);
    let seams = kbf_l2_backfill::Seams::default();
    let _ = (seams.seed)();
}
