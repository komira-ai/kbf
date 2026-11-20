//! The flags: their defaults, what is passed on to the run, and everything the tool refuses
//! before it reads or writes anything.

use std::time::Duration;

use clap::Parser as _;
use kbf_l2_backfill::{Args, Error, Mode, Plan};

use crate::support::{ENDPOINT, SOCKET_URL, World, says, write_secret};

fn args(world: &World, endpoint: &str, index: &str, more: &[&str]) -> Args {
    let keys = world.credentials.to_str().expect("a path");
    let mut argv = vec![
        "kbf-l2-backfill",
        "--store-endpoint",
        endpoint,
        "--store-bucket",
        "kbf-fake",
        "--pack-index",
        index,
        "--store-credentials-file",
        keys,
    ];
    argv.extend_from_slice(more);
    Args::try_parse_from(argv).expect("the flags parse")
}

fn plan(world: &World, more: &[&str]) -> Result<Plan, Error> {
    args(world, ENDPOINT, SOCKET_URL, more).plan()
}

fn refusal(checked: Result<Plan, Error>) -> String {
    let Err(error) = checked else {
        panic!("the flags were accepted");
    };
    assert_eq!(error.code(), 2);
    let said = error.to_string();
    assert!(!said.contains('\n'), "{said}");
    said
}

/// Asserts the defaults: a dry run, 14 days, no rate, no limit, no sample, and a backfill pass.
#[test]
fn the_defaults_are_a_dry_run_over_fourteen_days_with_no_rate_limit_or_sample() {
    let world = World::new("flags-defaults");
    let plan = plan(&world, &[]).expect("accepted");

    assert_eq!(plan.mode, Mode::DryRun);
    assert_eq!(plan.within_days, 14);
    assert_eq!(plan.within, Duration::from_secs(14 * 86_400));
    assert_eq!(plan.rate, None);
    assert_eq!(plan.limit_trees, None);
    assert_eq!(plan.audit, None);
    assert!(plan.backfills());
}

/// Asserts that the flags reach the plan.
#[test]
fn the_flags_reach_the_plan() {
    let world = World::new("flags-reach");
    let more = [
        "--mode",
        "apply",
        "--within-days",
        "3",
        "--trees-per-second",
        "5",
        "--limit-trees",
        "9",
        "--audit-keys-out",
        "keys.txt",
        "--audit-keys",
        "11",
        "--audit-seed",
        "42",
        "--report-json",
        "r.json",
    ];
    let plan = plan(&world, &more).expect("accepted");

    assert_eq!(plan.mode, Mode::Apply);
    assert_eq!(plan.within, Duration::from_secs(3 * 86_400));
    assert_eq!(plan.rate.map(|r| r.get()), Some(5));
    assert_eq!(plan.limit_trees, Some(9));
    let audit = plan.audit.expect("a sample");
    assert_eq!(audit.out.to_str(), Some("keys.txt"));
    assert_eq!(audit.keys.get(), 11);
    assert_eq!(audit.seed, Some(42));
    assert_eq!(
        plan.report_json.as_deref().and_then(|p| p.to_str()),
        Some("r.json")
    );
}

/// Asserts that a dry run with a sample is the sample alone, and that an apply with one also
/// backfills.
#[test]
fn a_dry_run_with_a_sample_is_the_sample_alone_and_an_apply_backfills_too() {
    let world = World::new("flags-interplay");

    let alone = plan(&world, &["--audit-keys-out", "k"]).expect("accepted");
    let both = plan(&world, &["--audit-keys-out", "k", "--mode", "apply"]).expect("accepted");

    assert!(!alone.backfills());
    assert!(both.backfills());
}

/// Asserts the refusals, each one line, status 2.
#[test]
fn a_file_store_a_zero_limit_and_bad_files_are_refused() {
    let world = World::new("flags-refusals");

    says(
        &refusal(args(&world, "file:///x", SOCKET_URL, &[]).plan()),
        "file:// store is refused",
    );
    says(
        &refusal(plan(&world, &["--limit-trees", "0"])),
        "--limit-trees",
    );
    says(
        &refusal(args(&world, ENDPOINT, "not a url", &[]).plan()),
        "--pack-index: not a postgres:// URL",
    );
    let said = refusal(args(&world, ENDPOINT, "postgres://u:hunter2@db/kbf", &[]).plan());
    says(&said, "no password");
    assert!(!said.contains("hunter2"), "{said}");
    write_secret(&world.credentials, 0o644);
    says(&refusal(plan(&world, &[])), "lets others read a secret");
    write_secret(&world.credentials, 0o600);
    std::fs::write(&world.credentials, "not json").expect("written");
    says(&refusal(plan(&world, &[])), "not JSON with access_key_id");
    std::fs::remove_file(&world.credentials).expect("removed");
    says(&refusal(plan(&world, &[])), "--store-credentials-file");
}

/// Asserts that a store the client cannot be built for is refused.
#[test]
fn a_store_that_cannot_be_built_is_refused() {
    let world = World::new("flags-store");

    let said = refusal(args(&world, "ftp://nowhere", SOCKET_URL, &[]).plan());

    says(&said, "--store-endpoint or --store-bucket");
}

/// Asserts that `--audit-keys` and `--audit-seed` need `--audit-keys-out`, and that a window or
/// a sample of no size is a usage error.
#[test]
fn the_sample_flags_need_a_file_and_zeros_are_usage_errors() {
    let world = World::new("flags-usage");

    for more in [
        &["--audit-keys", "5"][..],
        &["--audit-seed", "5"][..],
        &["--within-days", "0"][..],
        &["--audit-keys-out", "k", "--audit-keys", "0"][..],
        &["--trees-per-second", "0"][..],
    ] {
        let ran = world.run(more);
        assert_eq!(ran.code, 2, "{more:?}: {}", ran.err);
        assert!(ran.out.is_empty());
        says(&ran.err, "error:");
    }
}
