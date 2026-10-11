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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use kbf_meta::{ActionAnswer, MetaState};
use kbf_sim::{NodeId, Sim};
use kbf_types::{Digest, FarmTime, LeaseId, OperationId};

use crate::cell::{CellNode, Kind, Scenario};
use crate::client::Job;
use crate::replica::{self, Broken};
use crate::server::Seen;

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

/// `Ok`, or `property` broken as `detail` says.
fn hold_if(detail: Option<String>, property: Property) -> Result<(), Broken> {
    detail.map_or(Ok(()), |d| Err((property, d)))
}

/// Property 2 over every unfenced run: (lease, operation, node).
fn one_run<'a>(
    runs: impl Iterator<Item = (LeaseId, OperationId, &'a NodeId)>,
) -> Result<(), Broken> {
    let mut by_operation: BTreeMap<(u64, u64), Vec<&NodeId>> = BTreeMap::new();
    for (lease, operation, node) in runs {
        by_operation
            .entry((lease.term, operation.0))
            .or_default()
            .push(node);
    }
    let twice = by_operation.iter().find(|(_, on)| on.len() > 1);
    let detail = twice.map(|(op, on)| format!("operation {op:?} runs unfenced on {on:?}"));
    hold_if(detail, Property::OneRun)
}

/// Property 5 over the blobs answered present since the last check.
fn no_false_present(
    present: &[(FarmTime, Digest)],
    holds: impl Fn(&Digest) -> bool,
) -> Result<(), Broken> {
    let lacking = present.iter().find(|(_, d)| !holds(d));
    let detail = lacking.map(|(at, d)| format!("{d:?} answered present at {} ms", at.as_millis()));
    hold_if(detail, Property::NoFalsePresent)
}

/// Property 3.
fn leases_kept(seen: &Seen) -> Result<(), Broken> {
    let listed = seen.requeues.iter().find(|r| r.3);
    let detail = listed.map(|(at, lease, worker, _)| {
        format!(
            "lease {lease} of {worker} given up at {} ms while listed",
            at.as_millis()
        )
    });
    hold_if(detail, Property::LeasesKept)
}

/// Property 4, after a step: no result accepted twice, and an action-cache entry
/// exactly for each action with an accepted result.
fn one_result(seen: &Seen, meta: &MetaState) -> Result<(), Broken> {
    let twice = seen.accepted.iter().find(|(_, n)| **n > 1);
    let uncached = seen
        .won
        .iter()
        .find(|a| !matches!(meta.action(a), ActionAnswer::Hit { .. }));
    let entries = meta.action_count();
    let detail = match (twice, uncached) {
        (Some((lease, n)), _) => Some(format!("lease {lease}'s result accepted {n} times")),
        (None, Some(a)) => Some(format!("{a:?} won a result but has no action-cache entry")),
        (None, None) => (entries != seen.won.len()).then(|| {
            format!(
                "{entries} action-cache entries for {} actions with a result",
                seen.won.len()
            )
        }),
    };
    hold_if(detail, Property::ExactlyOneResult)
}

/// Property 4, at the end: every lease a daemon reported was accepted.
fn all_accepted(
    reported: &BTreeSet<LeaseId>,
    accepted: &BTreeMap<LeaseId, u32>,
) -> Result<(), Broken> {
    let lost = reported
        .iter()
        .find(|l| accepted.get(l).is_none_or(|n| *n == 0));
    let detail = lost.map(|lease| format!("lease {lease} reported, never accepted"));
    hold_if(detail, Property::ExactlyOneResult)
}

