//! From exported rows to the operations a replay submits.
//!
//! An *operation* is one submission and the ladder of attempts it needed: a chain of
//! own-limit kills of one action digest, each queued when the last ended, and the attempt
//! that ended it. The replay keeps from the record only what is not the scheduler's to
//! decide: when the operation arrived, what it first booked, how much memory it really
//! needed, and how long its attempts ran.
//!
//! The need is a model, not a measurement. A run that completed needed at most its
//! recorded peak (and more than the limit of the attempt that died before it); a run that
//! was killed needed more than its limit, and the model says "one byte more", the least
//! the record supports.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use kbf_estimator::{Basis, Cfg, Instance, Key, PlatformClass, Shape, memory_max};

use super::Params;
use crate::row::{Capacity, Row};

const SECOND_US: u64 = 1_000_000;

/// How an attempt ended, as the model uses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ok,
    Failed,
    Kill,
}

/// The attempt a row records.
#[derive(Clone, Debug)]
struct Attempt {
    kind: Kind,
    queued_us: u64,
    ended_us: u64,
    run_us: u64,
    booked: Option<u64>,
    raised: Option<u64>,
    peak: Option<u64>,
    cpu_millis: Option<u64>,
    cpu_usage_us: u64,
    operation: Option<String>,
}

/// One submission of a replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    /// The REAPI instance.
    pub instance: String,
    /// The action digest as the record names it.
    pub digest: String,
    /// The estimator's key, when the record names a shape (without one the action is
    /// booked cold).
    pub key: Option<Key>,
    /// When it arrived, in microseconds after the first arrival of the trace.
    pub arrives_us: u64,
    /// What the first attempt booked.
    pub first_booked: u64,
    /// The CPU it books, in millicores.
    pub cpu_millis: u64,
    /// The memory it needs, in bytes: it is killed at any booking whose limit is below.
    pub need: u64,
    /// How long an attempt that completes runs, in microseconds.
    pub run_us: u64,
    /// How long an attempt that is killed runs before it is.
    pub kill_us: u64,
    /// CPU time of the completing attempt, in microseconds.
    pub cpu_usage_us: u64,
    /// Own-limit kills the record shows for this operation.
    pub recorded_kills: u32,
    /// Attempts the record shows for it.
    pub recorded_attempts: u32,
}

/// What the record holds, to compare a replay with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Recorded {
    /// Operations.
    pub ops: u64,
    /// Attempts.
    pub attempts: u64,
    /// Own-limit kills.
    pub kills: u64,
}

/// A trace ready to replay.
#[derive(Clone, Debug, Default)]
pub struct Trace {
    /// Operations by arrival.
    pub ops: Vec<Op>,
    /// The nodes the kept rows ran on and their capacity.
    pub nodes: BTreeMap<String, Capacity>,
    /// Rows kept and dropped, and why.
    pub funnel: Funnel,
    /// What the kept rows show.
    pub recorded: Recorded,
}

/// Where the rows went.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Funnel {
    /// Rows read.
    pub rows: u64,
    /// Rows that became attempts of an operation.
    pub kept: u64,
    /// Rows dropped, by reason.
    pub dropped: BTreeMap<&'static str, u64>,
    /// Operations whose action has no shape key (booked cold, never learned).
    pub no_shape: u64,
    /// Completed operations whose record has no peak (their need is taken as 0).
    pub no_peak: u64,
    /// Operations whose first attempt has no recorded booking: success with no recorded
    /// booking, assumed to have booked the first rung.
    pub assumed_first_rung: u64,
}

impl Funnel {
    fn drop_row(&mut self, why: &'static str) {
        *self.dropped.entry(why).or_default() += 1;
    }
}

/// Reads every `*.jsonl` file of `dir` (or the one file `dir` names) into a trace.
///
/// # Errors
/// A file cannot be read, or a line is not a row (the error names the file and line).
pub fn load(path: &Path, params: &Params) -> io::Result<Trace> {
    Ok(build(read_rows(path)?, params))
}

