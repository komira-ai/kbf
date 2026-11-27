//! The retry flags of the pack store: their defaults, what each sets, and their bounds.

use std::time::Duration;

use clap::error::ErrorKind;
use kbf_front::PackSettings;
use kbf_front::store_retry::RetryPolicy;

use super::{
    DEFAULT_BUDGET_SECS, DEFAULT_FIRST_MS, DEFAULT_GROWTH, DEFAULT_INPUT_WALK_SECS,
    DEFAULT_JITTER_PERCENT, DEFAULT_LONGEST_SECS,
};
use crate::frontend::{self, FrontendFlags};

/// What a frontend needs of the command line, with a pack store.
const BASE: [&str; 10] = [
    "frontend",
    "--storage=http://127.0.0.1:1",
    "--listen=127.0.0.1:0",
    "--worker-listen=127.0.0.1:0",
    "--api-listen=127.0.0.1:0",
    "--state-store=postgres:///kbf?host=/run/postgresql",
    "--pack-index",
    "--pack-store-endpoint=http://127.0.0.1:9",
    "--pack-store-bucket=kbf-packs",
    "--pack-store-credentials-file=/etc/kbf/pack.json",
];

/// The flags given `more` after the base.
fn parsed(more: &[&str]) -> Result<FrontendFlags, clap::Error> {
    let argv = BASE.iter().chain(more).copied();
    frontend::parse(&frontend::command().try_get_matches_from(argv)?)
}

/// Asserts that the default strings of the flags are the numbers of [`RetryPolicy::DEFAULT`]
/// and of the packs' default input walk, so the help and the code cannot drift apart.
/// Catches a default changed in one place and not the other.
#[test]
fn the_default_strings_are_the_defaults_of_the_policy_and_the_walk() {
    let policy = RetryPolicy::DEFAULT;
    assert_eq!(
        DEFAULT_FIRST_MS.parse::<u128>().expect("number"),
        policy.first.as_millis()
    );
    assert_eq!(
        DEFAULT_GROWTH.parse::<u32>().expect("number"),
        policy.growth
    );
    assert_eq!(
        DEFAULT_LONGEST_SECS.parse::<u64>().expect("number"),
        policy.longest.as_secs()
    );
    assert_eq!(
        DEFAULT_JITTER_PERCENT.parse::<u8>().expect("number"),
        policy.jitter_percent
    );
    assert_eq!(
        DEFAULT_BUDGET_SECS.parse::<u64>().expect("number"),
        policy.budget.as_secs()
    );
    assert_eq!(
        DEFAULT_INPUT_WALK_SECS.parse::<u64>().expect("number"),
        PackSettings::default().input_walk.as_secs()
    );
}

/// Asserts that with no retry flag given the packs get the default policy and walk deadline.
/// Catches a flag left out reading as zero rather than its default.
#[test]
fn left_out_the_flags_give_the_defaults() {
    let packs = parsed(&[]).expect("parses").packs.expect("packs");
    assert_eq!(packs.retry, RetryPolicy::DEFAULT);
    assert_eq!(packs.input_walk, PackSettings::default().input_walk);
}

/// Asserts that every flag given reaches the policy or the walk deadline of the pack flags.
/// Catches a flag parsed and dropped, or two flags swapped.
#[test]
fn each_flag_sets_its_own_number() {
    let packs = parsed(&[
        "--pack-store-retry-first-ms=250",
        "--pack-store-retry-growth=3",
        "--pack-store-retry-longest-secs=20",
        "--pack-store-retry-jitter-percent=10",
        "--pack-store-retry-budget-secs=45",
        "--pack-input-walk-secs=90",
    ])
    .expect("parses")
    .packs
    .expect("packs");
    assert_eq!(
        packs.retry,
        RetryPolicy {
            first: Duration::from_millis(250),
            growth: 3,
            longest: Duration::from_secs(20),
            jitter_percent: 10,
            budget: Duration::from_secs(45),
        }
    );
    assert_eq!(packs.input_walk, Duration::from_secs(90));
}

/// Asserts the ends of each range are taken: a budget of 0 (no retry), jitter 0 and 100, growth
/// 1 and 10, and the largest of the rest.
/// Catches a range that is one short at an end.
#[test]
fn the_ends_of_the_ranges_are_taken() {
    let low = parsed(&[
        "--pack-store-retry-first-ms=1",
        "--pack-store-retry-growth=1",
        "--pack-store-retry-longest-secs=1",
        "--pack-store-retry-jitter-percent=0",
        "--pack-store-retry-budget-secs=0",
        "--pack-input-walk-secs=1",
    ])
    .expect("parses")
    .packs
    .expect("packs");
    assert_eq!(low.retry.budget, Duration::ZERO);
    assert_eq!(low.retry.jitter_percent, 0);
    assert_eq!(low.input_walk, Duration::from_secs(1));
    let high = parsed(&[
        "--pack-store-retry-first-ms=60000",
        "--pack-store-retry-growth=10",
        "--pack-store-retry-longest-secs=60",
        "--pack-store-retry-jitter-percent=100",
        "--pack-store-retry-budget-secs=3600",
        "--pack-input-walk-secs=3600",
    ])
    .expect("parses")
    .packs
    .expect("packs");
    assert_eq!(high.retry.first, Duration::from_millis(60_000));
    assert_eq!(high.retry.growth, 10);
    assert_eq!(high.retry.longest, Duration::from_secs(60));
    assert_eq!(high.retry.jitter_percent, 100);
    assert_eq!(high.retry.budget, Duration::from_secs(3600));
    assert_eq!(high.input_walk, Duration::from_secs(3600));
}

/// Asserts that a value outside its range is refused by the command line parser.
/// Catches a bound dropped from a flag.
#[test]
fn values_out_of_range_are_refused() {
    for flag in [
        "--pack-store-retry-growth=0",
        "--pack-store-retry-growth=11",
        "--pack-store-retry-jitter-percent=101",
        "--pack-store-retry-longest-secs=0",
        "--pack-store-retry-longest-secs=61",
        "--pack-store-retry-first-ms=0",
        "--pack-store-retry-first-ms=60001",
        "--pack-store-retry-budget-secs=3601",
        "--pack-input-walk-secs=0",
        "--pack-input-walk-secs=3601",
    ] {
        let error = parsed(&[flag]).expect_err(flag);
        assert_eq!(error.kind(), ErrorKind::ValueValidation, "{flag}");
    }
}
