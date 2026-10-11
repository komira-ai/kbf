//! A fake daemon: one process that never restarts, with the daemon's own rules for
//! contact and epochs and a fake run of each lease.
//!
//! Its contact with the server is a [`Contact`], the daemon's: a heartbeat every
//! interval the `Welcome` named, each recorded when sent and confirmed by its
//! acknowledgement; once contact is lost (T after the newest acknowledged send) every
//! run is killed, and once a gap is declared (two intervals without an
//! acknowledgement) the daemon opens a new stream. A `Welcome` drops the leases of an
//! earlier epoch by [`superseded`], the daemon's rule. A run ends after a few seconds
//! with a completed result; the result is kept until its `ResultAck`, listed in every
//! heartbeat and resent with each, and on every new stream. One run in ten is long. A `Start` is taken only on
//! the current stream, with contact confirmed, for a lease it does not hold.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kbf_daemon::{Contact, FENCE_AFTER, Moment, superseded};
use kbf_sim::{Chance, Event, NodeId, NodeInput, Output, SimRng};
use kbf_types::{Digest, Effect, FarmTime, LeaseId, OperationId, Outcome, Resources, StateMachine};

use crate::{Msg, input_of};

/// How often the daemon checks its runs and its contact.
const POLL: Duration = Duration::from_secs(1);
const T_POLL: u64 = 0;
const T_HEARTBEAT: u64 = 1;
/// How long a run takes: between these, in milliseconds; one in ten takes
/// [`LONG_RUN`], longer than the scheduler's grace G, so a daemon cut off mid-run
/// must fence it before its lease is granted again.
const RUN_MS: (u64, u64) = (1_000, 6_000);
const LONG_RUN: Duration = Duration::from_secs(80);
/// A `Hello` not answered within this is sent again on a new stream.
const HELLO_WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
struct Run {
    epoch: Option<u64>,
    operation: OperationId,
    action: Digest,
    ends: FarmTime,
}

/// The daemon node.
#[derive(Debug)]
pub struct FakeDaemon {
    server: NodeId,
    instance: String,
    capacity: Resources,
    contact: Contact,
    stream: u64,
    hello_sent: FarmTime,
    welcomed: bool,
    interval: Duration,
    seq: u64,
    epoch: Option<u64>,
    runs: BTreeMap<LeaseId, Run>,
    unacked: BTreeMap<LeaseId, (Digest, Outcome)>,
    /// Every lease this daemon has reported a result for.
    reported: BTreeSet<LeaseId>,
    out: Vec<Output<Msg>>,
}

fn moment(t: FarmTime) -> Moment {
    Moment::from_origin(Duration::from_millis(t.as_millis()))
}

impl FakeDaemon {
    /// A daemon of `capacity` that connects to `server`.
    #[must_use]
    pub fn new(server: NodeId, instance: String, capacity: Resources) -> Self {
        Self {
            server,
            instance,
            capacity,
            contact: Contact::new(FENCE_AFTER),
            stream: 0,
            hello_sent: FarmTime::default(),
            welcomed: false,
            interval: Duration::from_secs(5),
            seq: 0,
            epoch: None,
            runs: BTreeMap::new(),
            unacked: BTreeMap::new(),
            reported: BTreeSet::new(),
            out: Vec::new(),
        }
    }

    /// The runs going on at `now` and not fenced: the lease of each, and the term and
    /// operation it runs.
    pub fn unfenced_runs(&self, now: FarmTime) -> impl Iterator<Item = (LeaseId, OperationId)> {
        let fenced = self.contact.lost(moment(now));
        self.runs
            .iter()
            .filter(move |_| !fenced)
            .map(|(lease, run)| (*lease, run.operation))
    }

    /// Every lease this daemon has reported a result for.
    #[must_use]
    pub fn reported(&self) -> &BTreeSet<LeaseId> {
        &self.reported
    }

