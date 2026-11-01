//! A scripted session over the public API of the real [`Farm`], and the text it writes.
//!
//! Every call the script makes is a line that starts with `>` and the time the script has
//! moved the clock on by so far, in milliseconds (the clock is tokio's, paused: it moves
//! when the script advances it, and by the timers the farm itself waits on, which are
//! the same at every run). What the call led to follows, indented, in a fixed order within
//! the call:
//!
//! 1. `txn` and the rows under it: what the farm asked the state store to write, in order;
//! 2. `cache`: an action-cache entry that is there now and was not before;
//! 3. `msg`: each message the farm sent to a node, node by node, oldest first;
//! 4. `stage`: each caller whose stage changed, in the order the callers asked.
//!
//! The order inside a call between these four is a convention of the transcript
//! (`transcript.rs`), not something the public API shows; the order within each is the
//! farm's. The text is built by hand from the fields, never from `Debug`, so a field that
//! is added to a message is added to the text on purpose (`text.rs` writes the words).

use std::sync::Arc;
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_front::{Described, Dispatch, Submission};
use kbf_proto::worker::{self, ServerMessage, server_message};
use kbf_sched::{DaemonInstance, NodePhase, Request};
use kbf_server::farm::{NodeAction, Reported, StreamId, TestCore};
use kbf_server::{EpochPolicy, Farm, StateStore};
use kbf_state::Store;
use kbf_types::{ActionKey, LeaseId, Resources, WorkerId};
use tokio::sync::mpsc;

use crate::tape::as_written;
pub use crate::text::{Ended, Job};
use crate::text::{
    GIB, ended_text, lease_list, lease_text, phase_text, result_of, wire_lease_text,
};
use crate::transcript::{Node, Transcript};

/// The term a new store is seeded with.
pub const TERM: u64 = 7;
/// The wall clock when the farm is built, in milliseconds since the Unix epoch.
const WALL_MS: u64 = 1_900_000_000_000;

/// The farm, the store under it, and the text of everything the script did.
pub struct Session {
    farm: Option<Farm>,
    t: Transcript<StreamId>,
}

impl Session {
    /// A farm over an empty store, as a first start (the store's epoch is the seed).
    pub async fn start(title: &str) -> Self {
        let mut session = Self {
            farm: None,
            t: Transcript::new(title, as_written),
        };
        session.farm = Some(session.build(EpochPolicy::Keep));
        let farm = session.farm();
        let note = format!(
            "farm term={} lease_epoch={}",
            farm.term(),
            farm.lease_epoch()
        );
        session.t.say("start");
        session.t.note(note);
        session.t.sync().await;
        session
    }

    /// The text, one line per line, ending in a newline.
    pub fn text(&self) -> String {
        self.t.text()
    }

    fn build(&self, policy: EpochPolicy) -> Farm {
        let core = TestCore::new(
            TERM,
            WALL_MS,
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        );
        let store: Arc<dyn Store> = self.t.tape.clone();
        Farm::for_test(self.t.cache.clone(), core)
            .with_state(Some(StateStore {
                store,
                epoch: policy,
                members: Default::default(),
                history: None,
            }))
            .expect("a farm over the store")
    }

    fn farm(&self) -> &Farm {
        self.farm.as_ref().expect("the process is running")
    }

    /// Registers `name` with `cores` cores and `gib` GiB, running `drivers`.
    pub async fn register(&mut self, name: &str, cores: u64, gib: u64, drivers: &[&str]) {
        self.t.say(&format!(
            "register {name} cores={cores} gib={gib} drivers={}",
            drivers.join(",")
        ));
        let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .expect("caps")
            .with_drivers(drivers.iter().copied());
        let reported = Reported::serving(Resources::new(cores * 1_000, gib * GIB), caps);
        let (outbound, inbox) = mpsc::unbounded_channel();
        let welcome = ServerMessage {
            message: Some(server_message::Message::Welcome(worker::Welcome {
                protocol_version: 1,
                heartbeat_interval_ms: 5_000,
                epoch: self.farm().lease_epoch(),
                ..Default::default()
            })),
        };
        let stream = self.farm().register(
            &WorkerId::new(name),
            DaemonInstance::new(format!("daemon-{name}")),
            DaemonInstance::new(""),
            reported,
            outbound,
            welcome,
        );
        let drivers = drivers.iter().map(|d| (*d).to_owned()).collect();
        let node = Node {
            stream,
            inbox,
            beat: 0,
            drivers,
        };
        self.t.nodes.insert(name.to_owned(), node);
        self.t.sync().await;
    }

