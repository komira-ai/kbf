//! The fake object store, with the faults a store meets: it can hang (every PUT held
//! until a time), be slow, fail PUTs (the next N, or every one in a window), keep an
//! object but lose its acknowledgement, or keep it and answer that it failed.
//!
//! It never loses an object it kept. A PUT to an id it already holds replaces the
//! object (a plain PUT, the worst case: no `conditional_put`), so a writer that reuses
//! an id is caught by property 9 rather than refused by the store; a replacement with
//! other content is recorded ([`ObjectStore::rewrites`], invariant C).
//!
//! An object is one blob (the direct path's PUT) or a flushed segment of the local
//! write tier ([`Body`]: its records and footer, and where each blob is). Both meet
//! the same faults. Reads (`Get`) are answered at once, from what the store holds.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kbf_meta::ObjectId;
use kbf_sim::{Event, NodeId, NodeInput, Output};
use kbf_types::{Digest, Effect, FarmTime, StateMachine};

use crate::{Msg, content};

/// A flushed segment as the store holds it: its bytes (the records, then the footer
/// `kbf_segments` encodes from the committed list) and each blob's offset in them,
/// sorted by digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    pub blobs: Vec<(Digest, u64)>,
    pub bytes: Vec<u8>,
}

/// What one stored object holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Held {
    /// One blob, PUT by the direct path.
    Blob(Digest),
    /// A segment the write tier's flusher PUT.
    Segment(Body),
}

impl Held {
    /// Whether this object holds `digest`'s bytes: it is the blob, or the segment
    /// lists it at an offset where its bytes are.
    #[must_use]
    pub fn holds(&self, digest: &Digest) -> bool {
        match self {
            Self::Blob(d) => d == digest,
            Self::Segment(body) => body.blobs.iter().any(|(d, at)| {
                let from = usize::try_from(*at).unwrap_or(usize::MAX);
                let to = from.saturating_add(usize::try_from(d.size_bytes).unwrap_or(0));
                d == digest && body.bytes.get(from..to) == Some(content(d).as_slice())
            }),
        }
    }
}

/// Where a blob's bytes are durable, as the checks ask it: the seam the local write
/// tier plugs into. Today only the object store holds bytes; with the tier (rollout
/// stages 3 and 4 of docs/design/write-path.md) a staged object's block on a server's
/// disk does too, and the cell checks properties 8 and 9 against both.
pub trait BlobHomes {
    /// Whether `object` holds `digest`'s bytes, readable now.
    fn holds(&self, object: ObjectId, digest: &Digest) -> bool;
}

/// A fault the store meets, from the time its [`StorePlan`] entry names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreFault {
    /// The store hangs: every PUT is held, unanswered and not yet kept, until `until`,
    /// then kept and answered.
    Pause { until: FarmTime },
    /// Every PUT until `until` is kept and answered `by` later.
    Slow { until: FarmTime, by: Duration },
    /// The next `n` PUTs fail and keep nothing.
    FailNext { n: u32 },
    /// Every PUT until `until` fails and keeps nothing.
    FailUntil { until: FarmTime },
    /// The next `n` PUTs are kept, but their acknowledgement is lost.
    LoseAck { n: u32 },
    /// The next `n` PUTs are kept, but answered as failed.
    KeepButFail { n: u32 },
}

impl StoreFault {
    /// When the fault's effect ends, if it has a window.
    #[must_use]
    pub fn until(self) -> Option<FarmTime> {
        match self {
            Self::Pause { until } | Self::Slow { until, .. } | Self::FailUntil { until } => {
                Some(until)
            }
            Self::FailNext { .. } | Self::LoseAck { .. } | Self::KeepButFail { .. } => None,
        }
    }

    /// How late the fault answers a PUT.
    #[must_use]
    pub fn delay(self) -> Duration {
        match self {
            Self::Slow { by, .. } => by,
            _ => Duration::ZERO,
        }
    }

    /// How many PUTs a counted fault meets.
    #[must_use]
    pub fn count(self) -> u32 {
        match self {
            Self::FailNext { n } | Self::LoseAck { n } | Self::KeepButFail { n } => n,
            Self::Pause { .. } | Self::Slow { .. } | Self::FailUntil { .. } => 0,
        }
    }
}

/// When the store meets its faults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorePlan {
    pub faults: Vec<(FarmTime, StoreFault)>,
}

