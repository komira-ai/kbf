//! The F3 checker: a shadow of what the scheduler was fed and what it emitted, and the
//! invariants of `docs/design/simulation.md` section 4 that F3 needs, checked after
//! every input the scheduler is fed.
//!
//! The shadow is a reference model, not a copy of the scheduler's state: it keeps its
//! own cordons and drains, its own bookings, liveness, holdings and queue, computes the
//! placement round and each queued operation's verdict by first fit over the workers in
//! name order, and compares what the scheduler did with that.
//!
//! Checked: I1 (lease ids unique, increasing), I2 (`Start` only for the current,
//! committed grant), I3 (one holding per operation; `leases_on` is the holdings), I4
//! (finished once, each waiter answered once), I5 (an answer names the newest
//! committed grant, and carries the outcome that lease's run produced; F3's world
//! never sends a stale or fenced result, because a worker that goes down drops its
//! runs, so here I5 proves only that the outcome is passed through: a scheduler that
//! accepts a fenced result is F2's to catch), I6 (bookings
//! fit at a grant; `booked` is the sum of the holdings), I7 (grants go to a live worker
//! whose last reported capabilities satisfy the platform), I8 (no grant to a cordoned
//! worker, across sessions), I9 (a lease is given up only when its worker went down
//! after the grant or was down at it; the cordon and drain state is the reference's
//! exactly), I10 and L2 (refused exactly
//! at the tick an unbroken unservable run reaches the wait; work only cordoned workers
//! could run is never refused; the reason is the last one stated, the wait appended),
//! I11 (each round's grants are the reference first fit's, in order), I14 (the queue is
//! the queued operations, in urgency then submission order). I12 and I13 belong to the
//! cell and dedup families (F2, F1): every request here is unique, and no daemon is
//! modelled. L1 is [`Check::finish`].
//!
//! It is local to F3 on purpose: the shared checker of the catalog
//! (`tests/sim/check.rs`) lands with F1, written in parallel; this one moves there
//! once both are in.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{Cordon, Event, FARM_RERUNS, Input, OpState, PLACEMENT_ROUND, Request, Scheduler};
use kbf_types::{
    ControlRecord, Digest, DigestFunction, Effect, Failure, FarmTime, LeaseId, LostWhy,
    OperationId, Outcome, Resources, WaiterId, WorkerId,
};

/// The scheduler's grace G, in milliseconds.
pub const GRACE_MS: u64 = 60_000;
/// The prefix of the reason work waits with while only cordoned workers could run it.
pub const CORDON_REASON: &str = "every live worker that can run it is cordoned";

/// The digest a run of `lease` reports, so an answer shows which run it carries (I5).
pub fn outcome_of(lease: LeaseId) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&lease.term.to_be_bytes());
    hash[8..16].copy_from_slice(&lease.seq.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, 1)
}

/// The reference verdict on a queued operation, for one round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// A live, uncordoned worker could run it.
    Servable,
    /// Only a live, cordoned one could.
    Cordoned,
    /// None could.
    Unservable,
}

/// Whether `request` is an action at the whole host's memory of a worker of `capacity`
/// (the last step of the memory ladder): it runs alone on the node.
fn exclusive(capacity: Resources, request: Resources) -> bool {
    request.memory_bytes > 0 && request.memory_bytes >= capacity.memory_bytes
}

/// Whether an action `request` can be placed on a worker of `capacity` whose holdings
/// book `booked` in `leases` leases, `whole` saying one holds the node alone. An
/// action's memory is the limit it first runs under, not a booking: only the worker's
/// memory capacity bounds it, besides the whole host running alone.
fn room(
    capacity: Resources,
    booked: Resources,
    whole: bool,
    leases: usize,
    request: Resources,
) -> bool {
    !whole
        && request.cpu_millis <= capacity.cpu_millis.saturating_sub(booked.cpu_millis)
        && request.gpus <= capacity.gpus.saturating_sub(booked.gpus)
        && request.vms <= capacity.vms.saturating_sub(booked.vms)
        && request.memory_bytes <= capacity.memory_bytes
        && (!exclusive(capacity, request) || leases == 0)
}