    /// The node's daemon resends its `Hello` with another phase.
    pub async fn phase(&mut self, name: &str, cores: u64, gib: u64, phase: NodePhase) {
        self.t
            .say(&format!("hello again {name} phase={}", phase_text(&phase)));
        let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .expect("caps")
            .with_drivers(self.t.nodes[name].drivers.iter().map(String::as_str));
        let reported = Reported {
            capacity: Resources::new(cores * 1_000, gib * GIB),
            caps,
            phase,
            protocol: None,
        };
        let stream = self.t.nodes[name].stream;
        self.farm().resize(&WorkerId::new(name), stream, reported);
        self.t.sync().await;
    }

    /// The node's stream ends: what the farm sent it is dropped from then on.
    pub async fn disconnect(&mut self, name: &str) {
        self.t.say(&format!("disconnect {name}"));
        self.t.nodes.remove(name);
        self.t.sync().await;
    }

    /// Submits `job` as one more caller.
    pub async fn submit(&mut self, job: &Job) {
        let n = self.t.submitted(job);
        self.t.say(&format!(
            "submit {} qos={} kind={} cores_milli={} mem_gib={} do_not_cache={}",
            job.label,
            job.qos.name(),
            job.kind,
            job.resources.cpu_millis,
            job.resources.memory_bytes / GIB,
            job.do_not_cache
        ));
        let request = Request {
            key: ActionKey {
                instance: "main".to_owned(),
                action: job.digest(),
            }
            .into(),
            qos: job.qos.clone(),
            kind: job.kind,
            resources: job.resources,
            hermetic: true,
            do_not_cache: job.do_not_cache,
            whole_worker: false,
            needs: kbf_caps::Request::default(),
        };
        let described = Described {
            arguments: vec!["/bin/run".to_owned(), job.label.to_owned()],
            outputs: vec![format!("out/{}", job.label)],
            label: Default::default(),
        };
        let submitted = self.farm().submit(Submission {
            request,
            call: None,
            described,
            evidence: Default::default(),
            max_wall: None,
        });
        match submitted {
            Ok(ticket) => {
                let label = format!("{}#{n}", job.label);
                self.t.note(format!("ticket {label} name={}", ticket.name));
                self.t.names.insert(label.clone(), ticket.name.clone());
                self.t.follow(label, ticket.stage);
            }
            Err(status) => self.t.note(format!(
                "refused code={} message={:?}",
                status.code() as i32,
                status.message()
            )),
        }
        self.t.sync().await;
    }

    /// Asks again for the operation the caller `of` was given, as WaitExecution does
    /// after a restart, and follows it as `label`.
    pub async fn wait(&mut self, label: &str, of: &str) {
        let name = self.t.names[of].clone();
        self.t.say(&format!("wait {name} as {label}"));
        match self.farm().wait(&name) {
            Some(ticket) => self.t.follow(label.to_owned(), ticket.stage),
            None => self.t.note("no such operation".to_owned()),
        }
        self.t.sync().await;
    }

    /// The node's heartbeat, naming the leases it still runs.
    pub async fn heartbeat(&mut self, name: &str, running: &[LeaseId]) {
        let leases = running.to_vec();
        let node = self.t.nodes.get_mut(name).expect("a connected node");
        node.beat += 1;
        let (stream, seq) = (node.stream, node.beat);
        self.t.say(&format!(
            "heartbeat {name} seq={seq} running=[{}]",
            lease_list(&leases)
        ));
        let acked = self
            .farm()
            .heartbeat(&WorkerId::new(name), stream, seq, leases)
            .await;
        self.t.note(format!("acknowledged={acked}"));
        self.t.sync().await;
    }

    /// The node reports the end of the lease it ran `label` under.
    pub async fn report(&mut self, name: &str, label: &'static str, ended: Ended) {
        let lease = self.lease_of(label);
        self.report_lease(name, label, lease, ended).await;
    }