/// Every row of the `*.jsonl` files of `path` (or of the one file it names), in file-name
/// then line order.
///
/// # Errors
/// A file cannot be read, or a line is not a row (the error names the file and line).
pub fn read_rows(path: &Path) -> io::Result<Vec<Row>> {
    let mut rows = Vec::new();
    let files = if path.is_dir() {
        // An entry that cannot be read is skipped; a file that can be listed and not read
        // is an error below.
        let mut names: Vec<_> = fs::read_dir(path)?.flatten().map(|e| e.path()).collect();
        names.retain(|p| p.extension().is_some_and(|e| e == "jsonl"));
        names.sort();
        names
    } else {
        vec![path.to_path_buf()]
    };
    for file in files {
        for (n, line) in fs::read_to_string(&file)?.lines().enumerate() {
            let row = Row::parse(line).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}:{}: {e}", file.display(), n + 1),
                )
            })?;
            rows.push(row);
        }
    }
    Ok(rows)
}

/// The trace of `rows`.
pub fn build(rows: Vec<Row>, params: &Params) -> Trace {
    let mut trace = Trace::default();
    trace.funnel.rows = rows.len() as u64;
    let capacity: BTreeMap<&str, Capacity> = rows
        .iter()
        .filter_map(|r| Some((r.node.as_str(), r.node_capacity?)))
        .collect();
    let first = params.first_booking();
    let mut kept: Vec<(&Row, Attempt)> = Vec::new();
    for row in &rows {
        match admit(row, &capacity) {
            Ok(a) => kept.push((row, a)),
            Err(why) => trace.funnel.drop_row(why),
        }
    }
    kept.sort_by(|(ra, a), (rb, b)| {
        (&ra.instance, &ra.digest, a.queued_us).cmp(&(&rb.instance, &rb.digest, b.queued_us))
    });
    let mut chains: Vec<Vec<(&Row, Attempt)>> = Vec::new();
    for item in kept {
        let joins = chains.last().is_some_and(|chain| {
            let (row, last) = &chain[chain.len() - 1];
            (row.instance.as_str(), row.digest.as_str())
                == (item.0.instance.as_str(), item.0.digest.as_str())
                && continues(last, &item.1, params.chain_gap_us)
        });
        match chains.last_mut() {
            Some(chain) if joins => chain.push(item),
            _ => chains.push(vec![item]),
        }
    }
    for chain in &chains {
        trace.funnel.kept += chain.len() as u64;
        for (row, _) in chain {
            // `admit` kept only rows of a node some row gave a capacity for.
            trace
                .nodes
                .insert(row.node.clone(), capacity[row.node.as_str()]);
        }
    }
    let mut ops: Vec<Op> = chains
        .iter()
        .map(|chain| op_of(chain, first, &mut trace.funnel))
        .collect();
    ops.sort_by(|a, b| {
        (a.arrives_us, &a.instance, &a.digest).cmp(&(b.arrives_us, &b.instance, &b.digest))
    });
    let t0 = ops.first().map_or(0, |o| o.arrives_us);
    ops.iter_mut().for_each(|o| o.arrives_us -= t0);
    trace.recorded = Recorded {
        ops: ops.len() as u64,
        attempts: ops.iter().map(|o| u64::from(o.recorded_attempts)).sum(),
        kills: ops.iter().map(|o| u64::from(o.recorded_kills)).sum(),
    };
    trace.ops = ops;
    trace
}

/// Whether `next` is the rerun of `last`: `last` was an own-limit kill the ladder retried,
/// and `next` was queued when it ended (or names the same operation).
fn continues(last: &Attempt, next: &Attempt, gap_us: u64) -> bool {
    if last.kind != Kind::Kill || last.raised.is_none() {
        return false;
    }
    if let (Some(a), Some(b)) = (&last.operation, &next.operation) {
        return a == b;
    }
    next.queued_us.saturating_add(SECOND_US) >= last.ended_us
        && next.queued_us <= last.ended_us.saturating_add(gap_us)
}

