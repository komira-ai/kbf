//! The bytes of the `update/desired` and `update/policy` rows (`docs/design/pull-updates.md`
//! section 6.2).
//!
//! Each row is a protobuf message written with `prost`, as `member/<id>` is
//! (`crate::members`): an unknown field is skipped, so a later version may add one, and a
//! field the reader does not find is its zero value. So **a field added later must be valid
//! at zero** (or optional), and no field changes its number or meaning (the state-store rule
//! of section 3.4: a row is frozen across one step). `tests/update_rows.rs` holds the bytes
//! of one value of each row, and fails if they move.
//!
//! `update/desired.source` is where the run's release comes from, and the node step's limits
//! in `update/policy` are each optional and absent when they are the default, so that a
//! writer that knows only the agreed fields leaves them at the default instead of at zero.
//! `paused` is an agreed field of `update/policy`; it took the next free number there, 18,
//! because no field changes its number. The rows older builds kept for hosts and places are
//! read by [`crate::stored_update`].
//!
//! **`source` is the one enum where zero is valid.** A row written before the field existed
//! has no tag for it, and that row is a fetch, what every run meant then: 0 (or an absent tag)
//! reads [`Source::Fetch`], 1 reads [`Source::Staged`], and a fetch is not written, so the
//! bytes of every row that exists are unchanged. A number above 1 is [`Corrupt`], as any
//! unknown enum number is.

use std::time::Duration;

use kbf_types::FarmTime;
use prost::Message as _;

use super::policy::Policy;
use super::run::{Generation, Release, Source};

/// What is wrong with a row's bytes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Corrupt(pub(super) String);

fn corrupt<T>(why: impl Into<String>) -> Result<T, Corrupt> {
    Err(Corrupt(why.into()))
}

/// `update/desired`: the run the farm is driven to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Desired {
    /// The generation of the run. It grows with every change of the desired release; a
    /// rollback is the older commit under a higher generation.
    pub generation: Generation,
    /// The release every host is driven to (the design's `commit`).
    pub commit: Release,
    /// When the release was set: where the idle wait counts from until a host finishes.
    pub set_at: FarmTime,
    /// Where the release comes from. It is part of the run: the same commit under another
    /// source is a new generation.
    pub source: Source,
}

/// Tag 4 was `paused` in an earlier draft of this row, which no release wrote (the pause is
/// `update/policy`'s); the number is not reused. `source` is 0 (fetch, not written) or 1
/// (staged).
#[derive(Clone, PartialEq, prost::Message)]
struct DesiredMsg {
    #[prost(uint64, tag = "1")]
    generation: u64,
    #[prost(string, tag = "2")]
    commit: String,
    #[prost(uint64, tag = "3")]
    set_at_ms: u64,
    #[prost(uint32, tag = "5")]
    source: u32,
}

/// Fields 1 to 8 are the agreed ones, in the design's order of the time; 9 to 17 are the node
/// step's limits; 18 is `paused`, an agreed field that came later (absent is not paused).
/// `idle_wait`, `max_hosts_out` and every limit are optional: absent is the default of
/// [`Policy::default`] (so `max_hosts_out` absent is 1, not the invalid 0), and an explicit
/// zero is a zero.
#[derive(Clone, PartialEq, prost::Message)]
struct PolicyMsg {
    #[prost(bool, tag = "1")]
    auto_nodes: bool,
    #[prost(bool, tag = "2")]
    auto_frontend: bool,
    #[prost(uint64, optional, tag = "3")]
    idle_wait_ms: Option<u64>,
    #[prost(uint64, tag = "4")]
    min_serving: u64,
    #[prost(uint64, optional, tag = "5")]
    max_hosts_out: Option<u64>,
    #[prost(uint64, optional, tag = "6")]
    mac_force_after_ms: Option<u64>,
    #[prost(bool, tag = "7")]
    new_work_only_to_updated: bool,
    #[prost(bool, tag = "8")]
    accept_failing_builds: bool,
    #[prost(uint64, optional, tag = "9")]
    ttl_ms: Option<u64>,
    #[prost(uint64, optional, tag = "10")]
    dwell_ms: Option<u64>,
    #[prost(uint64, optional, tag = "11")]
    lease_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "12")]
    mac_lease_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "13")]
    write_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "14")]
    activate_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "15")]
    health_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "16")]
    rebuild_limit_ms: Option<u64>,
    #[prost(uint64, optional, tag = "17")]
    repair_wait_ms: Option<u64>,
    #[prost(bool, tag = "18")]
    paused: bool,
}

/// The number a source is written as: a fetch is 0, which the row leaves out.
const fn source_code(source: Source) -> u32 {
    match source {
        Source::Fetch => 0,
        Source::Staged => 1,
    }
}

/// The source a row names. Unlike the other enums, 0 is valid: it is the fetch of a row that
/// has no `source`.
fn source_from(code: u32) -> Result<Source, Corrupt> {
    match code {
        0 => Ok(Source::Fetch),
        1 => Ok(Source::Staged),
        other => corrupt(format!("source {other} is not one this build knows")),
    }
}