#[derive(Clone, Debug)]
struct ShadowWorker {
    caps: NodeCaps,
    capacity: Resources,
    last_heard: Option<u64>,
    /// How many times it went down (lost its runs).
    downs: u64,
    /// Whether it is down now: a `Start` sent to it is lost.
    down: bool,
}

#[derive(Clone, Debug)]
struct ShadowOp {
    request: Request,
    waiter: WaiterId,
    done: bool,
    /// Its refusal is proposed, not yet committed.
    refusing: bool,
    /// The reason its callers were last told; `None` once told it is servable.
    told: Option<String>,
    /// Its class at the last round it was queued in.
    last_class: Option<Class>,
    /// Since when (ms) its unbroken unservable run lasts.
    since: Option<u64>,
    /// The workers whose silence ended a sent lease of it: placement prefers others.
    avoid: BTreeSet<WorkerId>,
    /// How many sent leases of it ended in silence (the farm budget, which only
    /// silence spends in F3).
    charged: u32,
    /// Its silent worker's lease is the farm budget's last: the scheduler proposed a
    /// fenced result for it, which answers the operation once committed.
    silenced: bool,
}

#[derive(Clone, Debug)]
struct Holding {
    lease: LeaseId,
    worker: WorkerId,
    /// The worker's `downs` at the grant.
    downs: u64,
    /// The worker was down at the grant: its `Start` is lost.
    lost: bool,
    /// It holds the node alone: an action at the worker's whole memory when granted.
    alone: bool,
    committed: bool,
    /// A worker's result of it was proposed (a silent worker's lease is then given up
    /// with no charge: that result decides the operation).
    proposed: bool,
}

/// What a round must produce, computed before it is fed.
#[derive(Debug, Default)]
pub struct Round {
    grants: Vec<(OperationId, WorkerId)>,
    refusals: BTreeSet<OperationId>,
    classes: BTreeMap<OperationId, Class>,
}

/// What the scenarios read back.
#[derive(Debug, Default)]
pub struct Seen {
    /// Per operation: when (ms) it was granted, where, and whether by an uncordon.
    pub grants: BTreeMap<OperationId, Vec<(u64, WorkerId, bool)>>,
    /// Per operation: when (ms) it was refused.
    pub refused: BTreeMap<OperationId, u64>,
    /// Per operation: when (ms) it was answered with a result.
    pub answered: BTreeMap<OperationId, u64>,
    /// Leases given up: when (ms), the operation, the worker.
    pub given_up: Vec<(u64, OperationId, WorkerId)>,
    /// Per operation: since when (ms) it waits with the cordon reason.
    pub cordon_wait_from: BTreeMap<OperationId, u64>,
    /// The longest wait (ms) with the cordon reason that ended servable.
    pub longest_cordon_wait: u64,
    /// Situations reached, by name, so a sweep can show it reached them.
    pub reached: BTreeMap<&'static str, u64>,
}

/// The F3 checker.
pub struct Check {
    /// `scenario seed`, for the failure message.
    label: (&'static str, u64),
    wait_ms: u64,
    now: u64,
    step: u64,
    workers: BTreeMap<WorkerId, ShadowWorker>,
    cordons: BTreeMap<WorkerId, Cordon>,
    ops: BTreeMap<OperationId, ShadowOp>,
    holding: BTreeMap<OperationId, Holding>,
    newest_grant: Option<LeaseId>,
    newest_committed: BTreeMap<OperationId, LeaseId>,
    answered_waiters: BTreeSet<WaiterId>,
    pub seen: Seen,
}

impl Check {
    pub fn new(scenario: &'static str, seed: u64, wait: Duration) -> Self {
        Self {
            label: (scenario, seed),
            wait_ms: u64::try_from(wait.as_millis()).expect("small"),
            now: 0,
            step: 0,
            workers: BTreeMap::new(),
            cordons: BTreeMap::new(),
            ops: BTreeMap::new(),
            holding: BTreeMap::new(),
            newest_grant: None,
            newest_committed: BTreeMap::new(),
            answered_waiters: BTreeSet::new(),
            seen: Seen::default(),
        }
    }