/// The attempt `row` records, or why the replay leaves it out.
fn admit(row: &Row, capacity: &BTreeMap<&str, Capacity>) -> Result<Attempt, &'static str> {
    let (Some(started), Some(ended)) = (row.started_us, row.ended_us) else {
        return Err("never_ran");
    };
    let kind = match (row.outcome.as_str(), row.memory_kill.as_str()) {
        ("ok", _) => Kind::Ok,
        ("failed", _) => Kind::Failed,
        ("memory_killed", "own_limit") => Kind::Kill,
        ("memory_killed", _) => return Err("node_pressure_kill"),
        ("timed_out", _) => return Err("timed_out"),
        ("infra", _) => return Err("infra"),
        ("lost", _) => return Err("lost"),
        ("cancelled", _) => return Err("cancelled"),
        _ => return Err("unknown_outcome"),
    };
    if row.platform != "linux" {
        return Err("not_linux");
    }
    if !capacity.contains_key(row.node.as_str()) {
        return Err("no_node_capacity");
    }
    let wall = row.cgroup.as_ref().and_then(|c| c.wall_us);
    Ok(Attempt {
        kind,
        queued_us: row.queued_us,
        ended_us: ended,
        run_us: wall
            .unwrap_or_else(|| ended.saturating_sub(started))
            .max(SECOND_US),
        booked: row.booked_memory_bytes,
        raised: row.raised_memory_bytes,
        peak: row.cgroup.as_ref().and_then(|c| c.memory_peak_bytes),
        cpu_millis: row.booked_millicpus,
        cpu_usage_us: row.cgroup.as_ref().map_or(0, |c| c.cpu_usage_us),
        operation: row.operation.clone(),
    })
}

fn op_of(chain: &[(&Row, Attempt)], first_default: u64, funnel: &mut Funnel) -> Op {
    let (row, head) = &chain[0];
    funnel.assumed_first_rung += u64::from(head.booked.is_none());
    let (_, last) = &chain[chain.len() - 1];
    // The booking of each attempt: what the record says, else what the previous kill was
    // retried with, else the front's first rung.
    let mut booked = Vec::with_capacity(chain.len());
    let mut prev_raised = None;
    for (_, a) in chain {
        let b = a.booked.or(prev_raised).unwrap_or(first_default);
        booked.push(b);
        prev_raised = a.raised;
    }
    let kills: Vec<usize> = (0..chain.len())
        .filter(|&i| chain[i].1.kind == Kind::Kill)
        .collect();
    let beyond_last_kill = kills
        .last()
        .map_or(0, |&i| memory_max(booked[i]).unwrap_or(0).saturating_add(1));
    let completes = last.kind != Kind::Kill;
    let peak = last.peak.filter(|_| completes);
    if completes && peak.is_none() {
        funnel.no_peak += 1;
    }
    let need = peak.unwrap_or(0).max(beyond_last_kill);
    let kill_us = if kills.is_empty() {
        (last.run_us / 2).max(SECOND_US)
    } else {
        let total: u64 = kills.iter().map(|&i| chain[i].1.run_us).sum();
        total / kills.len() as u64
    };
    let key = key_of(row);
    if key.is_none() {
        funnel.no_shape += 1;
    }
    Op {
        instance: row.instance.clone(),
        digest: row.digest.clone(),
        key,
        arrives_us: head.queued_us,
        first_booked: booked[0],
        cpu_millis: head.cpu_millis.unwrap_or(1_000),
        need,
        run_us: last.run_us,
        kill_us,
        cpu_usage_us: last.cpu_usage_us,
        recorded_kills: kills.len() as u32,
        recorded_attempts: chain.len() as u32,
    }
}

/// The estimator key of an action on a Linux node: its instance, platform class, the
/// cgroup basis the replay measures with, its shape and its configuration.
///
/// The export knows no CPU architecture, so a replay key's platform class is the OS alone;
/// the server's carries the architecture when the request names one, so on a farm that
/// pins it a key reaches its first estimate later than the replay predicts.
///
/// The server's shadow keys its summaries the same way ([`kbf_server::learned::key_of`]),
/// the configuration hashed by the same function; a test holds the two to the same
/// estimates.
pub fn key_of(row: &Row) -> Option<Key> {
    let shape = u64::from_str_radix(&row.shape, 16)
        .ok()
        .filter(|s| *s != 0)?;
    let cfg = (row.cfg != "-" && !row.cfg.is_empty())
        .then(|| Cfg(kbf_server::learned::cfg_hash(&row.cfg)));
    Some(Key::new(
        Instance::new(row.instance.clone()),
        PlatformClass::new(row.platform.clone()),
        Basis::Cgroup,
        Shape(shape),
        cfg,
    ))
}

/// FNV-1a: a stable hash for configuration text.
pub(crate) fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
    })
}
