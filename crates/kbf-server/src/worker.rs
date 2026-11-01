//! The server side of `kbf.worker.v1`: one `Session` stream per daemon.
//!
//! Under mutual TLS a stream without a usable client certificate is refused before its
//! first message is read. The first message must be `Hello`. It is checked (protocol
//! version, node id, that the certificate names that node and neither is denied: see
//! [`identity`](crate::identity) and issue #79; the capacity entries of the node report,
//! and the entries placement matches platforms against: `arch` and the rest that
//! [`caps`] reads), answered with `Welcome`, and registers the node: only this first
//! `Hello` does (issue #25). `Welcome` names the farm's lease epoch
//! ([`Farm::lease_epoch`]: this process's term, or the Raft log's id; issue #137).
//! The registration names the daemon process by the `Hello`'s `instance_id`: leases
//! whose `Start` went to another process are given up only once it has fenced (issue
//! #140), or at once if the `Hello` names that process as its predecessor and the
//! stream it came from has closed (`predecessor_instance_id`; issue #22). On the
//! stream after that:
//! - a resent `Hello` whose `node_id` is not the stream's node, or that the deny list
//!   now refuses, ends the stream with that error; otherwise it changes the node's
//!   capacity and capabilities and nothing else;
//! - a `Heartbeat` the deny list now refuses ends the stream. Otherwise it is fed to
//!   the scheduler and acknowledged, unless a newer stream of
//!   the same node has registered since: then it is dropped unacknowledged, so a
//!   daemon still talking on the old stream fences on time. Over a state store it is
//!   fed but not acknowledged either when this frontend no longer owns the node (the daemon
//!   said `Hello` to another) or has not written the node's session row lately
//!   ([`crate::farm::ACK_WINDOW`]). A lease it lists that the
//!   scheduler no longer holds on the node is sent a `Cancel` (issue #23). A lease it
//!   leaves out that was lost to a restart past its farm reruns answers the
//!   operation's callers. The pack-read counts it carries, if any, are kept as the node's
//!   newest for `/metrics` ([`Farm::note_pack_reads`]), and so are its pack-write counts
//!   ([`Farm::note_pack_writes`]) and its input cache counts ([`Farm::note_input_cache`]);
//! - a `Result` the deny list now refuses ends the stream, so it never reaches the
//!   action cache. Otherwise it is accepted only from the node holding the
//!   operation's current lease, and only if the action it names (if any) is the one
//!   that lease runs, and answered with a `ResultAck`. It is settled on a task of its
//!   own: its writes to the object store may be slow, and the messages behind it
//!   (heartbeats above all) are handled meanwhile;
//! - a `NodeStatus` the deny list now refuses ends the stream, so it never reaches
//!   the operator API. Otherwise it is kept as the node's newest software status,
//!   unless a newer stream of the node has registered since;
//! - after the server ends a stream, nothing more is read from it;
//! - a `MapRequest` is answered with a `MapUpdate` of the storage map the server holds
//!   now, when it holds one (the `map_push` module);
//! - an `UpdateStatus` the deny list now refuses ends the stream. Otherwise it is kept
//!   as the node's newest update status, unless a newer stream of the node has
//!   registered since ([`Farm::update_status`]); the update session reads it
//!   ([`crate::update_status`]);
//! - an `Offer` from a daemon is not read: offers go the other way.
//!
//! A server given a [`MapSource`] ([`WorkerService::with_map`]) names its map in
//! `Welcome`, and sends a `MapUpdate` each time the source's map changes. A server that serves
//! `PackIndex/CommitPack` ([`WorkerService::with_pack_commit`]) says so in
//! `Welcome.pack_commit`; a daemon that writes packs commits them only to one that does.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::{Stream, stream};
use kbf_caps::NodeCaps;
use kbf_mapsync::MapSource;
use kbf_proto::worker::{
    Capability, DaemonMessage, HeartbeatAck, Hello, Phase, ServerMessage, Welcome, daemon_message,
    server_message, worker_server::Worker,
};
use kbf_sched::{DaemonInstance, NodePhase};
use kbf_types::{LeaseId, Resources, WorkerId};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tonic::{Request, Response, Status, Streaming};

use crate::farm::{Farm, Outbound, Reported, StreamId};
use crate::identity::{PeerCert, Peers, presented};
use crate::map_push::MapFeed;