    /// Panics with the seed, the step, the invariant and the replay command.
    #[track_caller]
    pub fn fail(&self, invariant: &str, what: &str) -> ! {
        let (scenario, seed) = self.label;
        panic!(
            "{invariant} violated: {what}\n  scenario {scenario}, seed {seed}, step {}, t={} ms\n  \
             replay: KBF_SIM_SCENARIO={scenario} KBF_SIM_SEED={seed} cargo test -p kbf-sched \
             --test f3_fleet -- --ignored --exact replay",
            self.step, self.now
        );
    }

    #[track_caller]
    fn ensure(&self, ok: bool, invariant: &str, what: impl FnOnce() -> String) {
        if !ok {
            self.fail(invariant, &what());
        }
    }

    pub fn reach(&mut self, what: &'static str) {
        *self.seen.reached.entry(what).or_default() += 1;
    }

    fn live(&self, worker: &WorkerId) -> bool {
        self.workers[worker]
            .last_heard
            .is_some_and(|h| self.now < h + GRACE_MS)
    }

    /// The worker went down (its runs are lost, so its leases may be given up), or
    /// came back up.
    pub fn down(&mut self, worker: &WorkerId, down: bool) {
        let w = self.workers.get_mut(worker).expect("known");
        w.downs += u64::from(down);
        w.down = down;
    }

    /// The operation `id` an input submits, as the scheduler numbers it.
    pub fn next_op(&self) -> OperationId {
        OperationId(self.ops.len() as u64)
    }

    /// Updates the shadow for `input`, and for a round (a tick or an uncordon) returns
    /// what it must produce.
    pub fn before(&mut self, input: &Input) -> Option<Round> {
        self.step += 1;
        self.now = self.now.max(input.now.as_millis());
        let now = self.now;
        match &input.event {
            Event::WorkerUp {
                worker,
                capacity,
                caps,
                ..
            } => {
                let w = self
                    .workers
                    .entry(worker.clone())
                    .or_insert_with(|| ShadowWorker {
                        caps: caps.clone(),
                        capacity: *capacity,
                        last_heard: None,
                        downs: 0,
                        down: false,
                    });
                w.caps = caps.clone();
                w.capacity = *capacity;
                w.last_heard = Some(now);
                None
            }
            Event::Capacity {
                worker,
                capacity,
                caps,
            } => {
                let w = self.workers.get_mut(worker).expect("registered");
                w.caps = caps.clone();
                w.capacity = *capacity;
                w.last_heard = Some(now);
                None
            }
            Event::Heartbeat { worker, .. } => {
                self.workers.get_mut(worker).expect("registered").last_heard = Some(now);
                None
            }
            // The fleet switches no node's session.
            Event::Phase { .. } => unreachable!("F3 feeds no node phase"),
            Event::Submit { waiter, request } => {
                let id = self.next_op();
                self.ops.insert(
                    id,
                    ShadowOp {
                        request: request.clone(),
                        waiter: *waiter,
                        done: false,
                        refusing: false,
                        told: None,
                        last_class: None,
                        since: None,
                        avoid: BTreeSet::new(),
                        charged: 0,
                        silenced: false,
                    },
                );
                None
            }
            Event::Cordon { worker } => {
                self.cordons
                    .entry(worker.clone())
                    .or_insert(Cordon::Cordoned);
                None
            }
            Event::Drain { worker, deadline } => {
                let deadline = *deadline;
                self.cordons
                    .insert(worker.clone(), Cordon::Draining { deadline });
                None
            }
            Event::Uncordon { worker } => {
                self.cordons.remove(worker);
                Some(self.round())
            }
            Event::Tick => {
                self.expire();
                Some(self.round())
            }
            Event::Committed(ControlRecord::Lease(grant)) => {
                let newest = self
                    .newest_committed
                    .entry(grant.operation)
                    .or_insert(grant.lease);
                *newest = (*newest).max(grant.lease);
                if let Some(h) = self.holding.get_mut(&grant.operation)
                    && h.lease == grant.lease
                {
                    h.committed = true;
                }
                None
            }
            Event::Report {
                operation, lease, ..
            } => {
                if let Some(h) = self.holding.get_mut(operation)
                    && h.lease == *lease
                    && h.committed
                {
                    h.proposed = true;
                }
                None
            }
            Event::Committed(_) | Event::Started { .. } => None,
        }
    }

