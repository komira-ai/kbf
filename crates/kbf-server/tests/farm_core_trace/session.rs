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
//! The order inside a call between these four is a convention of this file, not something
//! the public API shows; the order within each is the farm's. The text is built by hand
//! from the fields, never from `Debug`, so a field that is added to a message is added to
//! the text on purpose.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Described, Dispatch, Finished, MemoryMetaLog, Stage, Submission};
use kbf_objstore::MemoryStore;
use kbf_proto::google::rpc;
use kbf_proto::reapi::{ActionResult, Digest as ProtoDigest};
use kbf_proto::worker::{self, ServerMessage, server_message};
use kbf_sched::{DaemonInstance, NodePhase, Request};
use kbf_server::farm::{NodeAction, Reported, StreamId, TestCore};
use kbf_server::{EpochPolicy, Farm, StateStore};
use kbf_state::Store;
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseId, LeaseKind, Qos, Resources, WorkerId};
use prost_types::Timestamp;
use tokio::sync::{mpsc, watch};
use tonic::Status;

use crate::tape::Tape;

type MemoryCache = Cache<MemoryMetaLog, MemoryStore>;

/// The term a new store is seeded with.
pub const TERM: u64 = 7;
/// The wall clock when the farm is built, in milliseconds since the Unix epoch.
const WALL_MS: u64 = 1_900_000_000_000;
/// One GiB.
pub const GIB: u64 = 1 << 30;

/// An action the script submits: what it is called in the text, the digest it is keyed
/// by, and how it books.
#[derive(Clone, Debug)]
pub struct Job {
    label: &'static str,
    tag: u8,
    qos: Qos,
    kind: LeaseKind,
    resources: Resources,
    do_not_cache: bool,
}

impl Job {
    /// An action of the `ci` level that books one core and one GiB.
    pub fn new(label: &'static str, tag: u8) -> Self {
        Self {
            label,
            tag,
            qos: Qos::Ci,
            kind: LeaseKind::Action,
            resources: Resources::new(1_000, GIB),
            do_not_cache: false,
        }
    }

    pub fn qos(self, qos: Qos) -> Self {
        Self { qos, ..self }
    }

    pub fn kind(self, kind: LeaseKind) -> Self {
        Self { kind, ..self }
    }

    pub fn cores(self, cores: u64) -> Self {
        let resources = Resources::new(cores * 1_000, self.resources.memory_bytes);
        Self { resources, ..self }
    }

    pub fn gib(self, gib: u64) -> Self {
        let resources = Resources::new(self.resources.cpu_millis, gib * GIB);
        Self { resources, ..self }
    }

    pub fn do_not_cache(self) -> Self {
        Self {
            do_not_cache: true,
            ..self
        }
    }

    fn digest(&self) -> Digest {
        digest_of(self.tag)
    }
}

fn digest_of(tag: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag))
}

/// How a node reports the end of a lease.
#[derive(Clone, Copy, Debug)]
pub enum Ended {
    /// The action ran and exited with this code.
    Ran(i32),
    /// The daemon refused the action as invalid.
    Invalid,
    /// The run passed its own memory limit and was killed.
    OwnLimit,
    /// The node killed the run for memory pressure it was not the cause of.
    NodePressure,
    /// The daemon restarted while the action ran.
    LostToRestart,
    /// The node could not run the action (a full disk).
    Infra,
}

/// A node's stream: what the farm sends it, and the seq of its next heartbeat.
struct Node {
    stream: StreamId,
    inbox: mpsc::UnboundedReceiver<Result<ServerMessage, Status>>,
    beat: u64,
    /// The drivers its `Hello` named.
    drivers: Vec<String>,
}

/// A caller's stage, watched for changes.
struct Watched {
    label: String,
    stage: watch::Receiver<Stage>,
    last: String,
}

/// A `Start` a node was sent.
struct Seen {
    node: String,
    label: &'static str,
    lease: LeaseId,
    digest: ProtoDigest,
}

/// The farm, the store under it, and the text of everything the script did.
pub struct Session {
    farm: Option<Farm>,
    tape: Arc<Tape>,
    cache: Arc<MemoryCache>,
    out: Vec<String>,
    nodes: BTreeMap<String, Node>,
    watched: Vec<Watched>,
    seen: Vec<Seen>,
    labels: BTreeMap<String, &'static str>,
    tags: BTreeMap<&'static str, u8>,
    submits: BTreeMap<&'static str, u32>,
    /// The operation name each caller was given.
    names: BTreeMap<String, String>,
    cached: BTreeSet<&'static str>,
    t_ms: u64,
}

