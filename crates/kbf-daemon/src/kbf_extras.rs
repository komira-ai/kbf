//! A kbf node's answers to the session loop's hooks ([`SessionExtras`]): the storage
//! map, the data plane's pack counters, a driver's report, the Mac session's phase, the
//! container images and the memory backstop. The builders that set them are on
//! [`Daemon`]`<`[`KbfExtras`]`>`, which is what [`Daemon::new`] makes.

use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;
use kbf_proto::worker::{
    self, DaemonMessage, NodeStatus, PackReadCounts, PackWriteCounts, Welcome, daemon_message,
    server_message,
};
use kbf_types::NodePhase;
use tokio::sync::watch;

use crate::core::{LeaseCore, send};
use crate::daemon::Daemon;
use crate::extras::{ExtraWake, SessionExtras};
use crate::map::{MapLink, map_asked};
use crate::packed::{PackCommit, PackCounter};
use crate::pressure;
use crate::report::NodeReport;
use crate::status::{DriverReport, Software};

/// The memory backstop: the sampled pressure, the percentage above which a node that
/// runs leases takes no more, and whether it is paused now.
struct Backstop {
    stall: watch::Receiver<Option<f64>>,
    above: f64,
    paused: bool,
}

/// What a kbf node adds to the lease protocol; see the module documentation.
pub struct KbfExtras {
    /// What `NodeStatus` says about the operating system.
    software: Software,
    /// The driver's newest report, if it sends any; see [`Daemon::with_driver_report`].
    driver: Option<watch::Receiver<DriverReport>>,
    /// Every Xcode the driver's newest report names, ready or not.
    xcodes: Vec<worker::XcodeStatus>,
    /// The container images `NodeStatus` lists; see [`Daemon::with_container_images`].
    container_images: Vec<String>,
    /// The runtime's phase, if one reports it; see [`Daemon::with_phase`].
    phase: Option<watch::Receiver<NodePhase>>,
    /// The newest phase taken from it.
    pub(crate) phase_now: NodePhase,
    /// The memory backstop, if one is set; see [`Daemon::with_memory_backstop`]. Every
    /// Hello carries `phase_now`, or [`NodePhase::MemoryPressure`] in its place while
    /// the node is serving and the backstop has paused it.
    pressure: Option<Backstop>,
    /// Where the storage map of `Welcome` and `MapUpdate` goes, if the daemon routes by
    /// one; see [`Daemon::with_map`].
    map: Option<MapLink>,
    /// The counts of the pack reads of the data plane, if it reads packs; see
    /// [`Daemon::with_pack_reads`].
    pack_reads: Option<PackCounter>,
    /// The handle the frontend's Welcome turns pack writes on and off with, if the data plane
    /// writes packs; see [`Daemon::with_pack_writes`].
    pack_commit: Option<PackCommit>,
}

impl KbfExtras {
    /// A node with none of the extras set, its software detected ([`Software::detect`]).
    pub(crate) fn detect() -> Self {
        Self {
            software: Software::detect(),
            driver: None,
            xcodes: Vec::new(),
            container_images: Vec::new(),
            phase: None,
            phase_now: NodePhase::Serving,
            pressure: None,
            map: None,
            pack_reads: None,
            pack_commit: None,
        }
    }

    /// The counts of the pack writes for a Heartbeat: those of the counter
    /// [`Daemon::with_pack_reads`] was given, and only for a daemon that also writes packs
    /// ([`Daemon::with_pack_writes`]).
    fn pack_writes_counted(&self) -> Option<PackWriteCounts> {
        self.pack_commit.as_ref()?;
        self.pack_reads.as_ref().map(|c| c.stats().writes.into())
    }

    /// Takes the map a `Welcome` carried, the first of a new stream: the generation the
    /// holder waits for is asked for again on this stream, by [`Self::after_wake`]. These
    /// are methods rather than code in the session so that none of it is conditional
    /// there (the session is compiled once for each test binary, and the coverage tool
    /// counts the lines of the copy that covers most).
    fn new_map_stream(&mut self, map: Option<kbf_proto::kbf::map::v1::Map>) {
        if let Some(link) = self.map.as_mut() {
            link.new_stream();
            link.take(map);
        }
    }
}