/// What the store did, for the tests to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// PUTs received.
    pub puts: u64,
    /// PUTs held by a pause.
    pub held: u64,
    /// PUTs answered late by a slow window.
    pub slowed: u64,
    /// PUTs failed, keeping nothing.
    pub failed: u64,
    /// PUTs kept whose acknowledgement was lost.
    pub lost_acks: u64,
    /// PUTs kept but answered as failed.
    pub false_fails: u64,
}

/// How a fault met one PUT, for the tests to tie an outcome to the fault that caused it
/// (a network duplicate of a PUT or an upload can leave an orphan too).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Met {
    /// Failed, keeping nothing.
    Failed,
    /// Kept, its acknowledgement lost.
    LostAck,
    /// Kept, answered as failed.
    FalseFail,
}

const T_FAULT: u64 = 0;
const T_REPLY: u64 = 1 << 32;

/// A PUT whose answer waits: the sender, the object and what it holds.
type Pending = (NodeId, ObjectId, Held);

/// The object store node.
#[derive(Debug, Default)]
pub struct ObjectStore {
    plan: StorePlan,
    objects: BTreeMap<ObjectId, Held>,
    paused_until: Option<FarmTime>,
    slow: Option<(FarmTime, Duration)>,
    fail_until: Option<FarmTime>,
    fail_next: u32,
    lose_next: u32,
    keep_fail_next: u32,
    held: Vec<Pending>,
    /// Answers a slow window delays, by timer tag.
    delayed: BTreeMap<u64, Pending>,
    next_reply: u64,
    stats: StoreStats,
    /// Each PUT a counted or refusing fault met: its object, its digest and how.
    met: Vec<(ObjectId, Digest, Met)>,
    /// Each object PUT again while the store held it: a network duplicate of a PUT.
    repeated: BTreeSet<ObjectId>,
    /// Each flushed object a fault met, and how.
    flush_met: Vec<(ObjectId, Met)>,
    /// Each object replaced with other content.
    rewrites: BTreeSet<ObjectId>,
    out: Vec<Output<Msg>>,
}

impl ObjectStore {
    /// A store that meets `plan`'s faults.
    #[must_use]
    pub fn new(plan: StorePlan) -> Self {
        Self {
            plan,
            ..Self::default()
        }
    }

    /// Whether some object holds `digest`'s bytes, readable now.
    #[must_use]
    pub fn holds_digest(&self, digest: &Digest) -> bool {
        self.objects.values().any(|held| held.holds(digest))
    }

    /// The objects the store keeps, and what each holds.
    pub fn objects(&self) -> impl Iterator<Item = (ObjectId, &Held)> {
        self.objects.iter().map(|(o, held)| (*o, held))
    }

    /// Each flushed segment a refusal, a lost acknowledgement or a false failure met.
    #[must_use]
    pub fn flush_met(&self) -> &[(ObjectId, Met)] {
        &self.flush_met
    }

    /// Each object a PUT replaced with other content: a flush of an object that did
    /// not write the same bytes again (invariant C), or an id reused.
    #[must_use]
    pub fn rewrites(&self) -> &BTreeSet<ObjectId> {
        &self.rewrites
    }

    /// What the store did.
    #[must_use]
    pub fn stats(&self) -> StoreStats {
        self.stats
    }

    /// Each PUT a refusal, a lost acknowledgement or a false failure met, in order.
    #[must_use]
    pub fn met(&self) -> &[(ObjectId, Digest, Met)] {
        &self.met
    }

    /// Each object PUT again while the store held it (a network duplicate of a PUT,
    /// whose second answer may be done where the first was lost or failed).
    #[must_use]
    pub fn repeated(&self) -> &BTreeSet<ObjectId> {
        &self.repeated
    }

