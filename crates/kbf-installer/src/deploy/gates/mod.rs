//! The deploy gates (docs/design/kbf-cicd.md, section 7; kbf#949 part 1). A deploy that
//! `--apply`s is judged by six gates besides its own steps, and the verdict is written as a
//! `kbf-gate` result document (`RUNNING` first, `FINISHED` on every exit this process can
//! see):
//!
//! - **A**, `a_inflight_ids`: the builds in flight are recorded at the start, at every node
//!   step and at the end, and once more as a `since`/`until` listing over the whole deploy, so
//!   a build that started and ended between two snapshots is still known. A listing that
//!   cannot be read is red.
//! - **B**, `b_relay_untraced`: `kbf_frontend_relay_untraced` is 0 at every scrape.
//! - **C**, `c_farm_class_failures`: after the deploy, every recorded build's actions are read
//!   and an attempt that ended `infra` or `lost_*` is red. The class filter is client-side:
//!   the API cannot filter by class.
//! - **D**, `d_lost_blobs` and `d_blob_losses_delta`: `kbf_cache_lost_blobs` is 0 at every
//!   scrape and `kbf_cache_blob_losses_total` has not grown since the first scrape.
//! - **E**, `e_under_replicated`: `kbf_under_replicated_blobs` is 0 before the deploy and
//!   returns to 0 within the settle window after each node step; the next node step does not
//!   begin before it does.
//! - **F**, `f_auto_revert`: no node and not the frontend reverted by its own timer.
//!
//! A measurement that is not there is red, never zero: a scrape that fails, a series that is
//! missing, a listing that is cut off. The first red ends the deploy (reason `gate_red`) and
//! names the gate, the value and the host step. Halting and walking back is the next change.
//!
//! The canned farm in the tests decides what each gate sees, so they prove the gate logic and
//! not that a dropped connection moves `kbf_frontend_relay_untraced`; that is the leased-node
//! acceptance run (kbf#949).

mod invocations;
mod scrape;

use std::collections::BTreeSet;

use kbf_gate::{GateResult, Limit, Sample, SetHash, Verdict, judge};

use std::collections::BTreeMap;

use super::nodes::Node;
use super::summary::{Exit, NodeResult};
use super::{Fail, Run, fail};

/// Gate names, as they appear in the result document.
pub(super) mod name {
    pub const INFLIGHT: &str = "a_inflight_ids";
    pub const RELAY_UNTRACED: &str = "b_relay_untraced";
    pub const FARM_FAILURES: &str = "c_farm_class_failures";
    pub const LOST_BLOBS: &str = "d_lost_blobs";
    pub const BLOB_LOSSES: &str = "d_blob_losses_delta";
    pub const UNDER_REPLICATED: &str = "e_under_replicated";
    pub const AUTO_REVERT: &str = "f_auto_revert";
    pub const ALL: [&str; 7] = [
        INFLIGHT,
        RELAY_UNTRACED,
        FARM_FAILURES,
        LOST_BLOBS,
        BLOB_LOSSES,
        UNDER_REPLICATED,
        AUTO_REVERT,
    ];
}

/// The series the gates read (the server's names).
mod series {
    pub const RELAY_UNTRACED: &str = "kbf_frontend_relay_untraced";
    pub const LOST_BLOBS: &str = "kbf_cache_lost_blobs";
    pub const BLOB_LOSSES: &str = "kbf_cache_blob_losses_total";
    pub const UNDER_REPLICATED: &str = "kbf_under_replicated_blobs";
}

/// What a gate allows. A is satisfied by any listing that was read; the rest allow none.
fn limit(gate: &str) -> Limit {
    if gate == name::INFLIGHT {
        Limit::AtLeast(0.0)
    } else {
        Limit::AtMost(0.0)
    }
}

struct Reading {
    gate: &'static str,
    sample: Sample,
    at: u64,
}

/// What the gates have seen so far in one deploy.
pub(crate) struct Gates {
    /// Gates run only for an applied deploy.
    pub active: bool,
    /// Set by the first scrape; the finishing reads run only once it is.
    pub begun: Option<u64>,
    /// The host step the next reading is for.
    pub step: String,
    readings: Vec<Reading>,
    baseline_losses: Option<f64>,
    ids: BTreeSet<String>,
    /// The hash of the release set, when preflight derived it.
    pub derived_set_hash: Option<SetHash>,
    /// Which frontend each reading came from: `<step>: <which>` each time it changes.
    pub observed: Vec<String>,
    last_observed: String,
}