/// Property 7.
fn liveness(
    node: &NodeId,
    jobs: &[Job],
    refused: Option<&(FarmTime, String)>,
) -> Result<(), Broken> {
    let open: Vec<&Job> = jobs.iter().filter(|j| j.answered.is_none()).collect();
    let detail = open.first().map(|first| {
        format!(
            "{node} has {} actions unanswered, first {first:?}; first refused open: {refused:?}",
            open.len()
        )
    });
    hold_if(detail, Property::Liveness)
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

    /// Panics, naming the run and the replay command, if `result` is a broken property.
    #[track_caller]
    fn hold(&self, at: FarmTime, result: Result<(), Broken>) {
        if let Err((property, detail)) = result {
            let s = self.scenario;
            panic!(
                "{:?} seed {} at {} ms: property {property} violated: {detail}\nreplay: \
                 KBF_SIM_KIND={:?} KBF_SIM_SEED={} cargo test -p kbf-sim-cell --test cell -- --ignored replay",
                s.kind,
                s.seed,
                at.as_millis(),
                s.kind,
                s.seed
            );
        }
    }

    pub(crate) fn after_step(&mut self, sim: &Sim<CellNode>) {
        let now = sim.now();
        let server = server(sim);
        self.hold(now, server.check_durable());
        let meta = server.meta();
        if let Some(meta) = &meta {
            self.hold(now, replica::monotone(self.newest_time, meta.now()));
            self.newest_time = meta.now();
        }
        let runs = sim
            .nodes()
            .filter_map(|(id, n)| n.daemon().map(|d| (id, d)))
            .flat_map(|(id, d)| d.unfenced_runs(now).map(move |(l, o)| (l, o, id)));
        self.hold(now, one_run(runs));
        let seen = &server.seen;
        let store = sim
            .nodes()
            .find_map(|(_, n)| n.store())
            .expect("the cell has an object store");
        let present = &seen.present[self.present_checked..];
        self.hold(now, no_false_present(present, |d| store.holds(d)));
        self.present_checked = seen.present.len();
        if matches!(self.scenario.kind, Kind::Calm | Kind::Partition) {
            self.hold(now, leases_kept(seen));
        }
        if let (Kind::Calm, Some(meta)) = (self.scenario.kind, &meta) {
            self.hold(now, one_result(seen, meta));
        }
    }

    pub(crate) fn at_end(&self, sim: &Sim<CellNode>) {
        let now = sim.now();
        let seen = &server(sim).seen;
        for (id, node) in sim.nodes() {
            if let Some(client) = node.client() {
                self.hold(now, liveness(id, client.jobs(), seen.refused.first()));
            }
            if let (Kind::Calm, Some(daemon)) = (self.scenario.kind, node.daemon()) {
                self.hold(now, all_accepted(daemon.reported(), &seen.accepted));
            }
        }
    }
}

/// The cell's server.
pub(crate) fn server(sim: &Sim<CellNode>) -> &crate::Server {
    sim.nodes()
        .find_map(|(_, n)| n.server())
        .expect("the cell has a server")
}

#[cfg(test)]
mod tests {
    use kbf_meta::{
        ActionRecord, Applied, Closure, Command, Location, ObjectId, Retention, Role, StoreId,
    };
    use kbf_types::{DigestFunction, WorkerId};

    use super::*;
    use crate::{FakeClient, action, input_of};

    fn lease(seq: u64) -> LeaseId {
        LeaseId::new(1, seq)
    }

    fn property(r: Result<(), Broken>) -> Property {
        r.expect_err("broken").0
    }

    /// Catches a one-run check that misses two runs of one operation (on one node or
    /// two), or that takes runs of two terms' operations of one id for one operation.
    #[test]
    fn two_runs_of_one_operation_break_one_run() {
        let (a, b) = (NodeId::from("daemon-0"), NodeId::from("daemon-1"));
        let apart = [
            (lease(1), OperationId(0), &a),
            (lease(2), OperationId(1), &b),
        ];
        assert!(one_run(apart.into_iter()).is_ok());
        let terms = [
            (lease(1), OperationId(0), &a),
            (LeaseId::new(2, 1), OperationId(0), &b),
        ];
        assert!(one_run(terms.into_iter()).is_ok());
        let twice = [
            (lease(1), OperationId(0), &a),
            (lease(2), OperationId(0), &a),
        ];
        assert_eq!(property(one_run(twice.into_iter())), Property::OneRun);
    }

    /// Catches a no-false-present check that never asks the store.
    #[test]
    fn a_present_blob_the_store_lacks_breaks_no_false_present() {
        let d = Digest::new(DigestFunction::Sha256, [3; 32], 3);
        let present = [(FarmTime::from_millis(9), d)];
        assert!(no_false_present(&present, |_| true).is_ok());
        assert_eq!(
            property(no_false_present(&present, |_| false)),
            Property::NoFalsePresent
        );
    }