/// The `kbf.worker.v1` version this server speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The oldest version it accepts. The protocol promises N-1 and N; version 1 is the
/// first, so there is no N-1 yet.
pub const OLDEST_PROTOCOL_VERSION: u32 = 1;

/// The protocol version a wire field names: `None` for 0. Versions start at 1, and
/// proto3 cannot tell a 0 from a field the sender never set (a `Hello` or a storage
/// registration from a build that predates the field, or a stored row written before
/// the field existed), so a 0 is a node whose version is not known and not a node that
/// speaks version 0.
#[must_use]
pub const fn named_protocol(wire: u32) -> Option<u32> {
    if wire == 0 { None } else { Some(wire) }
}

/// The `Worker` service over a [`Farm`].
#[derive(Debug)]
pub struct WorkerService {
    farm: Arc<Farm>,
    peers: Peers,
    heartbeat_interval: Duration,
    hello_wait: Duration,
    /// What a node that reports VM slots keeps for itself ([`capacity`]).
    host_floor: HostFloor,
    map: MapFeed,
    /// The oldest and the newest `Hello` version a session accepts.
    protocols: (u32, u32),
    /// Whether `Welcome` says this frontend takes `PackIndex/CommitPack` (`Welcome.pack_commit`).
    pack_commit: bool,
    /// What `Welcome.placement_load` carries (`--placement-load`).
    placement_load: Option<bool>,
}

impl WorkerService {
    /// The service over `farm`, knowing its daemons as `peers`. `Welcome` asks daemons
    /// for a heartbeat every `heartbeat_interval`; a stream that sends no `Hello` within
    /// `hello_wait` is ended DEADLINE_EXCEEDED.
    pub const fn new(
        farm: Arc<Farm>,
        peers: Peers,
        heartbeat_interval: Duration,
        hello_wait: Duration,
    ) -> Self {
        Self {
            farm,
            peers,
            heartbeat_interval,
            hello_wait,
            host_floor: HostFloor::DEFAULT,
            map: MapFeed::NONE,
            protocols: (OLDEST_PROTOCOL_VERSION, PROTOCOL_VERSION),
            pack_commit: false,
            placement_load: None,
        }
    }

    /// The service accepting `Hello` versions from `oldest` and answering a newer one
    /// with `newest`. Only a test of the negotiation needs a range other than this
    /// build's: both are 1 today.
    #[doc(hidden)]
    #[must_use]
    pub const fn with_protocols(mut self, oldest: u32, newest: u32) -> Self {
        self.protocols = (oldest, newest);
        self
    }

    /// The service keeping `floor` of the CPUs and memory of each node that reports VM
    /// slots for the node itself ([`capacity`]).
    #[must_use]
    pub const fn with_host_floor(mut self, floor: HostFloor) -> Self {
        self.host_floor = floor;
        self
    }

    /// The service telling each daemon, in `Welcome.pack_commit`, whether this frontend takes
    /// `kbf.worker.v1.PackIndex/CommitPack` (`on`): a daemon that writes packs commits them only
    /// to a frontend that says so. Off for a service built without it.
    #[must_use]
    pub const fn with_pack_commit(mut self, on: bool) -> Self {
        self.pack_commit = on;
        self
    }

    /// The service telling each daemon, in `Welcome.placement_load`, the mode of
    /// `--placement-load`. `off` (the default) leaves the field out.
    #[must_use]
    pub const fn with_placement_load(mut self, mode: crate::PlacementLoad) -> Self {
        self.placement_load = mode.welcome();
        self
    }

    /// The service sending the storage map `map` holds to each daemon: in `Welcome`, and
    /// in a `MapUpdate` each time it changes.
    #[must_use]
    pub fn with_map(mut self, map: Arc<dyn MapSource>) -> Self {
        self.map = MapFeed::new(map);
        self
    }
}

/// The server's half of a session.
pub type SessionStream = Pin<Box<dyn Stream<Item = Result<ServerMessage, Status>> + Send>>;

#[tonic::async_trait]
impl Worker for WorkerService {
    type SessionStream = SessionStream;