impl SessionExtras for KbfExtras {
    fn welcome_checked(&mut self, welcome: &Welcome) {
        if let Some(commit) = &self.pack_commit {
            commit.welcomed(welcome.pack_commit);
        }
    }

    fn on_welcome(&mut self, welcome: Welcome) {
        self.new_map_stream(welcome.map);
    }

    fn on_server_message(&mut self, message: &server_message::Message) -> bool {
        let server_message::Message::MapUpdate(update) = message else {
            return false;
        };
        if let Some(link) = &self.map {
            link.take(update.map.clone());
        }
        true
    }

    fn pack_counts(&self) -> (Option<PackReadCounts>, Option<PackWriteCounts>) {
        (
            self.pack_reads.as_ref().map(|c| c.stats().into()),
            self.pack_writes_counted(),
        )
    }

    /// The `NodeStatus` this node sends now: its software, every Xcode the driver's
    /// newest report names, ready or not, and the container images it was given.
    fn node_status(&self, report: &NodeReport) -> NodeStatus {
        NodeStatus {
            xcodes: self.xcodes.clone(),
            container_images: self.container_images.clone(),
            ..self.software.status(report)
        }
    }

    /// The runtime's phase, or memory pressure in place of serving while the backstop has
    /// the node paused.
    fn hello_phase(&self, draining: bool) -> NodePhase {
        if draining && self.phase_now == NodePhase::Serving {
            return NodePhase::Draining;
        }
        match &self.pressure {
            Some(backstop) if backstop.paused && self.phase_now == NodePhase::Serving => {
                NodePhase::MemoryPressure
            }
            _ => self.phase_now.clone(),
        }
    }

    fn epoch_changed(&mut self, epoch: Option<u64>, core: &mut LeaseCore) {
        if let Some(lease) = self.phase_now.switching()
            && epoch.is_some_and(|epoch| epoch != lease.term)
        {
            // The server that granted it is gone: no Start for it will come again.
            tracing::warn!(%lease, ?epoch, "the switching lease is of an earlier epoch");
            core.leases.forget(lease);
        }
    }

    async fn wake(&mut self) -> ExtraWake {
        tokio::select! {
            () = changed_or_never(self.driver.as_mut()) => ExtraWake::Driver,
            () = changed_or_never(self.phase.as_mut()) => ExtraWake::Phase,
            () = changed_or_never(self.pressure.as_mut().map(|b| &mut b.stall)) => {
                ExtraWake::Pressure
            }
            () = map_asked(self.map.as_mut()) => ExtraWake::MapAsked,
        }
    }

    /// Sends the `MapRequest` for a generation the holder waits for, once on a stream.
    fn after_wake(&mut self, tx: &UnboundedSender<DaemonMessage>) {
        if let Some(request) = self.map.as_mut().and_then(MapLink::request) {
            send(tx, daemon_message::Message::MapRequest(request));
        }
    }

    fn take_driver(&mut self, base: &NodeReport, report: &mut NodeReport) -> bool {
        let Some(driver) = &mut self.driver else {
            return false;
        };
        let newest = driver.borrow_and_update().clone();
        let newest_report = base.clone().with_entries(newest.entries);
        self.xcodes = newest.xcodes;
        let changed = newest_report != *report;
        *report = newest_report;
        changed
    }

    fn take_phase(&mut self) -> bool {
        let Some(phase) = &mut self.phase else {
            return false;
        };
        let newest = phase.borrow_and_update().clone();
        let changed = newest != self.phase_now;
        self.phase_now = newest;
        changed
    }

    fn take_pressure(&mut self, running: usize, draining: bool) -> bool {
        let before = self.hello_phase(draining);
        let Some(backstop) = &mut self.pressure else {
            return false;
        };
        let stall = *backstop.stall.borrow_and_update();
        backstop.paused = pressure::paused(stall, backstop.above, running);
        self.hello_phase(draining) != before
    }
}

/// Completes when `changes` has a value the daemon has not taken; never without one, or
/// once its sender is gone.
async fn changed_or_never<T>(changes: Option<&mut watch::Receiver<T>>) {
    if let Some(changes) = changes
        && changes.changed().await.is_ok()
    {
        return;
    }
    std::future::pending::<()>().await;
}

