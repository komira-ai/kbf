//! `--pack-store-retry-first-ms`, `--pack-store-retry-growth`, `--pack-store-retry-longest-secs`,
//! `--pack-store-retry-jitter-percent` and `--pack-store-retry-budget-secs`: the numbers of the
//! one retry of the frontend's calls to the pack store once it serves
//! ([`kbf_front::store_retry::RetryPolicy`], whose module says what is retried and why the
//! defaults are what they are); and `--pack-input-walk-secs`, the deadline of an Execute's walk
//! of its input tree over the packs ([`kbf_front::PackSettings::input_walk`]).
//!
//! Every read, length check, delete and listing of the packs' store, every pack written, and
//! every write of an action tree (whose budget is `--task-put-retry-secs`) is retried with these
//! waits. The calls of the start keep their own bound (`--pack-store-start-wait-secs`).

use std::time::Duration;

use clap::{Arg, ArgMatches};
use kbf_front::PackSettings;
use kbf_front::store_retry::RetryPolicy;

const FIRST_MS: &str = "pack_store_retry_first_ms";
const GROWTH: &str = "pack_store_retry_growth";
const LONGEST_SECS: &str = "pack_store_retry_longest_secs";
const JITTER_PERCENT: &str = "pack_store_retry_jitter_percent";
const BUDGET_SECS: &str = "pack_store_retry_budget_secs";
const INPUT_WALK_SECS: &str = "pack_input_walk_secs";

/// The defaults, which clap cannot take as computed strings; a test compares them with
/// [`RetryPolicy::DEFAULT`] and [`PackSettings::default`].
pub(super) const DEFAULT_FIRST_MS: &str = "500";
pub(super) const DEFAULT_GROWTH: &str = "2";
pub(super) const DEFAULT_LONGEST_SECS: &str = "8";
pub(super) const DEFAULT_JITTER_PERCENT: &str = "50";
pub(super) const DEFAULT_BUDGET_SECS: &str = "120";
pub(super) const DEFAULT_INPUT_WALK_SECS: &str = "120";

/// The longest first wait, in milliseconds: a minute.
const MAX_FIRST_MS: u64 = 60_000;

/// The most growth: ten times the wait before.
const MAX_GROWTH: u32 = 10;

/// The most `--pack-store-retry-longest-secs`: a minute, the store client's own wait for a byte
/// of an answer.
const MAX_LONGEST_SECS: u64 = 60;

/// The longest budget and walk deadline, in seconds: an hour.
const MAX_SECS: u64 = 3600;

