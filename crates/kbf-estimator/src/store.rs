//! The summaries of all keys and the estimate over them.

use std::collections::BTreeMap;

use crate::key::{Day, Instance, Key, Level};
use crate::run::{Bounds, Hint, Observed, Run};
use crate::summary::{MAX_DAY, N_MIN, Summary};

/// A key idle for more than this many days is stale: no estimate, dropped by
/// [`Summaries::expire`].
pub const STALE_DAYS: u32 = 60;
/// Most keys kept in the least-recently-seen map; beyond it the least recently seen go.
pub const MAX_KEYS: usize = 65_536;
/// Most keys one instance may hold in that map.
pub const MAX_KEYS_PER_INSTANCE: usize = MAX_KEYS / 4;
/// Most keys kept in the safety map (see [`Summaries`]).
pub const MAX_PINNED: usize = 4096;

/// Why there is no estimate. The caller books what it books today and counts the reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fallback {
    /// No summary at either level.
    Missing,
    /// A summary exists but was last seen over [`STALE_DAYS`] days ago.
    Stale,
    /// Summaries exist but hold too few samples and no floor.
    Cold,
    /// A stored row did not decode (see [`Summaries::insert_encoded`]).
    Corrupt,
    /// The size would reach the memory of the largest matching node, which is a whole
    /// host: the caller books cold and the ladder and digest floors decide.
    OverCap,
}

/// A size for an action. Its memory is always strictly below `Bounds::host_memory`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// Memory to book, in bytes.
    pub memory_bytes: u64,
    /// CPU to book, in millicores.
    pub cpu_millis: u32,
    /// The level of the summary the sizes came from.
    pub level: Level,
}

/// Summaries keyed by `Key`, with per-instance counts.
#[derive(Clone, Debug, Default)]
struct Table {
    map: BTreeMap<Key, Summary>,
    counts: BTreeMap<Instance, usize>,
}

impl Table {
    fn get(&self, key: &Key) -> Option<&Summary> {
        self.map.get(key)
    }

    fn take(&mut self, key: &Key) -> Option<Summary> {
        let summary = self.map.remove(key)?;
        self.counts
            .entry(key.instance().clone())
            .and_modify(|count| *count -= 1);
        Some(summary)
    }

    /// Callers `take` a key before they `put` it, so a put is always a new key.
    fn put(&mut self, key: Key, summary: Summary) {
        *self.counts.entry(key.instance().clone()).or_insert(0) += 1;
        self.map.insert(key, summary);
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn count(&self, instance: &Instance) -> usize {
        self.counts.get(instance).copied().unwrap_or(0)
    }

    /// Drops the least recently seen keys (of `instance` if given; never `keep`) until
    /// at most `limit` remain in scope; ties go to the smaller key.
    fn trim(&mut self, instance: Option<&Instance>, limit: usize, keep: [&Key; 2]) {
        let in_scope = instance.map_or(self.len(), |instance| self.count(instance));
        for _ in limit..in_scope {
            let oldest = self
                .map
                .iter()
                .filter(|(key, _)| instance.is_none_or(|instance| key.instance() == instance))
                .filter(|(key, _)| !keep.contains(key))
                .min_by_key(|(key, summary)| (summary.last_seen(), *key))
                .map(|(key, _)| (*key).clone());
            oldest.iter().for_each(|key| {
                self.take(key);
            });
        }
    }

    fn expire(&mut self, now: Day) {
        self.map.retain(|_, summary| {
            let keep = fresh(summary, now) || summary.oom_floor() > 0;
            if !fresh(summary, now) {
                summary.strip_samples();
            }
            keep
        });
        self.counts.clear();
        for key in self.map.keys() {
            *self.counts.entry(key.instance().clone()).or_insert(0) += 1;
        }
    }
}

/// The summaries of every key.
///
/// Two tables: the main one is least-recently-seen bounded, with a quota per instance
/// so that one tenant cannot flush another's keys; a key that holds safety state (a
/// floor or a raise-only window) lives in a separate small table that the main table's
/// eviction never touches.
#[derive(Clone, Debug)]
pub struct Summaries {
    lru: Table,
    pinned: Table,
    max_keys: usize,
    per_instance: usize,
    max_pinned: usize,
    unpinned: u64,
}

impl Default for Summaries {
    fn default() -> Self {
        Self::with_limits(MAX_KEYS, MAX_KEYS_PER_INSTANCE, MAX_PINNED)
    }
}

impl Summaries {
    /// Tables bounded by `max_keys` (all instances), `per_instance` (one instance) and
    /// `max_pinned` (safety table). Each is at least two: a key and its L2.
    pub fn with_limits(max_keys: usize, per_instance: usize, max_pinned: usize) -> Self {
        Self {
            lru: Table::default(),
            pinned: Table::default(),
            max_keys: max_keys.max(2),
            per_instance: per_instance.max(2),
            max_pinned: max_pinned.max(2),
            unpinned: 0,
        }
    }

    /// Keys held, in both tables.
    pub fn len(&self) -> usize {
        self.lru.len() + self.pinned.len()
    }

    /// Whether no key is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The summary of `key`, if held.
    pub fn get(&self, key: &Key) -> Option<&Summary> {
        self.lru.get(key).or_else(|| self.pinned.get(key))
    }

    /// Feeds a finished run into `key`'s summary and into its L2 summary. Returns what
    /// the L1 (or only) summary did with it.
    pub fn observe(&mut self, key: &Key, run: &Run, now: Day, bounds: &Bounds) -> Observed {
        let l2 = key.l2();
        if key.has_cfg() {
            self.apply(&l2, run, now, bounds);
        }
        let observed = self.apply(key, run, now, bounds);
        self.evict(key, &l2);
        observed
    }