    /// The node reports the end of `lease`, which ran `label` (an old lease, for a late
    /// report).
    pub async fn report_lease(
        &mut self,
        name: &str,
        label: &'static str,
        lease: LeaseId,
        ended: Ended,
    ) {
        self.t.say(&format!(
            "result {name} lease={} action={label} ended={}",
            lease_text(lease),
            ended_text(ended)
        ));
        let digest = self.t.digest_of_start(label, lease);
        let ack = self
            .farm()
            .report(&WorkerId::new(name), result_of(lease, digest, label, ended))
            .await;
        match ack {
            Some(ack) => self.t.note(format!(
                "ack lease={} accepted={} reason={}",
                wire_lease_text(ack.lease_id.as_ref()),
                ack.accepted,
                worker::ResultAckReason::try_from(ack.reason).map_or("?", |r| r.as_str_name())
            )),
            None => self.t.note("no ack".to_owned()),
        }
        self.t.sync().await;
    }

    /// The farm's tick.
    pub async fn tick(&mut self) {
        self.t.say("tick");
        self.farm().tick().await;
        self.t.sync().await;
    }

    /// Time passes on tokio's clock, the store's and so the wall clock.
    pub async fn advance(&mut self, by: Duration) {
        self.t.t_ms += u64::try_from(by.as_millis()).expect("a short span");
        tokio::time::advance(by).await;
        self.t.tape.advance(by);
        self.t.say(&format!("advance {}ms", by.as_millis()));
        self.t.sync().await;
    }

    /// An operator's cordon, drain or uncordon of `name`.
    pub async fn place(&mut self, name: &str, action: NodeAction) {
        let what = match action {
            NodeAction::Cordon => "cordon".to_owned(),
            NodeAction::Drain(within) => format!("drain {}ms", within.as_millis()),
            NodeAction::Uncordon => "uncordon".to_owned(),
        };
        self.t.say(&format!("place {name} {what}"));
        match self.farm().place(&WorkerId::new(name), action) {
            Ok(_) => self.t.note("placed".to_owned()),
            Err(e) => self.t.note(format!("not placed: {e}")),
        }
        self.t.sync().await;
    }

    /// A handover takes the work from this farm.
    pub async fn fence(&mut self) {
        self.t.say("fence");
        self.farm().fence();
        self.t.sync().await;
    }

    /// The handover did not happen.
    pub async fn resume(&mut self) {
        self.t.say("resume");
        self.farm().resume();
        self.t.sync().await;
    }

    /// A drain closes the intake.
    pub async fn close_intake(&mut self) {
        self.t.say("close_intake");
        self.farm().close_intake();
        self.t.sync().await;
    }

    /// An operator raises the stored epoch.
    pub async fn bump_epoch(&mut self) {
        self.t.say("bump_epoch");
        match self.farm().bump_epoch() {
            Ok(Some(bump)) => self
                .t
                .note(format!("epoch {} -> {}", bump.previous, bump.epoch)),
            Ok(None) => self.t.note("no state store".to_owned()),
            Err(e) => self.t.note(format!("not bumped: {e}")),
        }
        self.t.sync().await;
    }

    /// The process dies and another starts over the same store, with `policy`. The
    /// streams and the callers die with it; the store and the cache stay.
    pub async fn restart(&mut self, policy: EpochPolicy) {
        let how = match policy {
            EpochPolicy::Keep => "keep",
            EpochPolicy::Bump => "bump",
        };
        self.t.say(&format!("restart epoch={how}"));
        drop(self.farm.take());
        self.t.restart();
        self.farm = Some(self.build(policy));
        let note = format!(
            "farm term={} lease_epoch={}",
            self.farm().term(),
            self.farm().lease_epoch()
        );
        self.t.note(note);
        self.t.sync().await;
    }

    /// The lease the newest `Start` of `label` named.
    pub fn lease_of(&self, label: &str) -> LeaseId {
        self.t.lease_of(label)
    }

    /// The node the newest `Start` of `label` went to.
    pub fn holder_of(&self, label: &str) -> String {
        self.t.holder_of(label)
    }

    /// How many `Start`s were sent for `label` so far.
    pub fn starts_of(&self, label: &str) -> usize {
        self.t.starts_of(label)
    }
}