    async fn session(
        &self,
        request: Request<Streaming<DaemonMessage>>,
    ) -> Result<Response<SessionStream>, Status> {
        let peer = self.peers.peer_of(&presented(request.extensions()))?;
        let mut inbound = request.into_inner();
        let first = timeout(self.hello_wait, inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("no Hello within the wait"))??;
        let Some(daemon_message::Message::Hello(hello)) = first.and_then(|m| m.message) else {
            return Err(Status::invalid_argument(
                "the first message of a session must be Hello",
            ));
        };
        let (oldest, newest) = self.protocols;
        let version = negotiate(hello.protocol_version, oldest, newest)?;
        if hello.node_id.is_empty() {
            return Err(Status::invalid_argument("Hello has no node_id"));
        }
        self.peers.admit(peer.as_ref(), &hello.node_id).await?;
        let mut reported = reported(&hello, self.host_floor).map_err(Status::invalid_argument)?;
        // The stream speaks what `Welcome` names, which a newer daemon's `Hello` may exceed.
        reported.protocol = Some(version);
        let worker = WorkerId::new(hello.node_id);
        let instance = DaemonInstance::new(hello.instance_id);
        let predecessor = DaemonInstance::new(hello.predecessor_instance_id);
        // Subscribed before the map is read: a change between the two is sent again as a
        // `MapUpdate`, never missed.
        let changes = self.map.subscribe();
        let welcome = message(server_message::Message::Welcome(Welcome {
            protocol_version: version,
            heartbeat_interval_ms: u64::try_from(self.heartbeat_interval.as_millis())
                .unwrap_or(u64::MAX),
            epoch: self.farm.lease_epoch(),
            map: self.map.welcome_map(),
            pack_commit: self.pack_commit,
            placement_load: self.placement_load,
        }));
        let (outbound, responses) = mpsc::unbounded_channel();
        let capacity = reported.capacity;
        let stream = self.farm.register(
            &worker,
            instance,
            predecessor,
            reported,
            outbound.clone(),
            welcome,
        );
        tracing::info!(%worker, ?capacity, "worker registered");
        // After `register`, which queued the `Welcome`: the first update follows it.
        self.map.push(changes, &outbound);
        let session = Session {
            map: self.map.clone(),
            farm: Arc::clone(&self.farm),
            peers: self.peers.clone(),
            peer,
            worker,
            stream,
            host_floor: self.host_floor,
        };
        tokio::spawn(session.serve(inbound, outbound));
        Ok(Response::new(Box::pin(stream::unfold(
            responses,
            |mut responses| async move { responses.recv().await.map(|m| (m, responses)) },
        ))))
    }
}

/// The version a session speaks, which `Welcome` names: the lesser of `hello`, the
/// version the `Hello` names, and `newest`, when `hello` is at least `oldest`.
///
/// A `Hello` in `oldest..=newest` is answered with its own version, which a daemon of
/// that version requires (it checks that `Welcome` names the version it sent). A `Hello`
/// above `newest` is a daemon newer than this server: it is answered with `newest`, and a
/// daemon newer than this server must accept a `Welcome` below its own version (the daemon
/// does, down to the oldest it speaks). A `Hello` below `oldest` is refused.
///
/// # Errors
/// FAILED_PRECONDITION: `hello` is below `oldest`.
fn negotiate(hello: u32, oldest: u32, newest: u32) -> Result<u32, Status> {
    if hello < oldest {
        return Err(Status::failed_precondition(format!(
            "protocol version {hello} is not accepted; this server speaks {oldest} to {newest}"
        )));
    }
    Ok(hello.min(newest))
}

/// One stream after its first `Hello`.
struct Session {
    map: MapFeed,
    farm: Arc<Farm>,
    peers: Peers,
    /// The stream's client certificate (`None` in plain text).
    peer: Option<PeerCert>,
    worker: WorkerId,
    stream: StreamId,
    host_floor: HostFloor,
}

impl Session {
    /// Handles the stream's messages until it ends, or one of them ends it.
    async fn serve(self, mut inbound: Streaming<DaemonMessage>, outbound: Outbound) {
        let worker = &self.worker;
        while let Ok(Some(received)) = inbound.message().await {
            // The response stream outlives this loop unless the stream is gone.
            match self.handle(received.message, &outbound).await {
                Ok(None) => {}
                Ok(Some(reply)) => {
                    let _ = outbound.send(Ok(message(reply)));
                }
                Err(status) => {
                    tracing::warn!(%worker, %status, "worker stream ended by the server");
                    let _ = outbound.send(Err(status));
                    break;
                }
            }
        }
        // The farm keeps the stream as the worker's until it registers again: `Start`s
        // sent meanwhile are lost, and the scheduler gives their leases up (at once on
        // the next session's first heartbeat, or after G).
        tracing::info!(%worker, "worker stream ended");
    }