impl Gates {
    pub fn new(active: bool) -> Gates {
        Gates {
            active,
            begun: None,
            step: String::new(),
            readings: Vec::new(),
            baseline_losses: None,
            ids: BTreeSet::new(),
            derived_set_hash: None,
            observed: Vec::new(),
            last_observed: String::new(),
        }
    }

    /// Note that the next reading comes from `which`, in the current step.
    pub fn saw(&mut self, which: &str) {
        if self.last_observed != which {
            which.clone_into(&mut self.last_observed);
            self.observed.push(format!("{}: {which}", self.step));
        }
    }

    /// Keep a reading and judge it: `Err` names the gate, the value and the host step.
    pub fn record(&mut self, gate: &'static str, sample: Sample, at: u64) -> Result<(), String> {
        self.record_with(gate, sample, at, "")
    }

    /// [`Gates::record`], with `extra` appended to the red message.
    pub fn record_with(
        &mut self,
        gate: &'static str,
        sample: Sample,
        at: u64,
        extra: &str,
    ) -> Result<(), String> {
        self.readings.push(Reading { gate, sample, at });
        if judge(sample, limit(gate)) != Verdict::Red {
            return Ok(());
        }
        let (value, bound) = match (sample, limit(gate)) {
            (Sample::Value(v), Limit::AtMost(m)) => (format!("{v}"), format!("at most {m}")),
            (Sample::Value(v), Limit::AtLeast(m)) => (format!("{v}"), format!("at least {m}")),
            (Sample::Value(v), Limit::Band { red, .. }) => {
                (format!("{v}"), format!("at most {red}"))
            }
            (Sample::Absent, _) => (
                "absent (no such series, or it could not be read)".to_owned(),
                "a measurement".to_owned(),
            ),
        };
        Err(format!(
            "gate {gate}: {value}, wanted {bound}, at step {}{extra}",
            self.step
        ))
    }

    /// The result of every gate, worst reading first: an absent reading beats any value, and
    /// a gate that was never read is absent too.
    pub fn results(&self, now: u64) -> Vec<GateResult> {
        name::ALL
            .iter()
            .map(|&gate| {
                let own: Vec<&Reading> = self.readings.iter().filter(|r| r.gate == gate).collect();
                let sample = if own.iter().any(|r| r.sample == Sample::Absent) || own.is_empty() {
                    Sample::Absent
                } else {
                    Sample::Value(
                        own.iter()
                            .filter_map(|r| match r.sample {
                                Sample::Value(v) => Some(v),
                                Sample::Absent => None,
                            })
                            .fold(f64::MIN, f64::max),
                    )
                };
                let started = own.first().map_or(now, |r| r.at);
                let finished = own.last().map_or(now, |r| r.at);
                GateResult::decide(gate, sample, limit(gate), started, finished)
            })
            .collect()
    }
}

fn gate_fail(exit: Exit, detail: String) -> Fail {
    fail(exit, "gate_red", detail)
}

