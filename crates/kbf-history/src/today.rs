//! Today's runs, in memory: what ran an action digest and what ran on a node, for the
//! reads that would otherwise need the day index the day-close fold writes
//! (ui-data.md 5.5, `GET /v1/actions/{digest}/runs` and `GET /v1/runs?node=`).
//!
//! The writer feeds it from the events it drains, one entry per run. Runs do not arrive
//! in the order they ended (a failed run reaches the writer after its logs are copied,
//! later than runs that ended after it), so the index orders them itself: by the key
//! (`ended_ms`, arrival sequence), the arrival sequence breaking a tie, and lists the
//! largest key first. It holds at most
//! [`Limits::today_bytes`](crate::Limits::today_bytes) of entries and forgets the oldest
//! by that key first. It is not durable and not complete: a restart empties it, a full queue drops
//! events before the writer sees them, and eviction forgets runs, so an answer carries
//! [`TodayPage::covers_from_ms`], the time from which the index has every run it was
//! sent, and a reader says its answer is partial.
//!
//! No clock is read here: the writer passes the time of its first step.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::records::{RunOutcome, RunRecord};
use crate::run_id::RunId;

/// What the index holds by default: 64 MiB, about 300,000 runs.
pub const TODAY_BYTES: usize = 64 * 1024 * 1024;

/// Entries one search looks at most, whatever it keeps: a filter that matches little
/// stops there with a cursor to go on from.
pub const MAX_EXAMINED: usize = 100_000;

/// An action digest: the hash and the size in bytes.
pub type ActionKey = (Vec<u8>, u64);

/// One run of today.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodayRun {
    /// The REAPI instance it was asked under.
    pub instance: String,
    /// Its id.
    pub run_id: RunId,
    /// The shape key; 0 for none.
    pub shape: u64,
    /// The shape's configuration (`-` for none).
    pub cfg: String,
    /// How it ended.
    pub outcome: RunOutcome,
    /// The node that ran it.
    pub node: String,
    /// When it started, milliseconds; its end when the run has no start time.
    pub started_ms: u64,
    /// When it ended, milliseconds.
    pub ended_ms: u64,
    /// The action digest, when the run has one.
    pub action: Option<ActionKey>,
}

impl TodayRun {
    /// The entry of `record`.
    #[must_use]
    pub fn of(record: &RunRecord) -> TodayRun {
        let ended_ms = match record.ended_us {
            0 => record.time_ms,
            us => us / 1_000,
        };
        TodayRun {
            instance: record.instance.clone(),
            run_id: record.run_id(),
            shape: record.shape,
            cfg: record.cfg.clone(),
            outcome: record.outcome(),
            node: record.node.clone(),
            started_ms: match record.started_us {
                0 => ended_ms,
                us => us / 1_000,
            },
            ended_ms,
            action: record.action.as_ref().map(|d| (d.hash.clone(), d.size)),
        }
    }

    /// Bytes the entry holds in memory: itself, its strings and its hash, and the three
    /// places the indexes name it.
    fn cost(&self) -> usize {
        const NAMED: usize = 3 * size_of::<u64>();
        size_of::<TodayRun>()
            + self.instance.len()
            + self.cfg.len()
            + self.node.len()
            + self.action.as_ref().map_or(0, |(hash, _)| hash.len())
            + NAMED
    }
}

/// What a search asks for, newest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TodayQuery<'a> {
    /// Only this instance.
    pub instance: Option<&'a str>,
    /// Only runs that ended this way.
    pub class: Option<RunOutcome>,
    /// Only runs that ended at or after this time, milliseconds.
    pub from_ms: u64,
    /// Only runs that started at or before this time, milliseconds.
    pub to_ms: u64,
    /// Runs a page holds at most.
    pub limit: usize,
    /// Where the page starts: after the entry [`TodayPage::next`] named.
    pub after: Option<TodayCursor>,
}

impl TodayQuery<'_> {
    /// The newest `limit` runs, whatever their instance, class and time.
    #[must_use]
    pub fn newest(limit: usize) -> TodayQuery<'static> {
        TodayQuery {
            instance: None,
            class: None,
            from_ms: 0,
            to_ms: u64::MAX,
            limit,
            after: None,
        }
    }
}

/// A page of a search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TodayPage {
    /// The runs, newest first.
    pub runs: Vec<TodayRun>,
    /// Where the next page starts; `None` after the last entry. A page may be short, or
    /// empty, with one: the search stopped at [`MAX_EXAMINED`].
    pub next: Option<TodayCursor>,
    /// The time from which the index has every run it was sent, milliseconds; `None`
    /// before the writer's first step.
    pub covers_from_ms: Option<u64>,
}