    /// The reply to one message, if any; an error ends the stream with it. A `Result`
    /// is answered later, on `outbound`.
    async fn handle(
        &self,
        received: Option<daemon_message::Message>,
        outbound: &Outbound,
    ) -> Result<Option<server_message::Message>, Status> {
        let (farm, worker, stream) = (&self.farm, &self.worker, self.stream);
        let reply = match received {
            Some(daemon_message::Message::Hello(hello)) => {
                if hello.node_id != worker.as_str() {
                    return Err(Status::permission_denied(format!(
                        "a resent Hello names node {:?}; this stream is {worker}'s",
                        hello.node_id
                    )));
                }
                self.peers
                    .admit(self.peer.as_ref(), worker.as_str())
                    .await?;
                match reported(&hello, self.host_floor) {
                    Ok(reported) => farm.resize(worker, stream, reported),
                    Err(e) => tracing::warn!(%worker, error = %e, "a resent Hello ignored"),
                }
                None
            }
            Some(daemon_message::Message::Heartbeat(beat)) => {
                self.peers
                    .admit(self.peer.as_ref(), worker.as_str())
                    .await?;
                // Before the acknowledgement below, so a daemon that has been acknowledged
                // has its counts on `/metrics`.
                farm.note_pack_reads(worker, stream, beat.pack_reads);
                farm.note_pack_writes(worker, stream, beat.pack_writes);
                farm.note_input_cache(worker, stream, beat.input_cache);
                // The accepted reading goes to the scheduler with the heartbeat.
                let load = farm.note_node_load(worker, stream, beat.node_load.as_ref());
                let running = beat
                    .running
                    .iter()
                    .map(|l| LeaseId::new(l.term, l.seq))
                    .collect();
                farm.heartbeat_with_load(worker, stream, beat.seq, running, load)
                    .await
                    .then_some(server_message::Message::HeartbeatAck(HeartbeatAck {
                        seq: beat.seq,
                    }))
            }
            Some(daemon_message::Message::Result(result)) => {
                // A revoked daemon must not write its result into the action cache,
                // even before its next heartbeat.
                self.peers
                    .admit(self.peer.as_ref(), worker.as_str())
                    .await?;
                // Settling it writes to the object store and the metadata log, which
                // can take many seconds; it runs beside this loop, so the heartbeats
                // behind it are still acknowledged before the daemon fences.
                let (farm, worker, outbound) = (Arc::clone(farm), worker.clone(), outbound.clone());
                tokio::spawn(async move {
                    if let Some(ack) = farm.report(&worker, result).await {
                        let reply = server_message::Message::ResultAck(ack);
                        let _ = outbound.send(Ok(message(reply)));
                    }
                });
                None
            }
            Some(daemon_message::Message::NodeStatus(status)) => {
                // A revoked daemon's status must not reach the operator API either.
                self.peers
                    .admit(self.peer.as_ref(), worker.as_str())
                    .await?;
                // Logged there; the lines are for tests.
                let _ = farm.node_status(worker, stream, status);
                None
            }
            Some(daemon_message::Message::MapRequest(_)) => {
                self.peers
                    .admit(self.peer.as_ref(), worker.as_str())
                    .await?;
                self.map.answer()
            }
            Some(daemon_message::Message::UpdateStatus(status)) => {
                // A revoked daemon's status must not reach the update session either.
                self.peers
                    .admit(self.peer.as_ref(), worker.as_str())
                    .await?;
                // Kept as the node's newest, unless a newer stream of the node has
                // registered since.
                let _ = farm.update_status(worker, stream, status);
                None
            }
            // The frontend sends offers; one from a daemon is not read.
            Some(daemon_message::Message::Offer(_)) => None,
            None => {
                tracing::warn!(%worker, "an empty daemon message ignored");
                None
            }
        };
        Ok(reply)
    }
}