    fn apply(&mut self, key: &Key, run: &Run, now: Day, bounds: &Bounds) -> Observed {
        let mut summary = self
            .lru
            .take(key)
            .or_else(|| self.pinned.take(key))
            .unwrap_or_else(|| Summary::new(key.basis(), now.min(Day(MAX_DAY))));
        let observed = summary.observe(run, now, bounds);
        self.place(key.clone(), summary);
        observed
    }

    /// Puts a summary where it belongs. A key with safety state goes to the safety
    /// table, which is never evicted from and holds at most a quarter of its slots per
    /// instance; when the table or the instance's share is full the key stays in
    /// the main table (still answering, but evictable) and the refusal is counted in
    /// [`Summaries::unpinned`]: a floor is never dropped silently.
    fn place(&mut self, key: Key, summary: Summary) {
        let room = self.pinned.len() < self.max_pinned
            && self.pinned.count(key.instance()) < (self.max_pinned / 4).max(1);
        if summary.holds_safety_state() && room {
            self.pinned.put(key, summary);
        } else {
            self.unpinned += u64::from(summary.holds_safety_state());
            self.lru.put(key, summary);
        }
    }

    /// How many times a key with safety state could not be given a place in the safety
    /// table because it was full (an operator signal to raise the limit).
    pub fn unpinned(&self) -> u64 {
        self.unpinned
    }

    fn evict(&mut self, keep: &Key, keep_too: &Key) {
        let keep = [keep, keep_too];
        self.lru
            .trim(Some(keep[0].instance()), self.per_instance, keep);
        self.lru.trim(None, self.max_keys, keep);
    }

    /// Drops every summary idle for more than [`STALE_DAYS`] days on `now`, except
    /// that a summary holding a floor keeps the floor (safety state does not expire by
    /// idleness; it goes only by decay) and loses its samples.
    pub fn expire(&mut self, now: Day) {
        self.lru.expire(now);
        self.pinned.expire(now);
    }

    /// Stores a row read back from the durable store, replacing the in-memory row of
    /// the key unconditionally: the caller must not load a stored row over a newer
    /// in-memory floor. A row that does not decode, or
    /// whose basis is not the key's, is not stored (the key stays cold) and is reported
    /// as [`Fallback::Corrupt`].
    pub fn insert_encoded(&mut self, key: &Key, bytes: &[u8]) -> Result<(), Fallback> {
        match Summary::decode(bytes) {
            Some(summary) if summary.basis() == key.basis() => {
                self.lru.take(key);
                self.pinned.take(key);
                self.place(key.clone(), summary);
                Ok(())
            }
            _ => Err(Fallback::Corrupt),
        }
    }

    /// The memory floor to apply to every operation of `key`'s shape that is not yet
    /// granted, on every path, including when [`Summaries::estimate`] answers
    /// [`Fallback::Stale`] or [`Fallback::OverCap`]: the L2 floor, strictly below `bounds.host_memory`; 0 when none. A floor
    /// does not expire by idleness, only by decay.
    pub fn key_floor(&self, key: &Key, bounds: &Bounds) -> u64 {
        // Idleness does not expire a floor.
        self.get(&key.l2())
            .map_or(0, |summary| summary.floor(bounds))
    }

    /// The size to book for an action of `key` on `now`, or why there is none.
    ///
    /// The first level with at least [`N_MIN`] samples answers; failing that the first
    /// with enough samples to raise (the estimate is then at least the default
    /// booking). Both levels' floors apply, and a raise-only state at either level
    /// forbids lowering. The client's `hint` is never lowered. A result that would
    /// reach `bounds.host_memory` (hint included) is [`Fallback::OverCap`]: the crate
    /// never returns a whole-host booking.
    pub fn estimate(
        &self,
        key: &Key,
        now: Day,
        bounds: &Bounds,
        hint: &Hint,
    ) -> Result<Estimate, Fallback> {
        let now = now.min(Day(MAX_DAY));
        let l1 = if key.has_cfg() { self.get(key) } else { None };
        let l2 = self.get(&key.l2());
        let levels = [(Level::L1, l1), (Level::L2, l2)]
            .map(|(level, summary)| (level, summary.filter(|s| fresh(s, now))));
        if levels.iter().all(|(_, summary)| summary.is_none()) {
            return Err(if l1.or(l2).is_some() {
                Fallback::Stale
            } else {
                Fallback::Missing
            });
        }
        let enough = |summary: &Summary| summary.samples() >= N_MIN;
        let (level, chosen) = levels
            .iter()
            .find_map(|(level, s)| s.filter(|s| enough(s)).map(|s| (*level, s)))
            .or_else(|| {
                levels
                    .iter()
                    .find_map(|(level, s)| s.filter(|s| s.usable()).map(|s| (*level, s)))
            })
            .ok_or(Fallback::Cold)?;
        let held = levels.iter().filter_map(|(_, summary)| *summary);
        let raise_only = held.clone().any(|summary| summary.raise_only(now));
        let floor = held.map(|summary| summary.floor(bounds)).fold(0, u64::max);
        let memory = chosen
            .memory(bounds, raise_only)
            .max(floor)
            .max(hint.memory_bytes.unwrap_or(0));
        if memory >= bounds.host_memory {
            return Err(Fallback::OverCap);
        }
        Ok(Estimate {
            memory_bytes: memory,
            cpu_millis: chosen.cpu(bounds).max(hint.cpu_millis.unwrap_or(0)),
            level,
        })
    }
}

fn fresh(summary: &Summary, now: Day) -> bool {
    now.0.saturating_sub(summary.last_seen().0) <= STALE_DAYS
}