/// A place in the listing: the sort key of an entry, its end time and its arrival
/// sequence. A page that was given a cursor lists the entries with a smaller key, newest
/// first, so a run that arrives late with an older end time than the cursor is on a later
/// page, and one with a newer end time is not listed by the pages that follow. Printed as
/// `<ended_ms>.<seq>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TodayCursor {
    ended_ms: u64,
    seq: u64,
}

impl fmt::Display for TodayCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.ended_ms, self.seq)
    }
}

impl FromStr for TodayCursor {
    type Err = ();

    /// The cursor `<ended_ms>.<seq>` prints as, nothing else (no sign, no padding).
    fn from_str(text: &str) -> Result<TodayCursor, ()> {
        let canonical = |n: &str| n.parse::<u64>().ok().filter(|v| v.to_string() == n);
        let (ended, seq) = text.split_once('.').ok_or(())?;
        Ok(TodayCursor {
            ended_ms: canonical(ended).ok_or(())?,
            seq: canonical(seq).ok_or(())?,
        })
    }
}

/// An entry as the indexes hold it: shared, with its place in arrival order.
#[derive(Debug)]
struct Entry {
    seq: u64,
    run: TodayRun,
}

impl Entry {
    fn key(&self) -> TodayCursor {
        TodayCursor {
            ended_ms: self.run.ended_ms,
            seq: self.seq,
        }
    }
}

/// Puts `entry` in `entries` (oldest key first), after every entry that ended no later:
/// its sequence is the largest, so that is where its key sorts. Runs arrive nearly in
/// order (they are late by the time the tail pool takes), so the entries it passes are
/// few.
fn insert(entries: &mut Entries, entry: Arc<Entry>) {
    let at = entries.partition_point(|e| e.run.ended_ms <= entry.run.ended_ms);
    entries.insert(at, entry);
}

type Entries = VecDeque<Arc<Entry>>;

#[derive(Debug, Default)]
struct State {
    /// Every entry, oldest key first.
    runs: Entries,
    /// The sequence of the next entry.
    next: u64,
    bytes: usize,
    by_action: BTreeMap<ActionKey, Entries>,
    by_node: BTreeMap<String, Entries>,
    /// How many entries each instance has.
    instances: BTreeMap<String, usize>,
    started_ms: Option<u64>,
    /// The latest end time among the entries forgotten, once there is one.
    forgotten_ms: Option<u64>,
}

impl State {
    /// Forgets the oldest entries, from every index they are in, until the rest fit in
    /// `limit` bytes.
    fn trim(&mut self, limit: usize) {
        let (mut freed, mut count) = (0, 0);
        for entry in &self.runs {
            if self.bytes - freed <= limit {
                break;
            }
            freed += entry.run.cost();
            count += 1;
        }
        self.bytes -= freed;
        if let Some(last) = count.checked_sub(1).and_then(|i| self.runs.get(i)) {
            let ended = last.run.ended_ms;
            self.forgotten_ms = self.forgotten_ms.max(Some(ended));
        }
        let State {
            runs,
            by_action,
            by_node,
            instances,
            ..
        } = self;
        for entry in runs.drain(..count) {
            if let Some(key) = &entry.run.action {
                drop_oldest(by_action, key);
            }
            drop_oldest(by_node, &entry.run.node);
            uncount(instances, &entry.run.instance);
        }
    }
}

/// Drops the oldest entry of `key` (the one being forgotten has the smallest key of all,
/// so of its key), and the key with its last entry.
fn drop_oldest<K: Ord>(index: &mut BTreeMap<K, Entries>, key: &K) {
    let emptied = index.get_mut(key).is_some_and(|entries| {
        entries.pop_front();
        entries.is_empty()
    });
    if emptied {
        index.remove(key);
    }
}

/// Takes one entry off the count of `key`, and the key with its last.
fn uncount(counts: &mut BTreeMap<String, usize>, key: &str) {
    let gone = counts.get_mut(key).is_some_and(|n| {
        *n -= 1;
        *n == 0
    });
    if gone {
        counts.remove(key);
    }
}

/// The runs of today. See the module docs.
#[derive(Debug)]
pub struct Today {
    limit: usize,
    state: Mutex<State>,
}