impl Daemon<KbfExtras> {
    /// Puts the storage map the server sends (in `Welcome`, and in each `MapUpdate`) in
    /// `holder`, which keeps the newest and never goes back, and asks the server for a
    /// map when the holder is waiting for a generation it does not have
    /// ([`kbf_mapsync::MapHolder::reach`]). Without one the daemon ignores the map.
    #[must_use]
    pub fn with_map(mut self, holder: Arc<kbf_mapsync::MapHolder>) -> Self {
        self.extras.map = Some(MapLink::new(holder));
        self
    }

    /// Puts the counts of the pack reads `counter` holds in every Heartbeat
    /// (`Heartbeat.pack_reads`), as they stand when it is sent: the totals since this
    /// process started, which the server shows per node. Without it a Heartbeat carries
    /// none, as before the field existed.
    #[must_use]
    pub fn with_pack_reads(mut self, counter: PackCounter) -> Self {
        self.extras.pack_reads = Some(counter);
        self
    }

    /// Sets `commit` from every Welcome (`Welcome.pack_commit`): the data plane that writes packs
    /// ([`crate::PackedCas::with_writes`], given the same handle) writes them only while the
    /// frontend says it takes commits. Given the counter of [`Self::with_pack_reads`] too, the
    /// daemon also puts the counts of its pack writes in every Heartbeat
    /// (`Heartbeat.pack_writes`). Without it a Welcome sets nothing and a Heartbeat carries no
    /// write counts.
    #[must_use]
    pub fn with_pack_writes(mut self, commit: PackCommit) -> Self {
        self.extras.pack_commit = Some(commit);
        self
    }

    /// The counter [`Self::with_pack_reads`] was given; `None` for a daemon whose
    /// Heartbeats carry no counts.
    #[must_use]
    pub const fn pack_reads(&self) -> Option<&PackCounter> {
        self.extras.pack_reads.as_ref()
    }

    /// The handle [`Self::with_pack_writes`] was given; `None` for a daemon that sets none from
    /// its Welcomes.
    #[must_use]
    pub const fn pack_writes(&self) -> Option<&PackCommit> {
        self.extras.pack_commit.as_ref()
    }

    /// Adds `driver`'s entries to the report and its Xcodes to `NodeStatus`, now and
    /// each time it changes (see the module documentation of [`crate::daemon`]).
    #[must_use]
    pub fn with_driver_report(mut self, driver: watch::Receiver<DriverReport>) -> Self {
        self.extras.driver = Some(driver);
        self.take_driver();
        self
    }

    /// Sends `phase`'s value in every Hello, and resends the Hello on the stream each
    /// time it changes (see the module documentation of [`crate::daemon`]).
    #[must_use]
    pub fn with_phase(mut self, phase: watch::Receiver<NodePhase>) -> Self {
        self.extras.phase = Some(phase);
        self.take_phase();
        self
    }

    /// Stops taking new leases while `stall`, the node's memory stall time as a
    /// percentage ([`crate::pressure::watch`]), is above `above` and a lease runs: the
    /// Hello says [`NodePhase::MemoryPressure`] then, and resends when it changes (see
    /// [`crate::pressure`]). Leases already running are not touched. A runtime's own
    /// phase (a session switch, a quarantine) takes the place of this one.
    #[must_use]
    pub fn with_memory_backstop(mut self, stall: watch::Receiver<Option<f64>>, above: f64) -> Self {
        self.extras.pressure = Some(Backstop {
            stall,
            above,
            paused: false,
        });
        self.take_pressure();
        self
    }

    /// The percentage of memory stall time above which the backstop pauses this node
    /// while it runs a lease; `None` without one ([`Self::with_memory_backstop`]).
    #[must_use]
    pub fn memory_backstop(&self) -> Option<f64> {
        self.extras.pressure.as_ref().map(|backstop| backstop.above)
    }

    /// Lists `images` in every `NodeStatus`, sorted and each once: the container
    /// images the driver checked present at start. Status only; they route no work.
    #[must_use]
    pub fn with_container_images(mut self, images: impl IntoIterator<Item = String>) -> Self {
        let images: std::collections::BTreeSet<String> = images.into_iter().collect();
        self.extras.container_images = images.into_iter().collect();
        self
    }
}
