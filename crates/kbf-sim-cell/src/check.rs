//! Properties 1 to 7 of docs/design/ha.md section 15, for one server, checked after
//! every step (7 at the end of the run).
//!
//! 1. **Replica equality.** A replica that replays the server's durable log through
//!    the server's applied index has the server's applied state (and its `Bootstrap`
//!    names the server's log): every entry the server has applied, and so answered
//!    from, a restart or a follower would apply alike.
//! 2. **One run.** No operation (a term and an operation id) has two unfenced runs at
//!    once.
//! 3. **Leases kept.** No lease is given up while its daemon, heard within T, listed
//!    it.
//! 4. **Exactly one result.** No lease's result is accepted twice, and every result a
//!    daemon reports is accepted once by the end; an action has an action-cache entry
//!    if and only if a result of it was accepted.
//! 5. **No false present.** Every blob the server answers present is in the object
//!    store then. (No loss fault and no collection yet, so its later clauses are
//!    vacuous.)
//! 6. **Monotone time.** Farm time never goes back from one entry of the durable log to
//!    the next, nor across a restart.
//! 7. **Liveness.** Every client's every action is answered by the end of the run.
//!
//! Properties 3 and 4 do not hold yet once the server restarts or its log stops,
//! because the scheduler's state is not in the log: property 3 is enforced in
//! [`Kind::Calm`] and [`Kind::Partition`] runs only, until control inputs go through
//! the log (ha.md section 16, PR 11), and property 4 in [`Kind::Calm`] runs only, until
//! a result and its action-cache entry commit together (PR 12). A daemon cut off past
//! T may have a result refused, so property 4's "accepted once" does not apply to
//! [`Kind::Partition`] runs.

use std::collections::BTreeMap;
use std::fmt;

use kbf_meta::ActionAnswer;
use kbf_sim::{NodeId, Sim};
use kbf_types::FarmTime;

use crate::cell::{CellNode, Kind, Scenario, server_id, store_id};

/// A property of ha.md section 15.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    ReplicaEquality,
    OneRun,
    LeasesKept,
    ExactlyOneResult,
    NoFalsePresent,
    MonotoneTime,
    Liveness,
}

impl fmt::Display for Property {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::ReplicaEquality => "1 (replica equality)",
            Self::OneRun => "2 (one run)",
            Self::LeasesKept => "3 (leases kept)",
            Self::ExactlyOneResult => "4 (exactly one result)",
            Self::NoFalsePresent => "5 (no false present)",
            Self::MonotoneTime => "6 (monotone time)",
            Self::Liveness => "7 (liveness)",
        };
        f.write_str(name)
    }
}

/// The checks' memory across steps.
pub(crate) struct Checker<'a> {
    scenario: &'a Scenario,
    newest_time: FarmTime,
    present_checked: usize,
}

impl<'a> Checker<'a> {
    pub(crate) fn new(scenario: &'a Scenario) -> Self {
        Self {
            scenario,
            newest_time: FarmTime::default(),
            present_checked: 0,
        }
    }

    #[track_caller]
    fn fail(&self, at: FarmTime, property: Property, detail: &str) -> ! {
        let s = self.scenario;
        panic!(
            "{:?} seed {} at {} ms: property {property} violated: {detail}\nreplay: \
             KBF_SIM_KIND={:?} KBF_SIM_SEED={} cargo test -p kbf-sim-cell --test cell -- --ignored replay",
            s.kind,
            s.seed,
            at.as_millis(),
            s.kind,
            s.seed
        )
    }

    pub(crate) fn after_step(&mut self, sim: &Sim<CellNode>) {
        let now = sim.now();
        let server = server(sim);
        if let Err((property, detail)) = server.check_durable() {
            self.fail(now, property, &detail);
        }
        let meta = server.meta();
        if let Some(meta) = &meta {
            if meta.now() < self.newest_time {
                let d = format!("farm time {:?} after {:?}", meta.now(), self.newest_time);
                self.fail(now, Property::MonotoneTime, &d);
            }
            self.newest_time = meta.now();
        }
        let mut runs: BTreeMap<(u64, u64), Vec<&NodeId>> = BTreeMap::new();
        for (id, node) in sim.nodes() {
            if let CellNode::Daemon(d) = node {
                for (lease, operation) in d.unfenced_runs(now) {
                    runs.entry((lease.term, operation.0)).or_default().push(id);
                }
            }
        }
        if let Some((op, on)) = runs.iter().find(|(_, on)| on.len() > 1) {
            let d = format!("operation {op:?} runs unfenced on {on:?}");
            self.fail(now, Property::OneRun, &d);
        }
        let seen = &server.seen;
        let store = store(sim);
        for (at, digest) in &seen.present[self.present_checked..] {
            if !store.holds(digest) {
                let d = format!("{digest:?} answered present at {} ms", at.as_millis());
                self.fail(now, Property::NoFalsePresent, &d);
            }
        }
        self.present_checked = seen.present.len();
        if !matches!(self.scenario.kind, Kind::Calm | Kind::Partition) {
            return;
        }
        if let Some((at, lease, worker)) = seen.requeued_listed.first() {
            let d = format!(
                "lease {lease} of {worker} given up at {} ms",
                at.as_millis()
            );
            self.fail(now, Property::LeasesKept, &d);
        }
        if self.scenario.kind != Kind::Calm {
            return;
        }
        if let Some((lease, n)) = seen.accepted.iter().find(|(_, n)| **n > 1) {
            let d = format!("lease {lease}'s result accepted {n} times");
            self.fail(now, Property::ExactlyOneResult, &d);
        }
        if let Some(meta) = &meta {
            let missing = seen
                .won
                .iter()
                .find(|a| !matches!(meta.action(a), ActionAnswer::Hit { .. }));
            if let Some(action) = missing {
                let d = format!("{action:?} won a result but has no action-cache entry");
                self.fail(now, Property::ExactlyOneResult, &d);
            }
            if meta.action_count() != seen.won.len() {
                let d = format!(
                    "{} cache entries, {} won results",
                    meta.action_count(),
                    seen.won.len()
                );
                self.fail(now, Property::ExactlyOneResult, &d);
            }
        }
    }

    pub(crate) fn at_end(&self, sim: &Sim<CellNode>) {
        let now = sim.now();
        let server = server(sim);
        for (id, node) in sim.nodes() {
            match node {
                CellNode::Client(c) => {
                    let open: Vec<_> = c.jobs().iter().filter(|j| j.answered.is_none()).collect();
                    if !open.is_empty() {
                        let d = format!(
                            "{id} has {} actions unanswered: {:?}; server opens refused: {:?}",
                            open.len(),
                            open.first(),
                            server.seen.refused.first()
                        );
                        self.fail(now, Property::Liveness, &d);
                    }
                }
                CellNode::Daemon(d) if self.scenario.kind == Kind::Calm => {
                    let accepted = &server.seen.accepted;
                    let lost = d.reported().iter().find(|l| !accepted.contains_key(l));
                    if let Some(lease) = lost {
                        let detail = format!("{id} reported lease {lease}, never accepted");
                        self.fail(now, Property::ExactlyOneResult, &detail);
                    }
                }
                _ => {}
            }
        }
    }
}

fn server(sim: &Sim<CellNode>) -> &crate::Server {
    match sim.node(&server_id()) {
        Some(CellNode::Server(s)) => s,
        _ => unreachable!("the cell has a server"),
    }
}

fn store(sim: &Sim<CellNode>) -> &crate::ObjectStore {
    match sim.node(&store_id()) {
        Some(CellNode::Store(s)) => s,
        _ => unreachable!("the cell has an object store"),
    }
}