pub(crate) fn message(message: server_message::Message) -> ServerMessage {
    ServerMessage {
        message: Some(message),
    }
}

/// What a `Hello` reports: [`capacity`], [`caps`] and [`phase`], and the version it
/// speaks ([`named_protocol`] of `protocol_version`).
///
/// # Errors
/// Any of the three is malformed.
pub fn reported(hello: &Hello, floor: HostFloor) -> Result<Reported, String> {
    Ok(Reported {
        capacity: capacity(hello, floor)?,
        caps: caps(hello)?,
        phase: phase(hello)?,
        protocol: named_protocol(hello.protocol_version),
    })
}

/// The node's phase: serving when the `Hello` names none. A phase this server does not
/// know is read as a quarantine that says so: the node takes no lease and is listed for
/// attention, rather than being offered work it may not be ready for.
///
/// # Errors
/// A preparing or restoring phase that names no lease.
pub fn phase(hello: &Hello) -> Result<NodePhase, String> {
    let Some(reported) = &hello.phase else {
        return Ok(NodePhase::Serving);
    };
    let lease = || {
        reported
            .lease_id
            .map(|l| LeaseId::new(l.term, l.seq))
            .ok_or_else(|| "a preparing or restoring phase names no lease".to_owned())
    };
    Ok(match Phase::try_from(reported.phase) {
        Ok(Phase::Serving) => NodePhase::Serving,
        Ok(Phase::Preparing) => NodePhase::Preparing(lease()?),
        Ok(Phase::Restoring) => NodePhase::Restoring(lease()?),
        Ok(Phase::Quarantined) => NodePhase::Quarantined(reported.reason.clone()),
        Ok(Phase::MemoryPressure) => NodePhase::MemoryPressure,
        Ok(Phase::Draining) => NodePhase::Draining,
        // `Tainted` is on the wire before this server acts on it: the fallback applies.
        Ok(Phase::Tainted) | Err(_) => NodePhase::Quarantined(format!(
            "the node reports phase {}, which this server does not know",
            reported.phase
        )),
    })
}

/// The most macOS guests one Mac runs at once (Apple's limit); a node's `vm.slots` is
/// clamped to it.
pub const MAX_VM_SLOTS: u64 = 2;

/// What a node that reports VM slots keeps for the host itself: subtracted from its
/// capacity so that guests cannot take the host's own share of cores and memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostFloor {
    /// CPU, in thousandths of a core.
    pub cpu_millis: u64,
    /// Memory, in bytes.
    pub memory_bytes: u64,
}

impl HostFloor {
    /// The floor of `cpus` cores and `mem_gib` GiB.
    #[must_use]
    pub const fn new(cpus: u64, mem_gib: u64) -> Self {
        Self {
            cpu_millis: cpus.saturating_mul(1_000),
            memory_bytes: mem_gib.saturating_mul(1 << 30),
        }
    }

    /// 2 cores and 8 GiB.
    pub const DEFAULT: Self = Self::new(2, 8);
}