    pub(crate) fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }

    fn keep_and_answer(&mut self, (to, object, held): Pending) {
        let before = self.objects.insert(object, held.clone());
        if before.is_some_and(|b| b != held) {
            self.rewrites.insert(object);
        }
        if self.lose_next > 0 {
            self.lose_next -= 1;
            self.stats.lost_acks += 1;
            self.record(object, &held, Met::LostAck);
            return;
        }
        let failed = self.keep_fail_next > 0;
        if failed {
            self.keep_fail_next -= 1;
            self.stats.false_fails += 1;
            self.record(object, &held, Met::FalseFail);
        }
        self.answer(to, object, &held, !failed);
    }

    /// Records that a fault met the PUT of `object`.
    fn record(&mut self, object: ObjectId, held: &Held, how: Met) {
        match held {
            Held::Blob(digest) => self.met.push((object, *digest, how)),
            Held::Segment(_) => self.flush_met.push((object, how)),
        }
    }

    /// Answers the PUT of `object`: done, or failed.
    fn answer(&mut self, to: NodeId, object: ObjectId, held: &Held, done: bool) {
        let msg = match (held, done) {
            (Held::Blob(digest), true) => Msg::PutDone {
                object,
                digest: *digest,
            },
            (Held::Blob(digest), false) => Msg::PutFailed {
                object,
                digest: *digest,
            },
            (Held::Segment(_), true) => Msg::FlushDone { object },
            (Held::Segment(_), false) => Msg::FlushFailed { object },
        };
        self.out.push(Output::Send { to, msg });
    }

    fn put(&mut self, now: FarmTime, pending: Pending) {
        self.stats.puts += 1;
        if self.objects.contains_key(&pending.1) {
            self.repeated.insert(pending.1);
        }
        if self.paused_until.is_some_and(|t| now < t) {
            self.stats.held += 1;
            self.held.push(pending);
            return;
        }
        let failing = self.fail_until.is_some_and(|t| now < t);
        if failing || self.fail_next > 0 {
            if !failing {
                self.fail_next -= 1;
            }
            self.stats.failed += 1;
            let (to, object, held) = pending;
            self.record(object, &held, Met::Failed);
            self.answer(to, object, &held, false);
            return;
        }
        if let Some((_, by)) = self.slow.filter(|(until, _)| now < *until) {
            self.stats.slowed += 1;
            let tag = T_REPLY + self.next_reply;
            self.next_reply += 1;
            self.delayed.insert(tag, pending);
            self.out.push(Output::Timer { after: by, tag });
            return;
        }
        self.keep_and_answer(pending);
    }

    fn begin(&mut self, fault: StoreFault, now: FarmTime) {
        match fault {
            StoreFault::Pause { until } => {
                self.paused_until = Some(until);
                let after = until.saturating_duration_since(now);
                // The tag past the faults' range releases the held PUTs.
                self.out.push(Output::Timer {
                    after,
                    tag: T_REPLY - 1,
                });
            }
            StoreFault::Slow { until, by } => self.slow = Some((until, by)),
            StoreFault::FailNext { n } => self.fail_next += n,
            StoreFault::FailUntil { until } => self.fail_until = Some(until),
            StoreFault::LoseAck { n } => self.lose_next += n,
            StoreFault::KeepButFail { n } => self.keep_fail_next += n,
        }
    }

    fn release_held(&mut self, now: FarmTime) {
        if self.paused_until.is_some_and(|t| now < t) {
            return;
        }
        self.paused_until = None;
        for pending in std::mem::take(&mut self.held) {
            self.keep_and_answer(pending);
        }
    }
}

impl BlobHomes for ObjectStore {
    fn holds(&self, object: ObjectId, digest: &Digest) -> bool {
        self.objects
            .get(&object)
            .is_some_and(|held| held.holds(digest))
    }
}

impl StateMachine for ObjectStore {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        let now = input.now;
        match input.event {
            Event::Start => {
                for (i, (at, _)) in self.plan.faults.iter().enumerate() {
                    let after = at.saturating_duration_since(now);
                    let tag = T_FAULT + i as u64;
                    self.out.push(Output::Timer { after, tag });
                }
            }
            Event::Timer { tag } if tag == T_REPLY - 1 => self.release_held(now),
            Event::Timer { tag } if tag >= T_REPLY => {
                let pending = self.delayed.remove(&tag);
                self.keep_and_answer(pending.expect("each reply timer fires once"));
            }
            Event::Timer { tag } => {
                let index = usize::try_from(tag - T_FAULT).expect("a fault index");
                let fault = self.plan.faults[index].1;
                self.begin(fault, now);
            }
            Event::Message { from, msg } => match msg {
                Msg::Put { object, digest } => self.put(now, (from, object, Held::Blob(digest))),
                Msg::Flush { object, body } => {
                    self.put(now, (from, object, Held::Segment(body)));
                }
                Msg::Get {
                    object,
                    digest,
                    tag,
                } => {
                    let held = self.holds(object, &digest);
                    let msg = Msg::Got { tag, held };
                    self.out.push(Output::Send { to: from, msg });
                }
                _ => {}
            },
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use kbf_meta::Epoch;
    use kbf_types::DigestFunction;

    use super::*;
    use crate::content;

    fn digest(n: u8) -> Digest {
        Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
    }

    fn object(n: u64) -> ObjectId {
        ObjectId::new(Epoch::new(1), n)
    }

    fn at(ms: u64, event: Event<Msg>) -> NodeInput<Msg> {
        NodeInput {
            now: FarmTime::from_millis(ms),
            entropy: 0,
            event,
        }
    }

    fn put(store: &mut ObjectStore, ms: u64, n: u8) -> Vec<Output<Msg>> {
        let msg = Msg::Put {
            object: object(n.into()),
            digest: digest(n),
        };
        let from = NodeId::from("server");
        store.apply(at(ms, Event::Message { from, msg }));
        store.take_outputs()
    }

    /// The store with `faults` started and every fault begun at its time.
    fn started(faults: Vec<(FarmTime, StoreFault)>) -> ObjectStore {
        let n = faults.len() as u64;
        let mut store = ObjectStore::new(StorePlan { faults });
        store.apply(at(0, Event::Start));
        store.take_outputs();
        for tag in 0..n {
            store.apply(at(0, Event::Timer { tag }));
        }
        store.take_outputs();
        store
    }

    fn answers(out: &[Output<Msg>]) -> Vec<&'static str> {
        out.iter()
            .filter_map(|o| match o {
                Output::Send {
                    msg: Msg::PutDone { .. },
                    ..
                } => Some("done"),
                Output::Send {
                    msg: Msg::PutFailed { .. },
                    ..
                } => Some("failed"),
                _ => None,
            })
            .collect()
    }