    /// A tick first gives up every holding on a worker not heard within G.
    fn expire(&mut self) {
        let expired: Vec<OperationId> = self
            .holding
            .iter()
            .filter(|(id, h)| !self.live(&h.worker) && !self.ops[id].silenced)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            // A sent lease given up for silence spends the operation's farm budget;
            // past it the lease is kept until its proposed result is committed.
            let spent = self.ops[&id].charged >= FARM_RERUNS;
            if self.holding[&id].committed && !self.holding[&id].proposed && spent {
                self.charge(id);
                self.ops.get_mut(&id).expect("known").silenced = true;
            } else {
                self.give_up(id);
            }
        }
    }

    /// The reference round at `now`: first fit in queue order.
    fn round(&mut self) -> Round {
        // Per worker: what its holdings book, how many there are, and whether one holds
        // the node alone.
        let mut booked: BTreeMap<WorkerId, (Resources, usize, bool)> = BTreeMap::new();
        let mut queue: Vec<OperationId> = self
            .ops
            .iter()
            .filter(|(id, op)| !op.done && !op.refusing && !self.holding.contains_key(id))
            .map(|(id, _)| *id)
            .collect();
        for (id, h) in &self.holding {
            let b = booked.entry(h.worker.clone()).or_default();
            *b = (
                b.0.saturating_add(self.ops[id].request.resources),
                b.1 + 1,
                b.2 || h.alone,
            );
        }
        queue.sort_by_key(|id| (Reverse(self.ops[id].request.qos.clone()), *id));
        let names: Vec<WorkerId> = self.workers.keys().cloned().collect();
        let mut round = Round::default();
        for id in queue {
            let request = &self.ops[&id].request;
            let usable = |w: &WorkerId, cordoned: bool| {
                let s = &self.workers[w];
                self.live(w)
                    && self.cordons.contains_key(w) == cordoned
                    && request.needs.matches(&s.caps)
            };
            // Placement skips the workers of `avoid` while another could run it.
            let elsewhere = names.iter().any(|w| {
                usable(w, false)
                    && !self.ops[&id].avoid.contains(w)
                    && self.workers[w].capacity.fits(&request.resources)
            });
            let fit = names.iter().find(|w| {
                let (held, leases, whole) = booked.get(*w).copied().unwrap_or_default();
                usable(w, false)
                    && room(
                        self.workers[*w].capacity,
                        held,
                        whole,
                        leases,
                        request.resources,
                    )
                    && !(elsewhere && self.ops[&id].avoid.contains(*w))
            });
            let class = match fit {
                Some(w) if round.grants.len() < PLACEMENT_ROUND => {
                    let alone = exclusive(self.workers[w].capacity, request.resources);
                    let b = booked.entry(w.clone()).or_default();
                    *b = (b.0.saturating_add(request.resources), b.1 + 1, b.2 || alone);
                    round.grants.push((id, w.clone()));
                    Class::Servable
                }
                _ => {
                    let large = |w: &&WorkerId| self.workers[*w].capacity.fits(&request.resources);
                    if names.iter().filter(|w| usable(w, false)).any(|w| large(&w)) {
                        Class::Servable
                    } else if names.iter().filter(|w| usable(w, true)).any(|w| large(&w)) {
                        Class::Cordoned
                    } else {
                        Class::Unservable
                    }
                }
            };
            let op = self.ops.get_mut(&id).expect("queued ops exist");
            op.since = match (class, op.last_class) {
                (Class::Unservable, Some(Class::Unservable)) => op.since,
                (Class::Unservable, _) => Some(self.now),
                _ => None,
            };
            op.last_class = Some(class);
            if op.since.is_some_and(|s| self.now >= s + self.wait_ms) {
                round.refusals.insert(id);
            }
            round.classes.insert(id, class);
        }
        round
    }