impl Today {
    /// An empty index holding at most `limit_bytes` of entries.
    #[must_use]
    pub fn new(limit_bytes: usize) -> Today {
        Today {
            limit: limit_bytes,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Maps and counters; a panic elsewhere leaves them whole.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes that the writer is running at `now_ms`; the first call is when the index
    /// starts covering.
    pub fn begin(&self, now_ms: u64) {
        self.state().started_ms.get_or_insert(now_ms);
    }

    /// Takes `run` in, in its place by end time (a run that ended before the entries held
    /// goes before them, and is the first forgotten), and forgets the oldest entries while there are more bytes than
    /// the limit (an entry larger than the limit is not kept).
    pub fn push(&self, run: TodayRun) {
        let mut st = self.state();
        let ended = run.ended_ms;
        st.started_ms.get_or_insert(ended);
        let entry = Arc::new(Entry { seq: st.next, run });
        st.next += 1;
        if let Some(key) = &entry.run.action {
            let held = st.by_action.entry(key.clone()).or_default();
            insert(held, Arc::clone(&entry));
        }
        let on_node = st.by_node.entry(entry.run.node.clone()).or_default();
        insert(on_node, Arc::clone(&entry));
        *st.instances.entry(entry.run.instance.clone()).or_default() += 1;
        st.bytes += entry.run.cost();
        insert(&mut st.runs, entry);
        st.trim(self.limit);
    }

    /// Entries held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state().runs.len()
    }

    /// Whether no entry is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes the entries hold, as [`TodayRun`] counts them.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.state().bytes
    }

    /// The instances with entries, sorted.
    #[must_use]
    pub fn instances(&self) -> Vec<String> {
        self.state().instances.keys().cloned().collect()
    }

    /// The time from which the index has every run it was sent: the first step of the
    /// writer, or, once the oldest entries were forgotten, the end of the oldest one
    /// held.
    #[must_use]
    pub fn covers_from_ms(&self) -> Option<u64> {
        covers(&self.state())
    }

    /// A page of the runs of `action`, newest first, that `allow` accepts and `q` names.
    pub fn of_action(
        &self,
        action: &ActionKey,
        q: &TodayQuery<'_>,
        allow: &dyn Fn(&str) -> bool,
    ) -> TodayPage {
        let st = self.state();
        search(&st, st.by_action.get(action), q, allow)
    }

    /// A page of the runs on `node` that overlapped the window of `q` (they ended at or
    /// after its start and started at or before its end), newest first, that `allow`
    /// accepts and `q` names.
    pub fn on_node(
        &self,
        node: &str,
        q: &TodayQuery<'_>,
        allow: &dyn Fn(&str) -> bool,
    ) -> TodayPage {
        let st = self.state();
        search(&st, st.by_node.get(node), q, allow)
    }
}

fn covers(st: &State) -> Option<u64> {
    let started = st.started_ms?;
    Some(match (st.runs.front(), st.forgotten_ms) {
        // A run that arrived late and is older than what was forgotten does not move
        // coverage back.
        (Some(oldest), Some(forgotten)) => started.max(oldest.run.ended_ms.max(forgotten)),
        _ => started,
    })
}

/// The newest runs of `entries` (oldest key first) that match, `q.limit` at most.
fn search(
    st: &State,
    entries: Option<&Entries>,
    q: &TodayQuery<'_>,
    allow: &dyn Fn(&str) -> bool,
) -> TodayPage {
    let mut page = TodayPage {
        covers_from_ms: covers(st),
        ..TodayPage::default()
    };
    let Some(entries) = entries else {
        return page;
    };
    let end = q.after.map_or(entries.len(), |after| {
        entries.partition_point(|e| e.key() < after)
    });
    for (examined, i) in (0..end).rev().enumerate() {
        let entry = &entries[i];
        if examined == MAX_EXAMINED {
            // Just above the entry, which this page has not looked at.
            let key = entry.key();
            page.next = Some(TodayCursor {
                seq: key.seq + 1,
                ..key
            });
            break;
        }
        let run = &entry.run;
        let matches = q.instance.is_none_or(|n| run.instance == n)
            && allow(&run.instance)
            && q.class.is_none_or(|c| run.outcome == c)
            && run.ended_ms >= q.from_ms
            && run.started_ms <= q.to_ms;
        if !matches {
            continue;
        }
        page.runs.push(run.clone());
        if page.runs.len() == q.limit {
            page.next = (i > 0).then(|| entry.key());
            break;
        }
    }
    page
}