    /// Catches a leases-kept check that fails an unlisted requeue (a lost `Start` is
    /// rightly given up), or passes a listed one.
    #[test]
    fn a_listed_lease_given_up_breaks_leases_kept() {
        let mut seen = Seen::default();
        let worker = WorkerId::new("daemon-0");
        seen.requeues
            .push((FarmTime::default(), lease(1), worker.clone(), false));
        assert!(leases_kept(&seen).is_ok());
        seen.requeues
            .push((FarmTime::default(), lease(2), worker, true));
        assert_eq!(property(leases_kept(&seen)), Property::LeasesKept);
    }

    /// Catches each half of property 4 going unchecked: a result accepted twice, a
    /// won result without its cache entry, a reported lease refused or never seen, a
    /// cache entry without a won result.
    #[test]
    fn each_break_of_one_result_is_reported() {
        let meta = MetaState::new(Retention::default());
        let mut seen = Seen::default();
        assert!(one_result(&seen, &meta).is_ok());
        seen.accepted.insert(lease(1), 2);
        assert_eq!(
            property(one_result(&seen, &meta)),
            Property::ExactlyOneResult
        );
        seen.accepted.insert(lease(1), 1);
        seen.won.insert(action(0));
        assert_eq!(
            property(one_result(&seen, &meta)),
            Property::ExactlyOneResult
        );
        let reported = BTreeSet::from([lease(1), lease(2)]);
        let lost = all_accepted(&reported, &seen.accepted);
        assert_eq!(property(lost), Property::ExactlyOneResult);
        seen.accepted.insert(lease(2), 0);
        let refused = all_accepted(&reported, &seen.accepted);
        assert_eq!(property(refused), Property::ExactlyOneResult);
        seen.accepted.insert(lease(2), 1);
        assert!(all_accepted(&reported, &seen.accepted).is_ok());
        // An action-cache entry for an action with no accepted result.
        let mut cached = MetaState::new(Retention::default());
        let Applied::Epoch(epoch) = cached.execute(1, Command::AllocEpoch) else {
            panic!("an epoch")
        };
        let result = input_of(&action(0));
        let location = Location {
            store: StoreId::new(0),
            object: ObjectId::new(epoch, 0),
            offset: 0,
        };
        cached.execute(
            2,
            Command::PutBlob {
                digest: result,
                location,
            },
        );
        let record = ActionRecord {
            result,
            closure: Closure::new(),
        };
        cached.execute(
            3,
            Command::PutAction {
                role: Role::Daemon,
                action: action(0),
                record,
            },
        );
        assert_eq!(
            property(one_result(&Seen::default(), &cached)),
            Property::ExactlyOneResult
        );
    }

    /// Catches a liveness check that passes an unanswered action.
    #[test]
    fn an_unanswered_action_breaks_liveness() {
        let client = FakeClient::new(NodeId::from("server"), [(action(0), FarmTime::default())]);
        let node = NodeId::from("client-0");
        assert_eq!(
            property(liveness(&node, client.jobs(), None)),
            Property::Liveness
        );
    }

    /// Catches a checker that does not stop the run, or whose message lacks the seed,
    /// the property or the replay command.
    #[test]
    #[should_panic(
        expected = "Calm seed 5 at 7 ms: property 2 (one run) violated: x\nreplay: KBF_SIM_KIND=Calm KBF_SIM_SEED=5"
    )]
    fn a_broken_property_panics_with_its_replay_line() {
        let scenario = Scenario::new(Kind::Calm, 5);
        let checker = Checker::new(&scenario);
        checker.hold(
            FarmTime::from_millis(7),
            Err((Property::OneRun, "x".to_owned())),
        );
    }

    /// Catches a renumbered property: failure lines and the docs name them so.
    #[test]
    fn each_property_prints_its_number() {
        let all = [
            Property::ReplicaEquality,
            Property::OneRun,
            Property::LeasesKept,
            Property::ExactlyOneResult,
            Property::NoFalsePresent,
            Property::MonotoneTime,
            Property::Liveness,
        ];
        let names: Vec<String> = all.iter().map(ToString::to_string).collect();
        let want = [
            "1 (replica equality)",
            "2 (one run)",
            "3 (leases kept)",
            "4 (exactly one result)",
            "5 (no false present)",
            "6 (monotone time)",
            "7 (liveness)",
        ];
        assert_eq!(names, want);
    }
}
