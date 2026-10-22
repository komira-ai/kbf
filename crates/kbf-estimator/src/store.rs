//! The summaries of all keys and the estimate over them.

use std::collections::BTreeMap;

use crate::key::{Day, Key, Level};
use crate::run::{Bounds, Hint, Observed, Run};
use crate::summary::{N_MIN, Summary};

/// A key idle for more than this many days is stale: no estimate, dropped by
/// [`Summaries::expire`].
pub const STALE_DAYS: u32 = 60;
/// Most keys kept; beyond it the least recently seen go.
pub const MAX_KEYS: usize = 65_536;

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
}

/// A size for an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// Memory to book, in bytes.
    pub memory_bytes: u64,
    /// CPU to book, in millicores.
    pub cpu_millis: u32,
    /// The level of the summary the sizes came from.
    pub level: Level,
}

/// The summaries of every key.
#[derive(Clone, Debug)]
pub struct Summaries {
    map: BTreeMap<Key, Summary>,
    max_keys: usize,
}

impl Default for Summaries {
    fn default() -> Self {
        Self::with_max_keys(MAX_KEYS)
    }
}

impl Summaries {
    /// A map that keeps at most `max_keys` keys (at least two: a key and its L2).
    pub fn with_max_keys(max_keys: usize) -> Self {
        Self {
            map: BTreeMap::new(),
            max_keys: max_keys.max(2),
        }
    }

    /// Keys held.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether no key is held.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The summary of `key`, if held.
    pub fn get(&self, key: &Key) -> Option<&Summary> {
        self.map.get(key)
    }

    /// Feeds a finished run into `key`'s summary and into its L2 summary. Returns what
    /// the L1 (or only) summary did with it.
    pub fn observe(&mut self, key: &Key, run: &Run, now: Day, bounds: &Bounds) -> Observed {
        let l2 = key.l2();
        if key.has_cfg() {
            self.summary(&l2, now).observe(run, now, bounds);
        }
        let observed = self.summary(key, now).observe(run, now, bounds);
        self.evict(key, &l2);
        observed
    }

    fn summary(&mut self, key: &Key, now: Day) -> &mut Summary {
        self.map
            .entry(key.clone())
            .or_insert_with(|| Summary::new(key.basis(), now))
    }

    /// Drops the least recently seen keys (never `keep` or `keep_too`) while over the
    /// limit; ties go to the smaller key, so the choice is deterministic.
    fn evict(&mut self, keep: &Key, keep_too: &Key) {
        for _ in self.max_keys..self.map.len() {
            let oldest = self
                .map
                .iter()
                .filter(|(key, _)| *key != keep && *key != keep_too)
                .min_by_key(|(key, summary)| (summary.last_seen(), *key))
                .map(|(key, _)| key.clone());
            oldest.iter().for_each(|key| {
                self.map.remove(key);
            });
        }
    }

    /// Drops every summary idle for more than [`STALE_DAYS`] days on `now`.
    pub fn expire(&mut self, now: Day) {
        self.map.retain(|_, summary| fresh(summary, now));
    }

    /// Stores a row read back from the durable store. A row that does not decode, or
    /// whose basis is not the key's, is not stored (the key stays cold) and is reported
    /// as [`Fallback::Corrupt`].
    pub fn insert_encoded(&mut self, key: &Key, bytes: &[u8]) -> Result<(), Fallback> {
        match Summary::decode(bytes) {
            Some(summary) if summary.basis() == key.basis() => {
                self.map.insert(key.clone(), summary);
                Ok(())
            }
            _ => Err(Fallback::Corrupt),
        }
    }

    /// The memory floor to apply to every operation of `key`'s shape that is not yet
    /// granted: the L2 floor, at most `bounds.memory_cap`; 0 when none or stale.
    pub fn key_floor(&self, key: &Key, now: Day, bounds: &Bounds) -> u64 {
        self.map
            .get(&key.l2())
            .filter(|summary| fresh(summary, now))
            .map_or(0, |summary| summary.floor(bounds))
    }

    /// The size to book for an action of `key` on `now`, or why there is none.
    ///
    /// The first level with at least [`N_MIN`] samples answers; failing that the first
    /// with enough samples to raise (the estimate is then at least the default
    /// booking). Both levels' floors apply, and a raise-only state at either level
    /// forbids lowering. The client's `hint` is never lowered.
    pub fn estimate(
        &self,
        key: &Key,
        now: Day,
        bounds: &Bounds,
        hint: &Hint,
    ) -> Result<Estimate, Fallback> {
        let l1 = if key.has_cfg() {
            self.map.get(key)
        } else {
            None
        };
        let l2 = self.map.get(&key.l2());
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
        let memory = chosen.memory(bounds, raise_only).max(floor);
        let cpu = chosen.cpu(bounds);
        Ok(Estimate {
            memory_bytes: memory.max(hint.memory_bytes.unwrap_or(0)),
            cpu_millis: cpu.max(hint.cpu_millis.unwrap_or(0)),
            level,
        })
    }
}

fn fresh(summary: &Summary, now: Day) -> bool {
    now.0.saturating_sub(summary.last_seen().0) <= STALE_DAYS
}