    /// Catches a pause that answers or keeps a PUT before it ends, or never releases
    /// what it held.
    #[test]
    fn a_pause_holds_puts_until_it_ends() {
        let until = FarmTime::from_millis(60_000);
        let mut s = started(vec![(FarmTime::default(), StoreFault::Pause { until })]);
        assert!(answers(&put(&mut s, 10, 1)).is_empty());
        assert!(!s.holds(object(1), &digest(1)));
        s.apply(at(59_999, Event::Timer { tag: T_REPLY - 1 }));
        assert!(
            s.take_outputs().is_empty(),
            "released before the pause ends"
        );
        s.apply(at(60_000, Event::Timer { tag: T_REPLY - 1 }));
        assert_eq!(answers(&s.take_outputs()), ["done"]);
        assert!(s.holds(object(1), &digest(1)));
        assert_eq!(answers(&put(&mut s, 60_001, 2)), ["done"]);
        assert_eq!(s.stats().held, 1);
        // Only PUTs reach the store; anything else changes nothing.
        let from = NodeId::from("server");
        let msg = Msg::Uploaded { digest: digest(3) };
        s.apply(at(60_002, Event::Message { from, msg }));
        assert!(s.take_outputs().is_empty());
    }

    /// Catches a failing PUT that keeps its object or answers done, a count that does
    /// not run out, and a window that outlives its end.
    #[test]
    fn failures_keep_nothing_and_end() {
        let mut s = started(vec![(FarmTime::default(), StoreFault::FailNext { n: 1 })]);
        assert_eq!(answers(&put(&mut s, 10, 1)), ["failed"]);
        assert!(!s.holds_digest(&digest(1)));
        assert_eq!(answers(&put(&mut s, 11, 2)), ["done"]);
        // A count begun inside a window waits it out: its failure comes after.
        let until = FarmTime::from_millis(100);
        let mut s = started(vec![
            (FarmTime::default(), StoreFault::FailUntil { until }),
            (FarmTime::default(), StoreFault::FailNext { n: 1 }),
        ]);
        assert_eq!(answers(&put(&mut s, 10, 1)), ["failed"]);
        assert_eq!(answers(&put(&mut s, 99, 2)), ["failed"]);
        assert_eq!(answers(&put(&mut s, 100, 3)), ["failed"]);
        assert!(!s.holds_digest(&digest(1)));
        assert!(!s.holds_digest(&digest(3)));
        assert_eq!(answers(&put(&mut s, 101, 4)), ["done"]);
        assert_eq!(s.stats().failed, 3);
        let failed: Vec<_> = s.met().iter().map(|(o, _, m)| (o.seq(), *m)).collect();
        assert_eq!(
            failed,
            [(1, Met::Failed), (2, Met::Failed), (3, Met::Failed)]
        );
    }

