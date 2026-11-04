//! The scripted session of `tests/farm_core_trace`, driven over `js_server_core::Core` with
//! kbf's adapter instead of over [`kbf_server::Farm`]: the same calls, in the same words, and
//! the text they write. Each method does what the method of the same name of `Farm` does with
//! the call, through the functions of `kbf_server::farm_js::dispatch` where the farm's own
//! work is in them and through the core where the scheduler's is. The read faults of the store
//! (`unreadable`, `readable`) are the tape's, as in the farm's session; the second frontend
//! (`peer start`) is a second core over the same store, with its own clock zero and its own
//! process term.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use js_server_core::{Beat, Core, NodeAction as CoreAction, Stream};
use js_store::{RowCodec as _, Value};
use kbf_caps::NodeCaps;
use kbf_front::{Backend, Described, Submission};
use kbf_log::{Format, LogCommand};
use kbf_proto::worker::{self, ServerMessage, server_message};
use kbf_sched::{DaemonInstance, NodePhase, Request};
use kbf_server::action_usage::Shapes;
use kbf_server::farm::{NodeAction, Reported};
use kbf_server::farm_js::codec::KbfRowCodec;
use kbf_server::farm_js::dispatch;
use kbf_server::farm_js::open;
use kbf_server::farm_js::{Connection, KbfAdapter, Wall};
use kbf_server::{EpochPolicy, PlacementLoad};
use kbf_state::Store;
use kbf_types::sched::usage::LeaseCost;
use kbf_types::{ActionKey, FarmTime, LeaseId, Resources, WorkerId};
use tokio::sync::mpsc;

pub use crate::text::{Ended, Job};
use crate::text::{
    GIB, ended_text, lease_list, lease_text, phase_text, result_of, wire_lease_text,
};
use crate::transcript::{Node, Transcript};

/// The term a new store is seeded with.
const TERM: u64 = 7;
/// The term of the second frontend of the store ([`Session::peer_start`]), which names it in
/// the `owner/` rows apart from the first.
const PEER_TERM: u64 = 8;
/// The wall clock when the core is built, in milliseconds since the Unix epoch.
const WALL_MS: u64 = 1_900_000_000_000;

/// The core, the store under it, and the text of everything the script did.
pub struct Session {
    core: Option<Core<KbfAdapter>>,
    t: Transcript<Stream>,
    shapes: Shapes,
    backend: Backend,
    wall: Wall,
    started: tokio::time::Instant,
    /// A second frontend of the same store, once the script starts one.
    peer: Option<Peer>,
    /// The nodes registered with the second frontend.
    on_peer: BTreeSet<String>,
}

/// A frontend's core, with the wall clock it reads and the moment it was built (its farm time
/// zero).
struct Peer {
    core: Core<KbfAdapter>,
    wall: Wall,
    started: tokio::time::Instant,
}

/// The value of a row as the tape writes it down: a lease row with the cost admission charged
/// the lease taken out, and every other row as it is.
///
/// This is the one difference between the rows of the two drivers. The farm's own driver
/// writes the grant of a `StartLease`, which has no cost; the core writes the grant it
/// committed, which has. The row is read with the codec the core writes it with, the cost is
/// set to the default and the row is written again, so everything else in it (the lease, the
/// operation, the node) is still compared, byte for byte, with the farm's.
///
/// Writing the row again resets the batch the row is wrapped in (its farm time and wall
/// time) to the defaults, so before that the row's batch must already have them; a row that
/// has other times is not normalised, and the call panics naming the row.
///
/// The row is written again by the codec under test, so what that codec writes for a cost
/// of no cost is compared too: a codec that wrote the cost differently (a cpu count one
/// higher, say) writes the default cost as a cost, and every lease row then differs from the
/// farm's, which has none. That is why a change of the cost's encoding turns the scripts
/// red although the cost is set aside: the scripts pin that no cost is left out of the row,
/// as the farm's rows have it. It is not a leak of the cost into another row; the cost is in
/// the lease row only.
///
/// # Panics
/// A lease row whose batch has a farm time or a wall time other than the default.
pub fn without_lease_cost(key: &str, value: &[u8]) -> Vec<u8> {
    let read = KbfRowCodec.read(key, value);
    match read {
        Ok(Value::Lease(mut grant) | Value::LegacyLease(mut grant))
            if key.starts_with("lease/") =>
        {
            assert_default_batch_times(key, value);
            grant.cost = LeaseCost::default();
            KbfRowCodec.lease(&grant).unwrap_or_else(|_| value.to_vec())
        }
        _ => value.to_vec(),
    }
}