/// The six flags.
pub(in crate::frontend) fn args() -> [Arg; 6] {
    [
        Arg::new(FIRST_MS)
            .long("pack-store-retry-first-ms")
            .value_name("MILLISECONDS")
            .value_parser(clap::value_parser!(u64).range(1..=MAX_FIRST_MS))
            .default_value(DEFAULT_FIRST_MS)
            .help(
                "The wait, in milliseconds, before the first retry of a call to the pack store \
                 that failed in a way that may pass: a refused or reset connection or one that \
                 got no answer, a 503 (SlowDown among them), a 500, 502 or 504, or a 400 \
                 RequestTimeout. Every other answer is the call's at once. Each wait after is \
                 `--pack-store-retry-growth` times the one before, up to \
                 `--pack-store-retry-longest-secs`, jittered by \
                 `--pack-store-retry-jitter-percent`, until `--pack-store-retry-budget-secs`. \
                 From 1 to 60000. Used only with `--pack-store-endpoint`",
            ),
        Arg::new(GROWTH)
            .long("pack-store-retry-growth")
            .value_name("TIMES")
            .value_parser(clap::value_parser!(u32).range(1..=i64::from(MAX_GROWTH)))
            .default_value(DEFAULT_GROWTH)
            .help(
                "How many times the wait before a retry of a call to the pack store is the one \
                 before it, from 1 (every wait is the first) to 10. Used only with \
                 `--pack-store-endpoint`",
            ),
        Arg::new(LONGEST_SECS)
            .long("pack-store-retry-longest-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(1..=MAX_LONGEST_SECS))
            .default_value(DEFAULT_LONGEST_SECS)
            .help(
                "The longest wait, in seconds, before one retry of a call to the pack store: \
                 the waits grow to this and stay. A first wait longer than this is this. From 1 \
                 to 60. Used only with `--pack-store-endpoint`",
            ),
        Arg::new(JITTER_PERCENT)
            .long("pack-store-retry-jitter-percent")
            .value_name("PERCENT")
            .value_parser(clap::value_parser!(u8).range(0..=100))
            .default_value(DEFAULT_JITTER_PERCENT)
            .help(
                "How much of each wait before a retry of a call to the pack store is drawn off \
                 at random, at most, in percent of it: a wait of w seconds lasts between \
                 w * (100 - this) / 100 and w, so the calls one restart of the store stopped \
                 together do not come back together. 0 waits exactly w. From 0 to 100. Used \
                 only with `--pack-store-endpoint`",
            ),
        Arg::new(BUDGET_SECS)
            .long("pack-store-retry-budget-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(0..=MAX_SECS))
            .default_value(DEFAULT_BUDGET_SECS)
            .help(
                "How long, in seconds from its first attempt, one call to the pack store is \
                 tried: no retry is made that would begin past this, and the store's last \
                 answer is then the call's, saying how many attempts were made and over how \
                 long (a client's call is answered UNAVAILABLE, which it makes again). An \
                 attempt in flight is not cut; the store client ends one that gets no byte for \
                 60 s. Once a call has spent its budget, the first failure of every call after \
                 it is its answer at once, until the store answers a call. 0 retries nothing. \
                 At most 3600. The default, 120, rides out a restart or a minute of 503s, and is \
                 under the 150 s the frontend's listener waits on a connection that moves no \
                 byte (`--http2-keepalive-interval-secs` and `--http2-keepalive-timeout-secs`); \
                 a call whose last attempt stalls may run past it by that attempt. The writes of action trees take \
                 `--task-put-retry-secs` as their budget instead. Used only with \
                 `--pack-store-endpoint`",
            ),
        Arg::new(INPUT_WALK_SECS)
            .long("pack-input-walk-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(1..=MAX_SECS))
            .default_value(DEFAULT_INPUT_WALK_SECS)
            .help(
                "How long, in seconds, an Execute's walk of its input tree over the packs (its \
                 reads of the Directories and its checks of the files) may take. A walk still \
                 running then is dropped, its reads with it, and the Execute is answered \
                 UNAVAILABLE, which the client makes again: never the action's own failure. The \
                 deadline is on the whole walk, so a walk that is merely slow (a very large tree) \
                 is cut too. From 1 to 3600. The default, 120, is the default budget of one read, so a \
                 store that stays down costs an Execute no more than one read's retries. Used \
                 only with `--pack-store-endpoint`",
            ),
    ]
}

/// What the flags in `m` set of the packs: the retry and the walk's deadline (clap gives the
/// default of each flag left out).
pub(super) fn from_matches(m: &ArgMatches) -> (RetryPolicy, Duration) {
    let default = RetryPolicy::DEFAULT;
    let secs =
        |id: &str, or: Duration| m.get_one::<u64>(id).map_or(or, |s| Duration::from_secs(*s));
    let policy = RetryPolicy {
        first: m
            .get_one::<u64>(FIRST_MS)
            .map_or(default.first, |ms| Duration::from_millis(*ms)),
        growth: m.get_one::<u32>(GROWTH).copied().unwrap_or(default.growth),
        longest: secs(LONGEST_SECS, default.longest),
        jitter_percent: m
            .get_one::<u8>(JITTER_PERCENT)
            .copied()
            .unwrap_or(default.jitter_percent),
        budget: secs(BUDGET_SECS, default.budget),
    };
    let walk = secs(INPUT_WALK_SECS, PackSettings::default().input_walk);
    (policy, walk)
}

#[cfg(test)]
#[path = "retry_tests.rs"]
mod tests;