    /// Catches a lost acknowledgement that also loses the object, a false failure that
    /// keeps nothing, and either one that does not run out.
    #[test]
    fn a_lost_ack_and_a_false_failure_keep_the_object() {
        let mut s = started(vec![
            (FarmTime::default(), StoreFault::LoseAck { n: 1 }),
            (FarmTime::default(), StoreFault::KeepButFail { n: 1 }),
        ]);
        assert!(answers(&put(&mut s, 1, 1)).is_empty());
        assert!(s.holds(object(1), &digest(1)));
        assert_eq!(answers(&put(&mut s, 2, 2)), ["failed"]);
        assert!(s.holds(object(2), &digest(2)));
        assert_eq!(answers(&put(&mut s, 3, 3)), ["done"]);
        let stats = s.stats();
        assert_eq!((stats.lost_acks, stats.false_fails, stats.puts), (1, 1, 3));
        let met = [
            (object(1), digest(1), Met::LostAck),
            (object(2), digest(2), Met::FalseFail),
        ];
        assert_eq!(s.met(), met);
        assert!(s.repeated().is_empty());
        put(&mut s, 4, 1);
        assert_eq!(s.repeated(), &BTreeSet::from([object(1)]));
    }

    /// Catches a slow window that answers at once, keeps the object late or loses it,
    /// or outlives its end.
    #[test]
    fn a_slow_window_answers_late() {
        let until = FarmTime::from_millis(1_000);
        let by = Duration::from_millis(700);
        let mut s = started(vec![(FarmTime::default(), StoreFault::Slow { until, by })]);
        let out = put(&mut s, 10, 1);
        assert!(answers(&out).is_empty());
        let late = [Output::<Msg>::Timer {
            after: by,
            tag: T_REPLY,
        }];
        assert_eq!(format!("{out:?}"), format!("{late:?}"));
        s.apply(at(710, Event::Timer { tag: T_REPLY }));
        assert_eq!(answers(&s.take_outputs()), ["done"]);
        assert!(s.holds(object(1), &digest(1)));
        assert_eq!(answers(&put(&mut s, 1_000, 2)), ["done"]);
        assert_eq!(s.stats().slowed, 1);
        let slow = StoreFault::Slow { until, by };
        assert_eq!(
            (slow.until(), slow.delay(), slow.count()),
            (Some(until), by, 0)
        );
        let lose = StoreFault::LoseAck { n: 2 };
        assert_eq!(
            (lose.until(), lose.delay(), lose.count()),
            (None, Duration::ZERO, 2)
        );
    }

    /// Catches a flushed segment taken as holding a blob it does not list or whose
    /// bytes differ, a rewrite with other bytes not recorded (or one with the same
    /// bytes recorded), a segment PUT answered as a blob's, and a read answered from
    /// another object.
    #[test]
    fn a_segment_is_held_by_its_bytes_and_a_rewrite_is_recorded() {
        let (a, b) = (digest(1), digest(2));
        let mut bytes = content(&a);
        bytes.extend(content(&b));
        let body = Body {
            blobs: vec![(a, 0), (b, 1)],
            bytes,
        };
        let mut s = started(vec![(FarmTime::default(), StoreFault::FailNext { n: 1 })]);
        let from = NodeId::from("server");
        let flush = |s: &mut ObjectStore, ms, body: &Body| {
            let msg = Msg::Flush {
                object: object(7),
                body: body.clone(),
            };
            s.apply(at(
                ms,
                Event::Message {
                    from: from.clone(),
                    msg,
                },
            ));
            format!("{:?}", s.take_outputs())
        };
        assert!(flush(&mut s, 1, &body).contains("FlushFailed"));
        assert_eq!(s.flush_met(), [(object(7), Met::Failed)]);
        assert!(flush(&mut s, 2, &body).contains("FlushDone"));
        assert!(s.holds(object(7), &a));
        assert!(s.holds(object(7), &b));
        assert!(!s.holds(object(7), &digest(3)));
        assert!(s.holds_digest(&b));
        flush(&mut s, 3, &body);
        assert!(s.rewrites().is_empty(), "the same bytes again");
        let mut other = body.clone();
        other.bytes[1] ^= 0xFF;
        flush(&mut s, 4, &other);
        assert!(!s.holds(object(7), &b), "other bytes");
        assert_eq!(s.rewrites(), &BTreeSet::from([object(7)]));
        let get = |s: &mut ObjectStore, o| {
            let msg = Msg::Get {
                object: object(o),
                digest: a,
                tag: 5,
            };
            s.apply(at(
                5,
                Event::Message {
                    from: from.clone(),
                    msg,
                },
            ));
            format!("{:?}", s.take_outputs())
        };
        assert!(get(&mut s, 7).contains("held: true"));
        assert!(get(&mut s, 8).contains("held: false"));
    }
}