/// Panics, naming `key`, unless the batch `value` is wrapped in has the default farm time and
/// the Unix epoch for its wall time.
fn assert_default_batch_times(key: &str, value: &[u8]) {
    let decoded = kbf_log::decode(Format::V1, value);
    let Ok(LogCommand::Farm(batch)) = decoded else {
        panic!("the lease row {key} is not a batch of the scheduler's inputs");
    };
    assert_eq!(
        batch.now,
        FarmTime::default(),
        "the lease row {key}: farm time"
    );
    assert_eq!(batch.wall, UNIX_EPOCH, "the lease row {key}: wall time");
}

/// Milliseconds since `started` on tokio's clock.
fn elapsed_ms(started: tokio::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl Session {
    /// A core over an empty store, as a first start (the store's epoch is the seed).
    pub async fn start(title: &str) -> Self {
        let t = Transcript::new(title, without_lease_cost);
        let backend: Backend = t.cache.clone().into();
        let started = tokio::time::Instant::now();
        let mut session = Self {
            core: None,
            t,
            shapes: Shapes::default(),
            backend,
            wall: wall_from(started),
            started,
            peer: None,
            on_peer: BTreeSet::new(),
        };
        session.core = Some(
            session
                .build(EpochPolicy::Keep)
                .expect("a core over the store"),
        );
        let epoch = session.epoch();
        session.t.say("start");
        session
            .t
            .note(format!("farm term={epoch} lease_epoch={epoch}"));
        session.t.sync().await;
        session
    }

    /// The text, one line per line, ending in a newline.
    pub fn text(&self) -> String {
        self.t.text()
    }

    /// A core over the session's store, that starts now, or why the store refused it, in the
    /// words of the error.
    fn build(&mut self, policy: EpochPolicy) -> Result<Core<KbfAdapter>, String> {
        let built = self.build_as(TERM, policy);
        (self.started, self.wall) = (built.started, built.wall);
        built.core
    }

    /// A core over the session's store for a frontend whose process term is `term`, that
    /// starts now, with its wall clock and its farm time zero.
    fn build_as(&self, term: u64, policy: EpochPolicy) -> Built {
        let started = tokio::time::Instant::now();
        let wall = wall_from(started);
        let store: Arc<dyn Store> = self.t.tape.clone();
        let adapter = KbfAdapter::new(
            term,
            self.backend.clone(),
            Arc::clone(&store),
            Arc::clone(&wall),
        );
        let unservable = kbf_sched::UNSERVABLE_WAIT;
        let retention = kbf_sched::FINISHED_RETENTION;
        let config = open::config(term, unservable, retention, PlacementLoad::Off);
        let now = FarmTime::from_millis(elapsed_ms(started));
        let core = open::open(&config, adapter, store, policy, now).map_err(|e| e.to_string());
        Built {
            core,
            wall,
            started,
        }
    }

    /// The core `node` registered with (the second frontend's if it registered there), its
    /// wall clock and its farm time zero.
    fn frontend_of(&self, node: &str) -> (&Core<KbfAdapter>, &Wall, tokio::time::Instant) {
        match self.peer.as_ref().filter(|_| self.on_peer.contains(node)) {
            Some(peer) => (&peer.core, &peer.wall, peer.started),
            None => (self.core(), &self.wall, self.started),
        }
    }

    /// Starts a second frontend over the same store, as another process would: it takes the
    /// store's epoch, and names itself apart in the `owner/` rows.
    pub async fn peer_start(&mut self) {
        self.t.say("peer start");
        let Built {
            core,
            wall,
            started,
        } = self.build_as(PEER_TERM, EpochPolicy::Keep);
        let core = core.expect("a second core over the store");
        let note = format!("peer lease_epoch={}", core.epoch().expect("a store"));
        self.peer = Some(Peer {
            core,
            wall,
            started,
        });
        self.t.note(note);
        self.t.sync().await;
    }

    /// Registers `name` with the second frontend, as [`Session::register`] does with the first.
    pub async fn peer_register(&mut self, name: &str, cores: u64, gib: u64, drivers: &[&str]) {
        self.on_peer.insert(name.to_owned());
        self.register_with("peer register", name, cores, gib, drivers)
            .await;
    }

    /// The caller `label` goes away (its client cancels): its stage is no longer watched.
    pub async fn cancel(&mut self, label: &str) {
        self.t.say(&format!("cancel {label}"));
        self.t.cancel(label);
        self.t.sync().await;
    }

    fn core(&self) -> &Core<KbfAdapter> {
        self.core.as_ref().expect("the process is running")
    }

    /// The epoch the store was opened under, which is the farm's term and its lease epoch.
    fn epoch(&self) -> u64 {
        self.core().epoch().expect("a core over a store")
    }

    /// Farm time: milliseconds since this core was built.
    fn now(&self) -> FarmTime {
        FarmTime::from_millis(elapsed_ms(self.started))
    }

    /// Registers `name` with `cores` cores and `gib` GiB, running `drivers`.
    pub async fn register(&mut self, name: &str, cores: u64, gib: u64, drivers: &[&str]) {
        self.on_peer.remove(name);
        self.register_with("register", name, cores, gib, drivers)
            .await;
    }

    /// Registers `name` with the frontend it is noted on, writing the call as `verb`.
    async fn register_with(
        &mut self,
        verb: &str,
        name: &str,
        cores: u64,
        gib: u64,
        drivers: &[&str],
    ) {
        self.t.say(&format!(
            "{verb} {name} cores={cores} gib={gib} drivers={}",
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
                epoch: self
                    .frontend_of(name)
                    .0
                    .epoch()
                    .expect("a core over a store"),
                ..Default::default()
            })),
        };
        let (core, _, started) = self.frontend_of(name);
        let now = FarmTime::from_millis(elapsed_ms(started));
        let stream = dispatch::register(
            core,
            now,
            &WorkerId::new(name),
            DaemonInstance::new(format!("daemon-{name}")),
            DaemonInstance::new(""),
            reported,
            Connection { outbound, welcome },
        )
        .expect("a host the core hears");
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
        let (core, _, started) = self.frontend_of(name);
        let now = FarmTime::from_millis(elapsed_ms(started));
        dispatch::resize(core, now, &WorkerId::new(name), stream, reported)
            .expect("a host the core hears");
        self.t.sync().await;
    }

    /// The node's stream ends: what the core sent it is dropped from then on.
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
            needs: kbf_caps::Request::default(),
            whole_worker: false,
            pin: None,
        };
        let described = Described {
            arguments: vec!["/bin/run".to_owned(), job.label.to_owned()],
            outputs: vec![format!("out/{}", job.label)],
            label: Default::default(),
        };
        let submission = Submission {
            request,
            call: None,
            described,
            evidence: Default::default(),
            max_wall: None,
        };
        match dispatch::submit(self.core(), self.now(), &self.shapes, submission) {
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
        match dispatch::wait(self.core(), &name) {
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
        let (core, _, started) = self.frontend_of(name);
        let now = FarmTime::from_millis(elapsed_ms(started));
        let acked = core
            .heartbeat(now, &WorkerId::new(name), stream, Beat::new(seq, leases))
            .await
            .expect("a host the core hears");
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
        let (core, wall, started) = self.frontend_of(name);
        let ack = dispatch::report(
            core,
            &self.backend,
            wall,
            &|| FarmTime::from_millis(elapsed_ms(started)),
            &WorkerId::new(name),
            result_of(lease, digest, label, ended),
        )
        .await
        .expect("a host the core hears");
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

    /// The core's tick.
    pub async fn tick(&mut self) {
        self.t.say("tick");
        self.core().tick(self.now()).await;
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
        let (what, action) = match action {
            NodeAction::Cordon => ("cordon".to_owned(), CoreAction::Cordon),
            NodeAction::Drain(within) => (
                format!("drain {}ms", within.as_millis()),
                CoreAction::Drain(within),
            ),
            NodeAction::Uncordon => ("uncordon".to_owned(), CoreAction::Uncordon),
        };
        self.t.say(&format!("place {name} {what}"));
        match self.core().place(self.now(), &WorkerId::new(name), action) {
            Ok(()) => self.t.note("placed".to_owned()),
            Err(e) => self.t.note(format!("not placed: {e}")),
        }
        self.t.sync().await;
    }

    /// A handover takes the work from this core.
    pub async fn fence(&mut self) {
        self.t.say("fence");
        self.core().fence();
        self.t.sync().await;
    }

    /// The handover did not happen.
    pub async fn resume(&mut self) {
        self.t.say("resume");
        self.core().resume();
        self.t.sync().await;
    }

    /// A drain closes the intake.
    pub async fn close_intake(&mut self) {
        self.t.say("close_intake");
        self.core().close_intake();
        self.t.sync().await;
    }

    /// An operator raises the stored epoch.
    pub async fn bump_epoch(&mut self) {
        self.t.say("bump_epoch");
        match self.core().bump_epoch() {
            Ok(Some(bump)) => self
                .t
                .note(format!("epoch {} -> {}", bump.previous, bump.epoch)),
            Ok(None) => self.t.note("no state store".to_owned()),
            Err(e) => self.t.note(format!("not bumped: {e}")),
        }
        self.t.sync().await;
    }

    /// The process dies and another starts over the same store, with `policy`. The
    /// streams and the callers die with it; the store and the cache stay. A start the store
    /// refuses is written down (`not started:` and the error), and leaves no process running
    /// until the next restart: a script makes no other call before it. A second frontend
    /// ([`Session::peer_start`]) dies with the first, and its nodes are forgotten.
    pub async fn restart(&mut self, policy: EpochPolicy) {
        let how = match policy {
            EpochPolicy::Keep => "keep",
            EpochPolicy::Bump => "bump",
        };
        self.t.say(&format!("restart epoch={how}"));
        drop(self.core.take());
        drop(self.peer.take());
        self.on_peer.clear();
        self.t.restart();
        match self.build(policy) {
            Ok(core) => {
                self.core = Some(core);
                let epoch = self.epoch();
                self.t
                    .note(format!("farm term={epoch} lease_epoch={epoch}"));
            }
            Err(e) => self.t.note(format!("not started: {e}")),
        }
        self.t.sync().await;
    }

    /// An intake poll: what the other frontends of the store did is taken in, and the
    /// callers that asked for an operation by name are shown what the store says of it.
    pub async fn intake(&mut self) {
        self.t.say("intake");
        self.core().intake(self.now());
        self.t.sync().await;
    }

    /// From now on a read of the row at `key` fails, as the backend's failures do.
    pub async fn unreadable(&mut self, key: &str) {
        self.t.say(&format!("unreadable {key}"));
        self.t.tape.unreadable(key);
        self.t.sync().await;
    }

    /// Every row can be read again.
    pub async fn readable(&mut self) {
        self.t.say("readable");
        self.t.tape.readable();
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

/// A core, or why the store refused it, with its wall clock and farm time zero.
struct Built {
    core: Result<Core<KbfAdapter>, String>,
    wall: Wall,
    started: tokio::time::Instant,
}

/// The wall clock of a core built at `started`: `WALL_MS` then, moving with tokio's clock.
fn wall_from(started: tokio::time::Instant) -> Wall {
    Arc::new(move || UNIX_EPOCH + Duration::from_millis(WALL_MS + elapsed_ms(started)))
}