impl Default for HostFloor {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What placement may book on a node: its `cpus` and `mem_gib` entries, its `gpu` entry
/// (0 when absent: a daemon that does not detect GPUs has none to book) and its
/// `vm.slots` entry (0 when absent), clamped to [`MAX_VM_SLOTS`]. Its
/// `action_slots` entry, when reported (it counts the usable slots, busy ones
/// included), caps how many Action and VM leases it holds at once
/// (`Resources::leases`); a node that reports none has no cap. A node that reports
/// at least one VM slot gives up `floor` of its CPUs and memory to the host; any other
/// node books the whole machine. The other protected floors (planned,
/// `docs/design/capabilities.md#planned`) are subtracted once they are reported.
///
/// # Errors
/// `cpus` or `mem_gib` is missing; an entry is repeated or not a whole number.
pub fn capacity(hello: &Hello, floor: HostFloor) -> Result<Resources, String> {
    let required = |key| entry(&hello.capabilities, key)?.ok_or_else(|| missing(key));
    let cpus = required("cpus")?;
    let mem_gib = required("mem_gib")?;
    let gpus = entry(&hello.capabilities, "gpu")?.unwrap_or(0);
    let vms = entry(&hello.capabilities, "vm.slots")?
        .unwrap_or(0)
        .min(MAX_VM_SLOTS);
    let mut cpu_millis = cpus.saturating_mul(1_000);
    let mut memory_bytes = mem_gib.saturating_mul(1 << 30);
    if vms > 0 {
        cpu_millis = cpu_millis.saturating_sub(floor.cpu_millis);
        memory_bytes = memory_bytes.saturating_sub(floor.memory_bytes);
    }
    let booked = Resources::new(cpu_millis, memory_bytes)
        .with_gpus(gpus)
        .with_vms(vms);
    // The entry counts usable slots, those running a lease included: it is a cap on the
    // leases held, not a count of the free ones.
    Ok(match entry(&hello.capabilities, "action_slots")? {
        Some(slots) => booked.with_lease_cap(slots),
        None => booked,
    })
}

/// What placement matches an action's platform against: the node report read by
/// `kbf_caps::NodeCaps::from_report` (`arch`, `cpu.features`, `os`, labels, ...).
///
/// # Errors
/// `arch` is missing, repeated or unknown; another single-valued entry is repeated; a
/// countable entry is not a whole number.
pub fn caps(hello: &Hello) -> Result<NodeCaps, String> {
    let entries = hello
        .capabilities
        .iter()
        .map(|c| (c.key.as_str(), c.value.as_str()));
    NodeCaps::from_report(entries).map_err(|e| e.to_string())
}

fn missing(key: &str) -> String {
    format!("the node report needs exactly one {key:?} entry")
}

/// The value of the `key` entry, `None` if there is none.
fn entry(capabilities: &[Capability], key: &str) -> Result<Option<u64>, String> {
    let mut values = capabilities.iter().filter(|c| c.key == key);
    let (only, None) = (values.next(), values.next()) else {
        return Err(missing(key));
    };
    only.map(|only| {
        only.value.parse().map_err(|_| {
            format!(
                "node report entry {key}={:?} is not a whole number",
                only.value
            )
        })
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::{named_protocol, negotiate};

    /// Catches: a wire 0 (a field its sender never set) taken for a version, so that a
    /// node naming none would read as speaking version 0 and pull the oldest version of
    /// a list down to it; and a version that is changed or dropped on the way.
    #[test]
    fn a_wire_zero_names_no_protocol_and_any_other_number_is_itself() {
        assert_eq!(named_protocol(0), None);
        assert_eq!(named_protocol(1), Some(1));
        assert_eq!(named_protocol(u32::MAX), Some(u32::MAX));
    }

    /// Catches: a `Welcome` that names the server's newest version for a `Hello` in range
    /// (the integration tests speak version 1 to a server of version 1, so they cannot
    /// tell it from the `Hello`'s own: an older daemon requires its own version back);
    /// the greater of the two for a `Hello` above the newest (a daemon newer than the
    /// server would be told a version the server does not speak); that `Hello` refused,
    /// as it was before; and an accepted range with the oldest bound moved out.
    #[test]
    fn a_hello_is_answered_with_the_lesser_of_its_version_and_the_newest() {
        assert_eq!(negotiate(1, 1, 3).expect("the oldest"), 1);
        assert_eq!(negotiate(2, 1, 3).expect("in range"), 2);
        assert_eq!(negotiate(3, 1, 3).expect("the newest"), 3);
        assert_eq!(negotiate(5, 1, 3).expect("newer than the server"), 3);
        assert_eq!(negotiate(u32::MAX, 1, 3).expect("far newer"), 3);
        assert_eq!(negotiate(1, 1, 1).expect("the only version"), 1);
        assert_eq!(negotiate(2, 1, 1).expect("above the only version"), 1);
    }

    /// Catches: a version below the oldest accepted (the check dropped, or the bound
    /// moved out by one), the wrong code, and a message that does not name the version
    /// refused and the range the server speaks.
    #[test]
    fn a_hello_below_the_oldest_is_refused_naming_the_range() {
        let status = negotiate(0, 1, 3).expect_err("below the oldest");
        assert_eq!(status.code(), Code::FailedPrecondition);
        let said = "protocol version 0 is not accepted";
        assert!(status.message().contains(said), "{}", status.message());
        assert!(status.message().contains("1 to 3"), "{}", status.message());
        assert_eq!(
            negotiate(1, 2, 3)
                .expect_err("below a raised oldest")
                .code(),
            Code::FailedPrecondition
        );
    }
}