impl Run<'_> {
    /// Find the frontend the gates read now: the slot the update API reports as serving
    /// (`GET /v1/update/frontend`, `active_slot`, present while an update is known), else the
    /// only child that answers. Asked at the start and after every frontend step, since a
    /// handover retires the child that was read.
    pub(super) fn resolve_observed(&mut self, step: &str, exit: Exit) -> Result<(), Fail> {
        if !self.gates.active {
            return Ok(());
        }
        let serving = self.get_json("/v1/update/frontend").ok().and_then(|v| {
            v.get("active_slot")
                .and_then(serde_json::Value::as_str)
                .and_then(super::observe::Slot::parse)
        });
        match self.env.observe.resolve(serving) {
            Ok(found) => {
                self.observed = Some(found);
                Ok(())
            }
            Err(e) => Err(fail(
                exit,
                "observe_unresolved",
                format!("step {step}: cannot find the frontend to observe: {e}"),
            )),
        }
    }

    /// Name the host step the next readings are for.
    pub(super) fn gate_step(&mut self, step: impl Into<String>) {
        self.gates.step = step.into();
    }

    /// Scrape `/metrics` and check B and D (and keep the first scrape's loss counter as the
    /// baseline). Returns the under-replicated reading for the caller to judge, since what
    /// it must be depends on the step. An unreadable scrape reads every series as absent.
    pub(super) fn gate_scrape(&mut self, exit: Exit) -> Result<Sample, Fail> {
        if !self.gates.active {
            return Ok(Sample::Value(0.0));
        }
        let now = self.env.clock.now();
        let which = self.observed.as_ref().map(|o| o.label.clone());
        self.gates.saw(which.as_deref().unwrap_or("none resolved"));
        let metrics = scrape::read(self);
        let get = |series: &str| match &metrics {
            Ok(m) => m.sum(series),
            Err(_) => Sample::Absent,
        };
        let losses = get(series::BLOB_LOSSES);
        let first = self.gates.baseline_losses.is_none() && self.gates.begun.is_none();
        if first {
            self.gates.begun = Some(now);
            self.gates.baseline_losses = match losses {
                Sample::Value(v) => Some(v),
                Sample::Absent => None,
            };
        }
        // A counter that went backwards was reset (a restart): the loss count across the
        // deploy cannot be told, which is not a pass.
        let delta = match (losses, self.gates.baseline_losses) {
            (Sample::Value(now), Some(base)) if now >= base => Sample::Value(now - base),
            _ => Sample::Absent,
        };
        let results = [
            self.gates
                .record(name::RELAY_UNTRACED, get(series::RELAY_UNTRACED), now),
            self.gates
                .record(name::LOST_BLOBS, get(series::LOST_BLOBS), now),
            self.gates.record(name::BLOB_LOSSES, delta, now),
        ];
        let unreadable = metrics.as_ref().err().cloned();
        if let Some(first_red) = results.into_iter().find_map(Result::err) {
            let why = unreadable.map_or(String::new(), |e| format!(" ({e})"));
            return Err(gate_fail(exit, format!("{first_red}{why}")));
        }
        Ok(get(series::UNDER_REPLICATED))
    }

    /// Record E for `sample` and fail if it is not 0.
    pub(super) fn gate_under_replicated(&mut self, sample: Sample, exit: Exit) -> Result<(), Fail> {
        let now = self.env.clock.now();
        self.gates
            .record(name::UNDER_REPLICATED, sample, now)
            .map_err(|e| gate_fail(exit, e))
    }

    /// E before anything is changed: storage must be known to be fully replicated. A farm that
    /// reports no such series at all (no storage members, or none that has reported lately)
    /// can never pass the gate, so it is refused here with that said, not found red mid-deploy.
    /// It stays red, not waived.
    pub(super) fn gate_preflight_storage(&mut self, under: Sample) -> Result<(), Fail> {
        if self.gates.active && under == Sample::Absent {
            let now = self.env.clock.now();
            let _ = self.gates.record(name::UNDER_REPLICATED, under, now);
            return Err(fail(
                Exit::Refused,
                "gate_unjudgeable",
                format!(
                    "gate {}: this farm reports no under-replicated series, so the gate cannot \
                     be judged (it needs storage members that have reported); at step preflight",
                    name::UNDER_REPLICATED
                ),
            ));
        }
        self.gate_under_replicated(under, Exit::Refused)
    }

    /// Record F: `reverted` is what fired (a node, or the frontend), else a clean look.
    pub(super) fn gate_revert(&mut self, reverted: Option<&str>) {
        if !self.gates.active {
            return;
        }
        let now = self.env.clock.now();
        if let Some(who) = reverted {
            self.gates.step = format!("{} ({who} reverted)", self.gates.step);
        }
        let value = if reverted.is_some() { 1.0 } else { 0.0 };
        let _ = self
            .gates
            .record(name::AUTO_REVERT, Sample::Value(value), now);
    }

    /// A: the ids of the builds in flight now.
    pub(super) fn gate_snapshot(&mut self, exit: Exit) -> Result<(), Fail> {
        if !self.gates.active {
            return Ok(());
        }
        let found = invocations::list(self, "state=open");
        self.take_ids(found, exit)
    }

    /// A step boundary: B and D at a scrape, and the builds in flight.
    pub(super) fn gate_checkpoint(&mut self, step: &str) -> Result<(), Fail> {
        self.gate_step(step);
        self.gate_scrape(Exit::Failed)?;
        self.gate_snapshot(Exit::Failed)
    }

    /// The gates at one look at the nodes: B and D at a scrape, E for the node whose repair is
    /// awaited (`repair`: its id and when it confirmed), and the builds in flight whenever a
    /// node has just been seen to confirm (`stepped`). Returns the under-replicated reading.
    pub(super) fn gate_nodes_tick(
        &mut self,
        list: Option<&[Node]>,
        done: &BTreeMap<String, NodeResult>,
        repair: &mut Option<(String, u64)>,
        stepped: bool,
    ) -> Result<Sample, Fail> {
        if !self.gates.active {
            return Ok(Sample::Value(0.0));
        }
        let list = list.unwrap_or_default();
        let waiting = list.iter().find(|n| !done.contains_key(&n.id));
        self.gate_step(match waiting {
            Some(n) => format!("nodes: waiting for {}", n.id),
            None => "nodes: settling".to_owned(),
        });
        let under = self.gate_scrape(Exit::Failed)?;
        if stepped {
            self.gate_snapshot(Exit::Failed)?;
        }
        let now = self.env.clock.now();
        let Some((id, since)) = repair.clone() else {
            return Ok(under);
        };
        if under == Sample::Value(0.0) {
            *repair = None;
            return self
                .gate_under_replicated(under, Exit::Failed)
                .map(|()| under);
        }
        // Not repaired. The next node's step must not begin, and the window is finite.
        let began = list.iter().find(|n| {
            n.id != id && !done.contains_key(&n.id) && n.state == "activated_unconfirmed"
        });
        let why = match began {
            Some(n) => format!("nodes: {} began its step before {id} was repaired", n.id),
            None if now.saturating_sub(since) >= self.opts.settle.as_secs() => format!(
                "nodes: {id} not repaired within the {}s settle window",
                self.opts.settle.as_secs()
            ),
            None => return Ok(under),
        };
        self.gate_step(why.clone());
        self.gate_under_replicated(under, Exit::Failed)?;
        // Only a reading below 0 passes the call above, and no gauge reads below 0.
        Err(gate_fail(
            Exit::Failed,
            format!("gate {}: {why}", name::UNDER_REPLICATED),
        ))
    }

    /// A node confirmed while the one before it is still unrepaired: it moved between two
    /// looks, so the next step had begun before the repair.
    pub(super) fn gate_confirmed_unrepaired(
        &mut self,
        node: &str,
        before: &str,
        under: Sample,
    ) -> Fail {
        self.gate_step(format!(
            "nodes: {node} confirmed before {before} was repaired"
        ));
        match self.gate_under_replicated(under, Exit::Failed) {
            Err(f) => f,
            Ok(()) => gate_fail(
                Exit::Failed,
                format!(
                    "gate {}: {node} confirmed before {before} was repaired",
                    name::UNDER_REPLICATED
                ),
            ),
        }
    }

    fn take_ids(&mut self, found: Result<Vec<String>, String>, exit: Exit) -> Result<(), Fail> {
        let now = self.env.clock.now();
        let sample = match found {
            Ok(ids) => {
                self.gates.ids.extend(ids);
                Sample::Value(self.gates.ids.len() as f64)
            }
            Err(_) => Sample::Absent,
        };
        self.gates
            .record(name::INFLIGHT, sample, now)
            .map_err(|e| gate_fail(exit, e))
    }

    /// The end of an applied deploy: A's listing over the whole window, then C over every
    /// recorded id. Both run after a failed deploy too, so the document is complete.
    pub(super) fn gate_finish(&mut self) -> Result<(), Fail> {
        let Some(begun) = self.gates.begun.filter(|_| self.gates.active) else {
            return Ok(());
        };
        self.gate_step("end");
        // The window is cut at whole seconds: round the end up so a build seen in the last
        // second is not outside it.
        let query = format!(
            "since={}&until={}",
            super::summary::utc(begun),
            super::summary::utc(self.env.clock.now() + 1)
        );
        let found = invocations::list(self, &query);
        let window = self.take_ids(found, Exit::Failed);
        let now = self.env.clock.now();
        let mut failures = Some(Vec::new());
        let mut unreadable = String::new();
        for id in self.gates.ids.clone() {
            match invocations::farm_class_failures(self, &id) {
                Ok(more) => failures
                    .as_mut()
                    .into_iter()
                    .for_each(|f| f.extend(more.clone())),
                Err(e) => {
                    failures = None;
                    unreadable = format!("; {e}");
                }
            }
        }
        let sample = failures
            .as_ref()
            .map_or(Sample::Absent, |f| Sample::Value(f.len() as f64));
        let extra = match &failures {
            Some(f) if !f.is_empty() => format!("; {}", f.join(", ")),
            _ => unreadable,
        };
        let classes = self
            .gates
            .record_with(name::FARM_FAILURES, sample, now, &extra)
            .map_err(|e| gate_fail(Exit::Failed, e));
        window.and(classes)
    }
}