    /// Checks what the scheduler emitted for one input, then its state against the
    /// shadow's.
    #[allow(clippy::too_many_lines)]
    pub fn after(
        &mut self,
        sched: &Scheduler,
        input: &Input,
        effects: &[Effect],
        round: Option<&Round>,
    ) {
        let now = self.now;
        let by_uncordon = matches!(input.event, Event::Uncordon { .. });
        let mut grants = Vec::new();
        let mut refusals = BTreeSet::new();
        let mut finished = BTreeSet::new();
        for effect in effects {
            match effect {
                Effect::Commit(ControlRecord::Lease(g)) => {
                    let op = &self.ops[&g.operation];
                    let w = &self.workers[&g.worker];
                    self.ensure(self.newest_grant < Some(g.lease), "I1", || {
                        format!("{g:?} after {:?}", self.newest_grant)
                    });
                    self.ensure(
                        !op.done && !self.holding.contains_key(&g.operation),
                        "I3",
                        || format!("{g:?} for an operation that is not queued"),
                    );
                    self.ensure(self.live(&g.worker), "I7", || {
                        format!("{g:?} to a worker not heard within G")
                    });
                    self.ensure(op.request.needs.matches(&w.caps), "I7", || {
                        format!(
                            "{g:?} to a worker whose caps do not satisfy {:?}",
                            op.request.needs
                        )
                    });
                    self.ensure(!self.cordons.contains_key(&g.worker), "I8", || {
                        format!("{g:?} to a worker that is {:?}", self.cordons[&g.worker])
                    });
                    let booked = self.booked_on(&g.worker);
                    let (leases, whole) = self.holds_on(&g.worker);
                    self.ensure(
                        room(w.capacity, booked, whole, leases, op.request.resources),
                        "I6",
                        || {
                            format!(
                                "{g:?}: {booked:?} booked in {leases} leases (alone: {whole}) \
                                 and the request do not fit {:?}",
                                w.capacity
                            )
                        },
                    );
                    self.newest_grant = Some(g.lease);
                    let (downs, lost) = (w.downs, w.down);
                    self.holding.insert(
                        g.operation,
                        Holding {
                            lease: g.lease,
                            worker: g.worker.clone(),
                            downs,
                            lost,
                            alone: exclusive(w.capacity, op.request.resources),
                            committed: false,
                            proposed: false,
                        },
                    );
                    let at = (now, g.worker.clone(), by_uncordon);
                    self.seen.grants.entry(g.operation).or_default().push(at);
                    grants.push((g.operation, g.worker.clone()));
                }
                Effect::Commit(ControlRecord::Refusal(r)) => {
                    let op = &self.ops[&r.operation];
                    let told = op.told.clone().unwrap_or_default();
                    self.ensure(op.told.is_some(), "I10", || {
                        format!("{} refused, its callers never told why", r.operation)
                    });
                    let wait = self.wait_ms / 1_000;
                    let want = format!("{told} (waited {wait} s for a worker that can run it)");
                    self.ensure(r.reason == want, "I10", || {
                        format!("refusal reason {:?}, want {want:?}", r.reason)
                    });
                    self.ops.get_mut(&r.operation).expect("known").refusing = true;
                    refusals.insert(r.operation);
                }
                Effect::Commit(ControlRecord::Result(r)) => {
                    // The proposal of a lease given up past the farm budget (silence
                    // is predicted at the tick; a replacement shows here). A worker's
                    // own result is a completion.
                    if r.outcome
                        != (Outcome::Completed {
                            action_result: outcome_of(r.lease),
                        })
                    {
                        let fenced = Outcome::Failed(Failure::Lost(LostWhy::Fenced));
                        self.ensure(r.outcome == fenced, "I5", || {
                            format!("{r:?}: the only failure proposed is a give-up's")
                        });
                        if !self.ops[&r.operation].silenced {
                            let charged = self.ops[&r.operation].charged;
                            self.ensure(charged >= FARM_RERUNS, "I5", || {
                                format!("{r:?} proposed within the farm budget")
                            });
                            self.charge(r.operation);
                            self.ops.get_mut(&r.operation).expect("known").silenced = true;
                        }
                    }
                    let h = self.holding.get(&r.operation);
                    self.ensure(h.is_some_and(|h| h.lease == r.lease), "I5", || {
                        format!("result of {:?} proposed; the holding is {h:?}", r.lease)
                    });
                }
                Effect::Commit(other) => self.fail("scheduler", &format!("{other:?}")),
                Effect::Start(s) => {
                    let h = self.holding.get(&s.operation);
                    let current = h
                        .is_some_and(|h| h.lease == s.lease && h.worker == s.worker && h.committed);
                    self.ensure(current, "I2", || {
                        format!("{s:?} is not the committed, current holding {h:?}")
                    });
                }
                Effect::Answer(a) => {
                    if self.ops[&a.operation].silenced {
                        // Silent past the farm budget: answered as a fenced run.
                        let fenced = Outcome::Failed(Failure::Lost(LostWhy::Fenced));
                        self.ensure(a.outcome == fenced, "I5", || {
                            format!("{a:?}: a silent node's last lease is answered as fenced")
                        });
                    } else {
                        let Outcome::Completed { action_result } = a.outcome else {
                            self.fail("I5", &format!("{a:?}: no run failed"));
                        };
                        self.ensure(action_result == outcome_of(a.lease), "I5", || {
                            format!("{a:?} carries another lease's outcome")
                        });
                    }
                    let newest = self.newest_committed.get(&a.operation);
                    self.ensure(newest == Some(&a.lease), "I5", || {
                        format!("{a:?}: the newest committed grant is {newest:?}")
                    });
                    self.finish_op(a.operation, &a.waiters);
                    self.holding.remove(&a.operation);
                    self.seen.answered.insert(a.operation, now);
                    finished.insert(a.operation);
                }
                Effect::Refuse(r) => {
                    self.ensure(self.ops[&r.operation].refusing, "I10", || {
                        format!("{r:?} without a proposed refusal")
                    });
                    self.finish_op(r.operation, &r.waiters);
                    self.seen.refused.insert(r.operation, now);
                }
                Effect::Waiting(w) => {
                    let op = self.ops.get_mut(&w.operation).expect("known");
                    op.told.clone_from(&w.reason);
                    match w.reason.as_deref() {
                        Some(r) if r.starts_with(CORDON_REASON) => {
                            self.seen.cordon_wait_from.entry(w.operation).or_insert(now);
                        }
                        Some(_) => {
                            self.seen.cordon_wait_from.remove(&w.operation);
                        }
                        // Servable again: a wait for a cordon ends here.
                        None => {
                            if let Some(from) = self.seen.cordon_wait_from.remove(&w.operation) {
                                let waited = now - from;
                                self.seen.longest_cordon_wait =
                                    self.seen.longest_cordon_wait.max(waited);
                            }
                        }
                    }
                }
            }
        }
        self.given_up(sched, &finished);
        self.progress_drains();
        if let Some(round) = round {
            self.check_round(round, &grants, &refusals);
        }
        self.compare(sched);
    }