impl Session {
    /// A farm over an empty store, as a first start (the store's epoch is the seed).
    pub async fn start(title: &str) -> Self {
        let mut session = Self {
            farm: None,
            tape: Arc::new(Tape::new()),
            cache: Arc::new(Cache::memory()),
            out: vec![format!("# {title}")],
            nodes: BTreeMap::new(),
            watched: Vec::new(),
            seen: Vec::new(),
            labels: BTreeMap::new(),
            tags: BTreeMap::new(),
            submits: BTreeMap::new(),
            names: BTreeMap::new(),
            cached: BTreeSet::new(),
            t_ms: 0,
        };
        session.farm = Some(session.build(EpochPolicy::Keep));
        let farm = session.farm();
        let note = format!(
            "  farm term={} lease_epoch={}",
            farm.term(),
            farm.lease_epoch()
        );
        session.say("start");
        session.out.push(note);
        session.sync().await;
        session
    }

    /// The text, one line per line, ending in a newline.
    pub fn text(&self) -> String {
        let mut text = self.out.join("\n");
        text.push('\n');
        text
    }

    fn build(&self, policy: EpochPolicy) -> Farm {
        let core = TestCore::new(
            TERM,
            WALL_MS,
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        );
        let store: Arc<dyn Store> = self.tape.clone();
        Farm::for_test(self.cache.clone(), core)
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

    fn say(&mut self, call: &str) {
        self.out.push(format!("> [t={}ms] {call}", self.t_ms));
    }

    fn note(&mut self, line: String) {
        self.out.push(format!("  {line}"));
    }

    /// Registers `name` with `cores` cores and `gib` GiB, running `drivers`.
    pub async fn register(&mut self, name: &str, cores: u64, gib: u64, drivers: &[&str]) {
        self.say(&format!(
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
        self.nodes.insert(
            name.to_owned(),
            Node {
                stream,
                inbox,
                beat: 0,
                drivers: drivers.iter().map(|d| (*d).to_owned()).collect(),
            },
        );
        self.sync().await;
    }

    /// The node's daemon resends its `Hello` with another phase.
    pub async fn phase(&mut self, name: &str, cores: u64, gib: u64, phase: NodePhase) {
        self.say(&format!("hello again {name} phase={}", phase_text(&phase)));
        let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .expect("caps")
            .with_drivers(self.nodes[name].drivers.iter().map(String::as_str));
        let reported = Reported {
            capacity: Resources::new(cores * 1_000, gib * GIB),
            caps,
            phase,
            protocol: None,
        };
        let stream = self.nodes[name].stream;
        self.farm().resize(&WorkerId::new(name), stream, reported);
        self.sync().await;
    }

    /// The node's stream ends: what the farm sent it is dropped from then on.
    pub async fn disconnect(&mut self, name: &str) {
        self.say(&format!("disconnect {name}"));
        self.nodes.remove(name);
        self.sync().await;
    }

    /// Submits `job` as one more caller.
    pub async fn submit(&mut self, job: &Job) {
        let count = self.submits.entry(job.label).or_default();
        *count += 1;
        let n = *count;
        let proto = kbf_front::digest_to_proto(&job.digest());
        self.labels.insert(proto.hash, job.label);
        self.tags.insert(job.label, job.tag);
        self.say(&format!(
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
        });
        match submitted {
            Ok(ticket) => {
                let label = format!("{}#{n}", job.label);
                self.note(format!("ticket {label} name={}", ticket.name));
                self.names.insert(label.clone(), ticket.name.clone());
                self.watched.push(Watched {
                    label,
                    stage: ticket.stage,
                    last: String::new(),
                });
            }
            Err(status) => self.note(format!(
                "refused code={} message={:?}",
                status.code() as i32,
                status.message()
            )),
        }
        self.sync().await;
    }

    /// Asks again for the operation the caller `of` was given, as WaitExecution does
    /// after a restart, and follows it as `label`.
    pub async fn wait(&mut self, label: &str, of: &str) {
        let name = self.names[of].clone();
        self.say(&format!("wait {name} as {label}"));
        match self.farm().wait(&name) {
            Some(ticket) => self.watched.push(Watched {
                label: label.to_owned(),
                stage: ticket.stage,
                last: String::new(),
            }),
            None => self.note("no such operation".to_owned()),
        }
        self.sync().await;
    }

    /// The node's heartbeat, naming the leases it still runs.
    pub async fn heartbeat(&mut self, name: &str, running: &[LeaseId]) {
        let leases = running.to_vec();
        let node = self.nodes.get_mut(name).expect("a connected node");
        node.beat += 1;
        let (stream, seq) = (node.stream, node.beat);
        self.say(&format!(
            "heartbeat {name} seq={seq} running=[{}]",
            lease_list(&leases)
        ));
        let acked = self
            .farm()
            .heartbeat(&WorkerId::new(name), stream, seq, leases)
            .await;
        self.note(format!("acknowledged={acked}"));
        self.sync().await;
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
        self.say(&format!(
            "result {name} lease={} action={label} ended={}",
            lease_text(lease),
            ended_text(ended)
        ));
        let digest = self
            .seen
            .iter()
            .rev()
            .find(|s| s.label == label && s.lease == lease)
            .map(|s| s.digest.clone());
        let ack = self
            .farm()
            .report(&WorkerId::new(name), result_of(lease, digest, label, ended))
            .await;
        match ack {
            Some(ack) => self.note(format!(
                "ack lease={} accepted={} reason={}",
                wire_lease_text(ack.lease_id.as_ref()),
                ack.accepted,
                worker::ResultAckReason::try_from(ack.reason).map_or("?", |r| r.as_str_name())
            )),
            None => self.note("no ack".to_owned()),
        }
        self.sync().await;
    }

    /// The farm's tick.
    pub async fn tick(&mut self) {
        self.say("tick");
        self.farm().tick().await;
        self.sync().await;
    }

    /// Time passes on tokio's clock, the store's and so the wall clock.
    pub async fn advance(&mut self, by: Duration) {
        self.t_ms += u64::try_from(by.as_millis()).expect("a short span");
        tokio::time::advance(by).await;
        self.tape.advance(by);
        self.say(&format!("advance {}ms", by.as_millis()));
        self.sync().await;
    }

    /// An operator's cordon, drain or uncordon of `name`.
    pub async fn place(&mut self, name: &str, action: NodeAction) {
        let what = match action {
            NodeAction::Cordon => "cordon".to_owned(),
            NodeAction::Drain(within) => format!("drain {}ms", within.as_millis()),
            NodeAction::Uncordon => "uncordon".to_owned(),
        };
        self.say(&format!("place {name} {what}"));
        match self.farm().place(&WorkerId::new(name), action) {
            Ok(_) => self.note("placed".to_owned()),
            Err(e) => self.note(format!("not placed: {e}")),
        }
        self.sync().await;
    }

    /// A handover takes the work from this farm.
    pub async fn fence(&mut self) {
        self.say("fence");
        self.farm().fence();
        self.sync().await;
    }

    /// The handover did not happen.
    pub async fn resume(&mut self) {
        self.say("resume");
        self.farm().resume();
        self.sync().await;
    }

    /// A drain closes the intake.
    pub async fn close_intake(&mut self) {
        self.say("close_intake");
        self.farm().close_intake();
        self.sync().await;
    }

    /// An operator raises the stored epoch.
    pub async fn bump_epoch(&mut self) {
        self.say("bump_epoch");
        match self.farm().bump_epoch() {
            Ok(Some(bump)) => self.note(format!("epoch {} -> {}", bump.previous, bump.epoch)),
            Ok(None) => self.note("no state store".to_owned()),
            Err(e) => self.note(format!("not bumped: {e}")),
        }
        self.sync().await;
    }

    /// The process dies and another starts over the same store, with `policy`. The
    /// streams and the callers die with it; the store and the cache stay.
    pub async fn restart(&mut self, policy: EpochPolicy) {
        let how = match policy {
            EpochPolicy::Keep => "keep",
            EpochPolicy::Bump => "bump",
        };
        self.say(&format!("restart epoch={how}"));
        drop(self.farm.take());
        self.nodes.clear();
        self.watched.clear();
        let old = std::mem::replace(&mut self.tape, Arc::new(Tape::new()));
        let old = Arc::into_inner(old).expect("nothing but the dead farm held the store");
        self.tape = Arc::new(old.restart());
        self.farm = Some(self.build(policy));
        let note = format!(
            "farm term={} lease_epoch={}",
            self.farm().term(),
            self.farm().lease_epoch()
        );
        self.note(note);
        self.sync().await;
    }

    /// The lease the newest `Start` of `label` named.
    pub fn lease_of(&self, label: &str) -> LeaseId {
        self.seen
            .iter()
            .rev()
            .find(|s| s.label == label)
            .map(|s| s.lease)
            .unwrap_or_else(|| panic!("no Start was sent for {label}:\n{}", self.text()))
    }

    /// The node the newest `Start` of `label` went to.
    pub fn holder_of(&self, label: &str) -> String {
        self.seen
            .iter()
            .rev()
            .find(|s| s.label == label)
            .map(|s| s.node.clone())
            .unwrap_or_else(|| panic!("no Start was sent for {label}:\n{}", self.text()))
    }

    /// How many `Start`s were sent for `label` so far.
    pub fn starts_of(&self, label: &str) -> usize {
        self.seen.iter().filter(|s| s.label == label).count()
    }

    /// Writes down what happened since the last call: the store's transactions, the
    /// action-cache entries that appeared, the messages sent to the nodes, and the
    /// stages that changed.
    async fn sync(&mut self) {
        let mut lines = self.tape.take();
        for (label, tag) in self.tags.clone() {
            if self.cached.contains(label) {
                continue;
            }
            let found = self.cache.action_result(&digest_of(tag)).await;
            if let Ok(Some((_, result))) = found {
                self.cached.insert(label);
                lines.push(format!("  cache {label} exit={}", result.exit_code));
            }
        }
        for (name, node) in &mut self.nodes {
            while let Ok(sent) = node.inbox.try_recv() {
                lines.push(format!(
                    "  msg {name} {}",
                    message_text(&sent, &self.labels)
                ));
                if let Ok(ServerMessage {
                    message: Some(server_message::Message::Start(start)),
                }) = &sent
                    && let Some(lease) = &start.lease_id
                    && let Some(digest) = &start.action_digest
                    && let Some(label) = self.labels.get(&digest.hash)
                {
                    self.seen.push(Seen {
                        node: name.clone(),
                        label,
                        lease: LeaseId::new(lease.term, lease.seq),
                        digest: digest.clone(),
                    });
                }
            }
        }
        for caller in &mut self.watched {
            let now = stage_text(&caller.stage.borrow());
            if now != caller.last {
                lines.push(format!("  stage {} {now}", caller.label));
                caller.last = now;
            }
        }
        self.out.extend(lines);
    }
}

fn lease_text(lease: LeaseId) -> String {
    format!("{}/{}", lease.term, lease.seq)
}

fn wire_lease_text(lease: Option<&worker::LeaseId>) -> String {
    lease.map_or_else(
        || "-".to_owned(),
        |l| lease_text(LeaseId::new(l.term, l.seq)),
    )
}

fn lease_list(leases: &[LeaseId]) -> String {
    let texts: Vec<String> = leases.iter().map(|l| lease_text(*l)).collect();
    texts.join(",")
}

fn phase_text(phase: &NodePhase) -> String {
    match phase {
        NodePhase::Serving => "serving".to_owned(),
        NodePhase::Preparing(lease) => format!("preparing({})", lease_text(*lease)),
        NodePhase::Restoring(lease) => format!("restoring({})", lease_text(*lease)),
        NodePhase::Quarantined(why) => format!("quarantined({why:?})"),
        NodePhase::MemoryPressure => "memory_pressure".to_owned(),
        NodePhase::Draining => "draining".to_owned(),
    }
}

fn ended_text(ended: Ended) -> String {
    match ended {
        Ended::Ran(code) => format!("ran(exit={code})"),
        Ended::Invalid => "invalid".to_owned(),
        Ended::OwnLimit => "own_limit".to_owned(),
        Ended::NodePressure => "node_pressure".to_owned(),
        Ended::LostToRestart => "lost_to_restart".to_owned(),
        Ended::Infra => "infra".to_owned(),
    }
}

fn status(code: tonic::Code, message: &str) -> Option<rpc::Status> {
    Some(rpc::Status {
        code: code as i32,
        message: message.to_owned(),
        details: Vec::new(),
    })
}

/// The `Result` a node sends for `lease`, which ran `label`.
fn result_of(
    lease: LeaseId,
    digest: Option<ProtoDigest>,
    label: &str,
    ended: Ended,
) -> worker::Result {
    let mut result = worker::Result {
        lease_id: Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        status: None,
        action_result: None,
        action_digest: digest,
        memory_kill: 0,
        lost: 0,
        job_outcome: None,
    };
    match ended {
        Ended::Ran(code) => {
            result.action_result = Some(ActionResult {
                exit_code: code,
                stdout_raw: format!("output of {label}").into_bytes(),
                ..Default::default()
            });
        }
        Ended::Invalid => result.status = status(tonic::Code::InvalidArgument, "bad input"),
        Ended::OwnLimit => {
            result.status = status(tonic::Code::ResourceExhausted, "killed");
            result.memory_kill = worker::MemoryKill::OwnLimit as i32;
        }
        Ended::NodePressure => {
            result.status = status(tonic::Code::ResourceExhausted, "killed");
            result.memory_kill = worker::MemoryKill::NodePressure as i32;
        }
        Ended::LostToRestart => {
            result.status = status(tonic::Code::Unavailable, "daemon restarted");
            result.lost = worker::Lost::DaemonRestarted as i32;
        }
        Ended::Infra => result.status = status(tonic::Code::Internal, "disk full"),
    }
    result
}

fn action_text(digest: Option<&ProtoDigest>, labels: &BTreeMap<String, &'static str>) -> String {
    digest.map_or_else(
        || "-".to_owned(),
        |d| {
            labels.get(&d.hash).map_or_else(
                || format!("{}:{}", &d.hash[..8], d.size_bytes),
                |label| (*label).to_owned(),
            )
        },
    )
}

fn message_text(
    sent: &Result<ServerMessage, Status>,
    labels: &BTreeMap<String, &'static str>,
) -> String {
    let message = match sent {
        Ok(ServerMessage { message: Some(m) }) => m,
        Ok(ServerMessage { message: None }) => return "empty".to_owned(),
        Err(status) => {
            return format!(
                "error code={} message={:?}",
                status.code() as i32,
                status.message()
            );
        }
    };
    match message {
        server_message::Message::Welcome(w) => format!(
            "welcome protocol={} interval_ms={} epoch={} pack_commit={}",
            w.protocol_version, w.heartbeat_interval_ms, w.epoch, w.pack_commit
        ),
        server_message::Message::HeartbeatAck(a) => format!("heartbeat_ack seq={}", a.seq),
        server_message::Message::Start(s) => format!(
            "start lease={} kind={} action={} millicpus={} memory_bytes={} heartbeat_seq={} \
             valid_for_ms={} device={:?} job={} fence={}",
            wire_lease_text(s.lease_id.as_ref()),
            s.kind,
            action_text(s.action_digest.as_ref(), labels),
            s.millicpus,
            s.memory_bytes,
            s.heartbeat_seq,
            s.valid_for_ms,
            s.device_id,
            s.job.is_some(),
            s.fence
        ),
        server_message::Message::LeaseOffer(o) => format!(
            "lease_offer lease={} kind={} action={}",
            wire_lease_text(o.lease_id.as_ref()),
            o.kind,
            action_text(o.action_digest.as_ref(), labels)
        ),
        server_message::Message::ResultAck(a) => format!(
            "result_ack lease={} accepted={} reason={}",
            wire_lease_text(a.lease_id.as_ref()),
            a.accepted,
            worker::ResultAckReason::try_from(a.reason).map_or("?", |r| r.as_str_name())
        ),
        server_message::Message::Cancel(c) => {
            format!("cancel lease={}", wire_lease_text(c.lease_id.as_ref()))
        }
        server_message::Message::MapUpdate(_) => "map_update".to_owned(),
        server_message::Message::UpdateOffer(_) => "update_offer".to_owned(),
        server_message::Message::UpdateOrder(_) => "update_order".to_owned(),
    }
}

fn rpc_status_text(status: &rpc::Status) -> String {
    let details: Vec<&str> = status.details.iter().map(|d| d.type_url.as_str()).collect();
    format!(
        "code={} message={:?} details=[{}]",
        status.code,
        status.message,
        details.join(",")
    )
}

fn ms(stamp: Option<&Timestamp>) -> String {
    stamp.map_or_else(
        || "-".to_owned(),
        |t| (t.seconds * 1_000 + i64::from(t.nanos) / 1_000_000).to_string(),
    )
}

fn result_text(result: &ActionResult) -> String {
    let meta = result.execution_metadata.as_ref();
    format!(
        "exit={} stdout={:?} worker={:?} queued_ms={} started_ms={} completed_ms={}",
        result.exit_code,
        String::from_utf8_lossy(&result.stdout_raw),
        meta.map_or("", |m| m.worker.as_str()),
        ms(meta.and_then(|m| m.queued_timestamp.as_ref())),
        ms(meta.and_then(|m| m.worker_start_timestamp.as_ref())),
        ms(meta.and_then(|m| m.worker_completed_timestamp.as_ref())),
    )
}

fn stage_text(stage: &Stage) -> String {
    match stage {
        Stage::Queued => "queued".to_owned(),
        Stage::Waiting(why) => format!("waiting {why:?}"),
        Stage::WaitingKept(why) => format!("waiting_kept {why:?}"),
        Stage::Executing => "executing".to_owned(),
        Stage::Done(Finished::Ran(result)) => format!("done ran {}", result_text(result)),
        Stage::Done(Finished::Failed(status)) => {
            format!("done failed {}", rpc_status_text(status))
        }
        Stage::Done(Finished::TimedOut { status, partial }) => format!(
            "done timed_out {} partial: {}",
            rpc_status_text(status),
            result_text(partial)
        ),
    }
}