fn ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn size(n: u64, field: &str) -> Result<usize, Corrupt> {
    // Only a 32-bit build fails here (and then the count is refused, not cut down), so no
    // test reaches the error; it is built with the call and not in a closure that is never run.
    usize::try_from(n)
        .ok()
        .ok_or(Corrupt(format!("{field} {n} does not fit")))
}

/// The bytes of `update/desired`.
#[must_use]
pub(super) fn encode_desired(desired: &Desired) -> Vec<u8> {
    DesiredMsg {
        generation: desired.generation.0,
        commit: desired.commit.as_str().to_owned(),
        set_at_ms: desired.set_at.as_millis(),
        source: source_code(desired.source),
    }
    .encode_to_vec()
}

/// # Errors
/// [`Corrupt`]: the bytes are not a message, the commit is empty, the generation is 0, or the
/// source is not one this build knows.
pub(super) fn decode_desired(bytes: &[u8]) -> Result<Desired, Corrupt> {
    let msg = DesiredMsg::decode(bytes).map_err(|e| Corrupt(e.to_string()))?;
    if msg.commit.is_empty() {
        return corrupt("the run has no commit");
    }
    // Generation 0 is the generation of a host no run has touched, so a run of it would
    // read every host as having finished it.
    if msg.generation == 0 {
        return corrupt("the run has no generation");
    }
    Ok(Desired {
        generation: Generation(msg.generation),
        commit: Release::new(msg.commit),
        set_at: FarmTime::from_millis(msg.set_at_ms),
        source: source_from(msg.source)?,
    })
}

/// `Some` of the whole milliseconds of `value` when it is not `default`; the row leaves a
/// default out.
fn unless_default(value: Duration, default: Duration) -> Option<u64> {
    (value != default).then(|| ms(value))
}

/// The duration a row holds, or `default` for a field it does not.
fn or_default(stored: Option<u64>, default: Duration) -> Duration {
    stored.map_or(default, Duration::from_millis)
}

/// The bytes of `update/policy`. Durations are whole milliseconds, and a field that is the
/// default of [`Policy::default`] is left out.
#[must_use]
pub(super) fn encode_policy(policy: &Policy) -> Vec<u8> {
    let default = Policy::default();
    PolicyMsg {
        auto_nodes: policy.auto_nodes,
        auto_frontend: policy.auto_frontend,
        paused: policy.paused,
        idle_wait_ms: unless_default(policy.idle_wait, default.idle_wait),
        min_serving: count(policy.min_serving),
        max_hosts_out: (policy.max_hosts_out != default.max_hosts_out)
            .then(|| count(policy.max_hosts_out)),
        mac_force_after_ms: policy.mac_force_after.map(ms),
        new_work_only_to_updated: policy.new_work_only_to_updated,
        accept_failing_builds: policy.accept_failing_builds,
        ttl_ms: unless_default(policy.ttl, default.ttl),
        dwell_ms: unless_default(policy.dwell, default.dwell),
        lease_limit_ms: unless_default(policy.lease_limit, default.lease_limit),
        mac_lease_limit_ms: unless_default(policy.mac_lease_limit, default.mac_lease_limit),
        write_limit_ms: unless_default(policy.write_limit, default.write_limit),
        activate_limit_ms: unless_default(policy.activate_limit, default.activate_limit),
        health_limit_ms: unless_default(policy.health_limit, default.health_limit),
        rebuild_limit_ms: policy.rebuild_limit.map(ms),
        repair_wait_ms: unless_default(policy.repair_wait, default.repair_wait),
    }
    .encode_to_vec()
}

/// # Errors
/// [`Corrupt`]: the bytes are not a message, or a count does not fit.
pub(super) fn decode_policy(bytes: &[u8]) -> Result<Policy, Corrupt> {
    let msg = PolicyMsg::decode(bytes).map_err(|e| Corrupt(e.to_string()))?;
    let default = Policy::default();
    Ok(Policy {
        auto_nodes: msg.auto_nodes,
        auto_frontend: msg.auto_frontend,
        paused: msg.paused,
        new_work_only_to_updated: msg.new_work_only_to_updated,
        accept_failing_builds: msg.accept_failing_builds,
        ttl: or_default(msg.ttl_ms, default.ttl),
        dwell: or_default(msg.dwell_ms, default.dwell),
        idle_wait: or_default(msg.idle_wait_ms, default.idle_wait),
        min_serving: size(msg.min_serving, "min_serving")?,
        max_hosts_out: match msg.max_hosts_out {
            Some(n) => size(n, "max_hosts_out")?,
            None => default.max_hosts_out,
        },
        lease_limit: or_default(msg.lease_limit_ms, default.lease_limit),
        mac_lease_limit: or_default(msg.mac_lease_limit_ms, default.mac_lease_limit),
        write_limit: or_default(msg.write_limit_ms, default.write_limit),
        activate_limit: or_default(msg.activate_limit_ms, default.activate_limit),
        health_limit: or_default(msg.health_limit_ms, default.health_limit),
        rebuild_limit: msg.rebuild_limit_ms.map(Duration::from_millis),
        repair_wait: or_default(msg.repair_wait_ms, default.repair_wait),
        mac_force_after: msg.mac_force_after_ms.map(Duration::from_millis),
    })
}