    fn finish_op(&mut self, id: OperationId, waiters: &[WaiterId]) {
        let op = self.ops.get_mut(&id).expect("known");
        let (done, waiter) = (op.done, op.waiter);
        op.done = true;
        self.ensure(!done, "I4", || format!("{id} finished twice"));
        self.ensure(waiters == [waiter], "I4", || {
            format!("{id} answers {waiters:?}, its waiter is {waiter:?}")
        });
        let first = self.answered_waiters.insert(waiter);
        self.ensure(first, "I4", || format!("{waiter:?} answered twice"));
    }

    /// Holdings the scheduler no longer has, though not finished: given up. Legitimate
    /// only when the worker went down since the grant (its run was lost); otherwise a
    /// live worker that lists the lease lost it, and a drain killed it (I9).
    fn given_up(&mut self, sched: &Scheduler, finished: &BTreeSet<OperationId>) {
        let gone: Vec<OperationId> = self
            .holding
            .iter()
            .filter(|(id, h)| !finished.contains(id) && holding(sched, **id) != Some(h.lease))
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.give_up(id);
        }
    }

    /// Spends one of `id`'s farm runs: its holding's worker is avoided from now on.
    fn charge(&mut self, id: OperationId) {
        let worker = self.holding[&id].worker.clone();
        let op = self.ops.get_mut(&id).expect("known");
        op.avoid.insert(worker);
        op.charged += 1;
    }

    fn give_up(&mut self, id: OperationId) {
        // The fleet's workers register a new process every time and lose a `Start` only
        // by going down: every sent lease given up is silent or replaced, both charged.
        if self.holding[&id].committed && !self.holding[&id].proposed {
            self.charge(id);
        }
        let h = self.holding.remove(&id).expect("held");
        let downs = self.workers[&h.worker].downs;
        self.ensure(h.lost || downs > h.downs, "I9", || {
            format!(
                "{:?} of {id} given up on {}, which never went down since the grant \
                 (cordon: {:?})",
                h.lease,
                h.worker,
                self.cordons.get(&h.worker)
            )
        });
        if self.cordons.contains_key(&h.worker) {
            self.reach("lease of a cordoned worker given up after it went down");
        }
        self.seen.given_up.push((self.now, id, h.worker));
    }

    /// How many leases `worker` holds, and whether one holds the node alone.
    fn holds_on(&self, worker: &WorkerId) -> (usize, bool) {
        let held = self.holding.values().filter(|h| h.worker == *worker);
        (held.clone().count(), held.clone().any(|h| h.alone))
    }

    fn booked_on(&self, worker: &WorkerId) -> Resources {
        self.holding
            .iter()
            .filter(|(_, h)| h.worker == *worker)
            .fold(Resources::default(), |b, (id, _)| {
                b.saturating_add(self.ops[id].request.resources)
            })
    }

    /// The reference's drains move on: drained once nothing is held, paused at the
    /// deadline. Nothing else moves a cordon but an operator.
    fn progress_drains(&mut self) {
        let held: BTreeSet<WorkerId> = self.holding.values().map(|h| h.worker.clone()).collect();
        let now = FarmTime::from_millis(self.now);
        for (worker, cordon) in &mut self.cordons {
            if let Cordon::Draining { deadline } = *cordon {
                if !held.contains(worker) {
                    *cordon = Cordon::Drained;
                } else if now >= deadline {
                    *cordon = Cordon::Paused { deadline };
                }
            }
        }
    }

    fn check_round(
        &mut self,
        round: &Round,
        grants: &[(OperationId, WorkerId)],
        refusals: &BTreeSet<OperationId>,
    ) {
        self.ensure(grants == round.grants, "I11", || {
            format!(
                "round granted {grants:?}, first fit gives {:?}",
                round.grants
            )
        });
        self.ensure(*refusals == round.refusals, "I10/L2", || {
            let mut why = String::new();
            for (id, op) in &self.ops {
                if refusals.contains(id) != round.refusals.contains(id) {
                    let _ = write!(
                        why,
                        " {id}: class {:?}, unservable since {:?}, told {:?};",
                        round.classes.get(id),
                        op.since,
                        op.told
                    );
                }
            }
            format!(
                "refused {refusals:?}, the reference refuses {:?}:{why}",
                round.refusals
            )
        });
        for (id, class) in &round.classes {
            let told = self.ops[id].told.as_deref();
            let ok = match class {
                Class::Servable => told.is_none(),
                Class::Cordoned => told.is_some_and(|r| r.starts_with(CORDON_REASON)),
                Class::Unservable => told.is_some_and(|r| !r.starts_with(CORDON_REASON)),
            };
            self.ensure(ok, "I10", || {
                format!("{id} is {class:?} by the reference, its callers were told {told:?}")
            });
            match class {
                Class::Cordoned => self.reach("work waits for a cordon"),
                Class::Unservable => self.reach("work waits unservable"),
                Class::Servable => {}
            }
        }
        if !refusals.is_empty() {
            self.reach("refusal");
        }
    }

    /// The scheduler's state against the shadow's: cordons, holdings, bookings, queue.
    fn compare(&mut self, sched: &Scheduler) {
        for name in self.workers.keys() {
            let cordon = sched.cordon(name);
            let want = self.cordons.get(name);
            self.ensure(cordon == want, "I9", || {
                format!("{name} is {cordon:?}; the reference says {want:?}")
            });
            let mine: Vec<LeaseId> = self
                .holding
                .values()
                .filter(|h| h.worker == *name)
                .map(|h| h.lease)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let leases = sched.leases_on(name);
            self.ensure(leases == mine, "I3", || {
                format!("{name} holds {leases:?}; the reference {mine:?}")
            });
            let booked = sched.booked(name);
            let want = self.booked_on(name);
            self.ensure(booked == Some(want), "I6", || {
                format!("{name} booked {booked:?}; its holdings sum to {want:?}")
            });
            match cordon {
                Some(Cordon::Drained) => self.ensure(leases.is_empty(), "I9", || {
                    format!("{name} drained while holding {leases:?}")
                }),
                Some(Cordon::Draining { deadline }) => self.ensure(
                    self.now < deadline.as_millis() && !leases.is_empty(),
                    "I9",
                    || format!("{name} draining at {} with {leases:?}", self.now),
                ),
                Some(Cordon::Paused { deadline }) => {
                    self.ensure(self.now >= deadline.as_millis(), "I9", || {
                        format!("{name} paused before {deadline:?}")
                    });
                }
                Some(Cordon::Cordoned) | None => {}
            }
        }
        for (id, h) in &self.holding {
            let state = holding(sched, *id);
            self.ensure(state == Some(h.lease), "I3", || {
                format!(
                    "{id} is {:?}; the reference holds {:?}",
                    sched.state(*id),
                    h.lease
                )
            });
        }
        let queued: Vec<OperationId> = sched.queued().collect();
        let mut want: Vec<OperationId> = self
            .ops
            .iter()
            .filter(|(id, op)| !op.done && !op.refusing && !self.holding.contains_key(id))
            .map(|(id, _)| *id)
            .collect();
        want.sort_by_key(|id| (Reverse(self.ops[id].request.qos.clone()), *id));
        self.ensure(queued == want, "I14", || {
            format!("queue {queued:?}; the queued operations in order are {want:?}")
        });
        for cordon in self.cordons.values() {
            let name = match cordon {
                Cordon::Cordoned => "cordoned",
                Cordon::Draining { .. } => "draining",
                Cordon::Drained => "drained",
                Cordon::Paused { .. } => "paused",
            };
            *self.seen.reached.entry(name).or_default() += 1;
        }
    }

    /// L1, once faults and arrivals stopped and the run drained: every operation
    /// finished, every waiter answered, nothing booked or held, the queue empty.
    pub fn finish(&self, sched: &Scheduler) {
        for (id, op) in &self.ops {
            self.ensure(op.done, "L1", || {
                format!(
                    "{id} unfinished at the end: {:?}, waiting {:?}, needs {:?}, cordons {:?}",
                    sched.state(*id),
                    sched.waiting(*id),
                    op.request.needs,
                    self.cordons
                )
            });
        }
        self.ensure(sched.queued().next().is_none(), "L1", || {
            "queue not empty".into()
        });
        for name in self.workers.keys() {
            let booked = sched.booked(name);
            self.ensure(booked == Some(Resources::default()), "L1", || {
                format!("{name} still books {booked:?}")
            });
        }
    }
}

/// The lease `id` holds in the scheduler, if any.
fn holding(sched: &Scheduler, id: OperationId) -> Option<LeaseId> {
    match sched.state(id)? {
        OpState::Leased { lease, .. } | OpState::Running { lease, .. } => Some(*lease),
        _ => None,
    }
}