    pub(crate) fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }

    fn send(&mut self, msg: Msg) {
        let to = self.server.clone();
        self.out.push(Output::Send { to, msg });
    }

    fn connect(&mut self, now: FarmTime) {
        self.stream += 1;
        self.welcomed = false;
        self.hello_sent = now;
        self.send(Msg::Hello {
            stream: self.stream,
            instance: self.instance.clone(),
            capacity: self.capacity,
        });
    }

    fn resend_results(&mut self) {
        let results: Vec<_> = self.unacked.iter().map(|(l, r)| (*l, *r)).collect();
        for (lease, (action, outcome)) in results {
            let stream = self.stream;
            self.send(Msg::Report {
                stream,
                lease,
                action,
                outcome,
            });
        }
    }

    fn heartbeat(&mut self, now: FarmTime) {
        if !self.welcomed {
            return;
        }
        self.seq += 1;
        self.contact.sent(self.seq, moment(now));
        let running = self
            .runs
            .keys()
            .chain(self.unacked.keys())
            .copied()
            .collect();
        let (stream, seq) = (self.stream, self.seq);
        self.send(Msg::Heartbeat {
            stream,
            seq,
            running,
        });
        self.resend_results();
    }

    fn poll(&mut self, now: FarmTime) {
        if self.contact.lost(moment(now)) {
            // Fenced: every run is killed; its result is never sent.
            self.runs.clear();
        }
        let gap = self.contact.check_gap(moment(now)).is_some();
        let unanswered =
            !self.welcomed && now.saturating_duration_since(self.hello_sent) >= HELLO_WAIT;
        if gap || unanswered {
            self.connect(now);
        }
        let ended: Vec<LeaseId> = self
            .runs
            .iter()
            .filter(|(_, run)| run.ends <= now)
            .map(|(lease, _)| *lease)
            .collect();
        for lease in ended {
            let run = self.runs.remove(&lease).expect("listed above");
            let outcome = Outcome::Completed {
                action_result: input_of(&run.action),
            };
            self.unacked.insert(lease, (run.action, outcome));
            self.reported.insert(lease);
            let stream = self.stream;
            self.send(Msg::Report {
                stream,
                lease,
                action: run.action,
                outcome,
            });
        }
    }

    /// Takes the epoch a `Welcome` named, and drops each lease granted under another.
    fn new_epoch(&mut self, epoch: u64) {
        self.epoch = Some(epoch);
        let newest = self.epoch;
        self.runs.retain(|_, run| !superseded(run.epoch, newest));
    }

    fn receive(&mut self, now: FarmTime, msg: Msg, entropy: u64) {
        match msg {
            Msg::Welcome {
                stream,
                epoch,
                interval_ms,
            } if stream == self.stream && !self.welcomed => {
                self.welcomed = true;
                self.interval = Duration::from_millis(interval_ms);
                self.contact.new_stream(self.interval);
                self.contact.confirm(moment(self.hello_sent));
                self.new_epoch(epoch);
                self.resend_results();
            }
            Msg::HeartbeatAck { stream, seq } if stream == self.stream => {
                self.contact.acknowledged(seq);
            }
            Msg::Start {
                stream,
                lease,
                operation,
                action,
            } if stream == self.stream && self.welcomed => {
                let held = self.runs.contains_key(&lease) || self.unacked.contains_key(&lease);
                if held || self.contact.lost(moment(now)) || self.reported.contains(&lease) {
                    return;
                }
                let mut rng = SimRng::from_seed(entropy);
                let run = if rng.chance(Chance::percent(10)) {
                    LONG_RUN
                } else {
                    Duration::from_millis(rng.between(RUN_MS.0, RUN_MS.1))
                };
                let ends = now.saturating_add(run);
                let epoch = self.epoch;
                self.runs.insert(
                    lease,
                    Run {
                        epoch,
                        operation,
                        action,
                        ends,
                    },
                );
            }
            Msg::ResultAck { lease, .. } => {
                self.unacked.remove(&lease);
            }
            Msg::Cancel { stream, lease } if stream == self.stream => {
                self.runs.remove(&lease);
            }
            Msg::Reset { stream } if stream == self.stream => self.connect(now),
            _ => {}
        }
    }
}

impl StateMachine for FakeDaemon {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        let now = input.now;
        match input.event {
            Event::Start => {
                self.connect(now);
                self.out.push(Output::Timer {
                    after: POLL,
                    tag: T_POLL,
                });
                self.out.push(Output::Timer {
                    after: self.interval,
                    tag: T_HEARTBEAT,
                });
            }
            Event::Timer { tag: T_POLL } => {
                self.poll(now);
                self.out.push(Output::Timer {
                    after: POLL,
                    tag: T_POLL,
                });
            }
            Event::Timer { .. } => {
                self.heartbeat(now);
                self.out.push(Output::Timer {
                    after: self.interval,
                    tag: T_HEARTBEAT,
                });
            }
            Event::Message { msg, .. } => self.receive(now, msg, input.entropy),
        }
        Vec::new()
    }
}
