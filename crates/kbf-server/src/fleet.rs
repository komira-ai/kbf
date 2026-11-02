//! What the server knows about each node for operators: the fleet view behind
//! `GET /v1/nodes` ([`crate::api`]), and where each node is in placement (serving,
//! cordoned, draining; see `kbf_sched::Cordon`).
//!
//! The software a node runs arrives in `NodeStatus` (`docs/design/fleet-updates.md`
//! section 3.1). The server keeps the newest one per node, from the node's current
//! stream only, **in memory**: like the scheduler's state it is gone after a restart,
//! and each daemon sends it again after its next `Welcome`.
//!
//! **Attention.** What a node needs a human for is listed in its `needs_attention`:
//! each installed Xcode that is not ready, with why and the command that fixes it
//! (issue #164), and a quarantine its daemon reported (`quarantined: <why>`, its
//! `phase` too; [`quarantine_change`] logs it once raised and once cleared, at the
//! levels below). An Xcode its node has not surveyed yet (`not_surveyed`: a daemon
//! says Hello before its first survey ends, so a restarted one first sends every
//! Xcode in that state) is listed in `xcodes` with that state, and **holds the item
//! the same app had in the node's previous status**: that item stays in
//! `needs_attention`, and is neither cleared nor raised again until a survey reports
//! the app (an app with no previous item holds none: nothing for a human to do yet).
//! kbf has no alert delivery yet, so the server also logs each item once, at `WARN`
//! under the target `kbf_server::attention`, when it first appears, and at `INFO` when
//! it clears ([`attention_changes`]); a status that repeats the same items, or that
//! lists them not surveyed yet (each new stream sends one), logs nothing. An item is
//! the same while its Xcode's app, build, state and fix are: a reason that changes
//! alone (an NSLog line's time and pid) is shown in `needs_attention` but not logged
//! again. The previous status is the one this server process received last: after a
//! server restart, an item is raised again when its node's survey reports it.

use kbf_proto::worker::{NodeStatus, XcodeState, XcodeStatus};
use kbf_sched::NodePhase;
use kbf_types::{Resources, WorkerId};
use serde::Serialize;

use crate::machine::FarmMachine;

/// One node, as `GET /v1/nodes` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodeView {
    /// The node id its daemon registered with.
    pub node_id: String,
    /// Whether its newest stream is still open.
    pub connected: bool,
    /// The `kbf.worker.v1` version its newest stream's `Welcome` named (the lesser of
    /// its first `Hello`'s and the server's newest); `None` if no version is known. It is kept after the stream closes, as the version the node last
    /// spoke.
    pub protocol: Option<u32>,
    /// The newest software status it sent; `None` if it sent none (a daemon that
    /// predates `NodeStatus`).
    pub software: Option<SoftwareView>,
    /// What an operator must do on the node, one line each
    /// ([`SoftwareView::needs_attention`]).
    pub needs_attention: Vec<String>,
    /// Whether placement may use it, and where its drain is.
    pub placement: PlacementView,
    /// What its newest `Hello` says it is doing besides running leases.
    pub phase: PhaseView,
    /// How many leases it holds (granted or running) now.
    pub lease_count: u32,
    /// What those leases book of its capacity.
    pub booked: ResourcesView,
}

/// A resource vector (`kbf_types::Resources`) as the operator API writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ResourcesView {
    /// CPU in thousandths of a core.
    pub cpu_millis: u64,
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// Whole GPUs.
    pub gpus: u64,
    /// VM slots.
    pub vms: u64,
}

impl ResourcesView {
    /// The view of `r`.
    #[must_use]
    pub const fn of(r: Resources) -> Self {
        Self {
            cpu_millis: r.cpu_millis,
            memory_bytes: r.memory_bytes,
            gpus: r.gpus,
            vms: r.vms,
        }
    }
}

/// One node as `GET /v1/nodes/{id}` answers: its entry of the list, and what the list
/// leaves out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodeDetail {
    /// Every field of the list entry.
    #[serde(flatten)]
    pub node: NodeView,
    /// What its newest `Hello` says it can run at once.
    pub capacity: ResourcesView,
    /// What is left of `capacity` after `booked`, never below zero.
    pub free: ResourcesView,
    /// The leases it holds, as `term.seq`, in lease order.
    pub leases: Vec<String>,
    /// What its daemon reports of the CPU and memory its actions use. Left out when the
    /// node has sent no reading this frontend accepted (a daemon or runtime that takes
    /// none, or a node another frontend holds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load: Option<LoadView>,
    /// What the node's input cache has done since its daemon process started, from the
    /// newest heartbeat that carried it. Left out when the node sent none (a daemon with its
    /// cache off or built before the counts, or a node another frontend holds): absent is
    /// not zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_cache: Option<InputCacheView>,
    /// Who placed the node out of placement, when it is (cordoned, draining or drained).
    /// Left out when the node is serving. A cordon with no recorded owner, which is every
    /// cordon today, is the operator's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<Owner>,
}

/// Who owns a node's cordon, as the state store keeps it (`cordon-owner/<node>`, see
/// `farm::cordon_owner`). Shown as `operator` or `updater:<unit>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Owner {
    /// An operator placed the node, or nothing records who did.
    #[default]
    Operator,
    /// The host updater placed the node, for the update of the named unit.
    Updater(String),
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Operator => f.write_str("operator"),
            Self::Updater(unit) => write!(f, "updater:{unit}"),
        }
    }
}

impl Serialize for Owner {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The counts of a node's input cache (`docs/api.md`, "Input cache counts"): cumulative since
/// its daemon process started, so they start again from 0 when the daemon restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct InputCacheView {
    /// Entries written into the cache from the content store, and their bytes.
    pub fills: u64,
    pub fill_bytes: u64,
    /// Entries a lease found in the cache and used, and their bytes.
    pub hits: u64,
    pub hit_bytes: u64,
    /// Entries evicted for the byte budget or the free-space floor, and their bytes.
    pub evictions: u64,
    pub evicted_bytes: u64,
    /// The shared tree store's counts. Left out when the node has no store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trees: Option<TreeCountsView>,
}

/// The counts of a node's shared tree store, cumulative since its daemon process started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TreeCountsView {
    pub builds: u64,
    pub hits: u64,
    pub evictions: u64,
}

impl From<&kbf_proto::worker::InputCacheCounts> for InputCacheView {
    fn from(counts: &kbf_proto::worker::InputCacheCounts) -> Self {
        Self {
            fills: counts.fills,
            fill_bytes: counts.fill_bytes,
            hits: counts.hits,
            hit_bytes: counts.hit_bytes,
            evictions: counts.evictions,
            evicted_bytes: counts.evicted_bytes,
            trees: counts.trees.as_ref().map(|trees| TreeCountsView {
                builds: trees.builds,
                hits: trees.hits,
                evictions: trees.evictions,
            }),
        }
    }
}

/// The load of a node's `actions/` cgroup, from the readings its heartbeats carried
/// (`docs/api.md`, "Node load"). The scheduler is handed each accepted reading as well;
/// whether it places by them is `--placement-load`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LoadView {
    /// The CPU the actions used, in thousandths of a CPU: the larger of the last two
    /// windows between accepted readings. `null` with fewer than two windows, and when
    /// `stale`.
    pub measured_cpu_millicores: Option<u64>,
    /// `memory.current` less the inactive file pages, in bytes, at the last reading.
    pub memory_working_set_bytes: u64,
    /// The active file pages of the cgroup, in bytes, at the last reading.
    pub memory_active_file_bytes: u64,
    /// The working set less the active file pages beyond a quarter of the node's memory:
    /// what an admission would count. Never above the working set.
    pub admission_working_set_bytes: u64,
    /// The leases that ran for the whole window before the last reading, as `term.seq`.
    pub covered: Vec<String>,
    /// Whether the cgroup has a CPU quota.
    pub actions_cpu_limited: bool,
    /// Cumulative CPU throttled time, in microseconds, when the daemon read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_throttled_usec: Option<u64>,
    /// Cumulative CPU pressure (`some` total), in microseconds, when the daemon read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_pressure_some_total_usec: Option<u64>,
    /// What the lists of the readings carried, and what was not taken.
    pub lists: ListsView,
    /// Whether no reading was accepted for three windows.
    pub stale: bool,
}

/// What the `lease_clocks` and `minutes` lists of a node's readings have come to since its
/// daemon process began (`docs/api.md`, "Node load"), so that a dropped record, or a feed
/// that is empty, can be seen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ListsView {
    /// Clock records kept, summed over the accepted readings.
    pub clocks: u64,
    /// Minute records kept, summed over the accepted readings.
    pub minutes: u64,
    /// Records dropped, from the three lists (`covered` included), by reason.
    pub dropped: DroppedView,
    /// Minute records carried by readings that were not accepted.
    pub refused_minutes: u64,
}

/// Why records of a reading's lists were dropped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DroppedView {
    /// A rule was broken: no lease, a minute above 63, a working set above 2^45 bytes, a mean
    /// above its peak, a start after the reading.
    pub invalid: u64,
    /// The record repeated an earlier kept one.
    pub duplicate: u64,
    /// The record was past the cap of its list.
    pub over_cap: u64,
}

/// A node's phase (`kbf_sched::NodePhase`). Serialized with its `state` as a tag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PhaseView {
    /// It takes leases.
    Serving,
    /// It switches its GUI session to the user of this whole-machine lease.
    Preparing {
        /// The lease, as `term.seq`.
        lease: String,
    },
    /// It switches the session back after this whole-machine lease, and cleans up.
    Restoring {
        /// The lease, as `term.seq`.
        lease: String,
    },
    /// A cleanup failed: it takes no lease until an operator repairs it.
    Quarantined {
        /// Why, as the node said.
        reason: String,
    },
    /// Its memory is under pressure: it takes no new lease until that falls.
    MemoryPressure,
    /// The daemon was told to stop: it takes no new lease and finishes the ones it runs.
    Draining,
}

impl PhaseView {
    /// The view of `phase`.
    #[must_use]
    pub fn new(phase: &NodePhase) -> Self {
        match phase {
            NodePhase::Serving => Self::Serving,
            NodePhase::Preparing(lease) => Self::Preparing {
                lease: lease.to_string(),
            },
            NodePhase::Restoring(lease) => Self::Restoring {
                lease: lease.to_string(),
            },
            NodePhase::Quarantined(reason) => Self::Quarantined {
                reason: reason.clone(),
            },
            NodePhase::MemoryPressure => Self::MemoryPressure,
            NodePhase::Draining => Self::Draining,
        }
    }
}

/// What an operator must do on a node: each item of its newest software status, then
/// its quarantine, if its phase is one (`quarantined: <why>`).
#[must_use]
pub fn needs_attention(software: Option<&SoftwareView>, phase: &NodePhase) -> Vec<String> {
    let mut items = software
        .map(SoftwareView::needs_attention)
        .unwrap_or_default();
    if let NodePhase::Quarantined(reason) = phase {
        items.push(format!("quarantined: {reason}"));
    }
    items
}

/// What to log when node `node`'s phase goes from `before` to `after`: its quarantine
/// raised (`true`, with why) or cleared (`false`), each once. A quarantine whose reason
/// changes is raised again; any other change logs nothing.
#[must_use]
pub fn quarantine_change(
    node: &str,
    before: &NodePhase,
    after: &NodePhase,
) -> Option<(bool, String)> {
    match (before, after) {
        (before, NodePhase::Quarantined(reason)) if before != after => {
            Some((true, format!("node {node}: quarantined: {reason}")))
        }
        (NodePhase::Quarantined(_), after) if !matches!(after, NodePhase::Quarantined(_)) => {
            Some((false, format!("node {node}: no longer quarantined")))
        }
        _ => None,
    }
}

/// The deadline a drain is shown with: `unix_ms` as it is, or `None` for a drain with no
/// deadline. A deadline of half the range of a `u64` of milliseconds (about 292 million years)
/// or more counts as none: it is what [`NO_DRAIN_DEADLINE`](crate::api::NO_DRAIN_DEADLINE)
/// becomes (saturated, or less the store's clock when another frontend applies the stored
/// row), and no operator's deadline comes near it.
#[must_use]
pub(crate) fn shown_deadline(unix_ms: u64) -> Option<u64> {
    (unix_ms < u64::MAX / 2).then_some(unix_ms)
}

/// Where a node is in placement. Serialized with its `state` as a tag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PlacementView {
    /// Placement may offer it leases.
    Serving,
    /// An operator cordoned it: no new lease; its leases run on.
    Cordoned,
    /// Cordoned, and its leases are waited for until the deadline.
    Draining {
        /// When the drain pauses if leases still run: milliseconds since the Unix
        /// epoch, server clock. Left out for a drain with no deadline.
        #[serde(skip_serializing_if = "Option::is_none")]
        deadline_unix_ms: Option<u64>,
        /// The leases it still holds, as `term.seq`.
        leases: Vec<String>,
    },
    /// Cordoned, and it holds no lease: it may be taken out of service.
    Drained,
    /// The deadline passed while leases still ran. They run on; nothing proceeds
    /// until an operator drains it again or uncordons it.
    DrainPaused {
        /// The deadline that passed (milliseconds since the Unix epoch). Left out for a
        /// drain that had none.
        #[serde(skip_serializing_if = "Option::is_none")]
        deadline_unix_ms: Option<u64>,
        /// The leases it still holds, as `term.seq`.
        leases: Vec<String>,
    },
}

/// A node's newest `NodeStatus`, and when the server received it. An empty string or
/// list is a value the node could not read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SoftwareView {
    /// `macOS`, or the Linux `os-release` `NAME`.
    pub os_name: String,
    /// The OS version.
    pub os_version: String,
    /// The OS build (Mac; Linux where `os-release` has a `BUILD_ID`).
    pub os_build: String,
    /// The kernel release (Linux).
    pub kernel: String,
    /// The `kbf-daemon` version.
    pub daemon_version: String,
    /// The Xcode builds actions can use on it (Mac).
    pub xcode_builds: Vec<String>,
    /// Every installed Xcode, ready or not (Mac; empty from a daemon that predates it).
    pub xcodes: Vec<XcodeView>,
    /// The container images the node checked present at its start (`--image`),
    /// sorted. Status only: no work is placed by image.
    pub container_images: Vec<String>,
    /// When the server received it: milliseconds since the Unix epoch, server clock.
    pub received_at_unix_ms: u64,
}

/// One installed Xcode, as its node last reported it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct XcodeView {
    /// The app (`/Applications/Xcode_16.2.app`).
    pub app: String,
    /// Its build; empty when not known.
    pub build: String,
    /// `ready`, `license_not_accepted`, `first_launch_not_run`,
    /// `metal_toolchain_missing`, `failed`, `not_surveyed` (found, not asked yet; never
    /// advertised), or `unknown` (a state this server does not know).
    pub state: &'static str,
    /// Why it is not ready; empty when ready.
    pub reason: String,
    /// The command that makes it ready; empty when ready or none is known.
    pub fix: String,
    /// While it is not surveyed yet: the same app as the node's previous status
    /// counted it, if that had an item ([`SoftwareView::hold_unsurveyed`]). Not
    /// listed: `state` says what the node sent.
    #[serde(skip)]
    held: Option<Box<XcodeView>>,
}

impl XcodeView {
    fn new(status: XcodeStatus) -> Self {
        let state = match XcodeState::try_from(status.state) {
            Ok(XcodeState::Ready) => "ready",
            Ok(XcodeState::LicenseNotAccepted) => "license_not_accepted",
            Ok(XcodeState::FirstLaunchNotRun) => "first_launch_not_run",
            Ok(XcodeState::MetalToolchainMissing) => "metal_toolchain_missing",
            Ok(XcodeState::Failed) => "failed",
            Ok(XcodeState::NotSurveyed) => "not_surveyed",
            Ok(XcodeState::Unspecified) | Err(_) => "unknown",
        };
        Self {
            app: status.app,
            build: status.build,
            state,
            reason: status.reason,
            fix: status.fix,
            held: None,
        }
    }

    /// What its attention item is judged by: what it holds while not surveyed yet,
    /// else itself.
    fn counted(&self) -> &Self {
        self.held.as_deref().unwrap_or(self)
    }

    /// What an operator must do about it, unless it is ready or not surveyed yet.
    fn attention(&self) -> Option<String> {
        if matches!(self.state, "ready" | "not_surveyed") {
            return None;
        }
        let named = match self.build.as_str() {
            "" => format!("Xcode ({})", self.app),
            build => format!("Xcode {build} ({})", self.app),
        };
        let fix = match self.fix.as_str() {
            "" => "none known",
            fix => fix,
        };
        Some(format!(
            "{named} installed but not ready: {}; fix: {fix}",
            self.reason
        ))
    }
}

impl SoftwareView {
    /// `status`, received at `received_at_unix_ms`.
    #[must_use]
    pub fn new(status: NodeStatus, received_at_unix_ms: u64) -> Self {
        Self {
            os_name: status.os_name,
            os_version: status.os_version,
            os_build: status.os_build,
            kernel: status.kernel,
            daemon_version: status.daemon_version,
            xcode_builds: status.xcode_builds,
            xcodes: status.xcodes.into_iter().map(XcodeView::new).collect(),
            container_images: status.container_images,
            received_at_unix_ms,
        }
    }

    /// Makes each Xcode not surveyed yet hold the item the same app has in `before`
    /// (the node's previous status), so that it keeps it until a survey reports the
    /// app (see the module documentation).
    pub fn hold_unsurveyed(&mut self, before: &[XcodeView]) {
        hold(before, &mut self.xcodes);
    }

    /// What an operator must do on the node, one line per Xcode that is not ready, or
    /// not surveyed yet and holding an item:
    /// `Xcode <build> (<app>) installed but not ready: <reason>; fix: <command>`.
    #[must_use]
    pub fn needs_attention(&self) -> Vec<String> {
        self.xcodes
            .iter()
            .filter_map(|x| x.counted().attention())
            .collect()
    }
}

/// [`SoftwareView::hold_unsurveyed`] on `after`.
fn hold(before: &[XcodeView], after: &mut [XcodeView]) {
    for x in after
        .iter_mut()
        .filter(|x| x.state == "not_surveyed" && x.held.is_none())
    {
        x.held = before
            .iter()
            .find(|b| b.app == x.app)
            .map(XcodeView::counted)
            .filter(|b| b.attention().is_some())
            .map(|b| Box::new(b.clone()));
    }
}

/// What to log when a node's Xcodes go from `before` to `after`: the attention item of
/// each Xcode newly not ready (`true`, at `WARN`), then that of each no longer so
/// (`false`, at `INFO`), as `node <node>: <item>` and `node <node>: resolved: <item>`.
/// An Xcode is compared by its app, build, state and fix, not its reason, and one not
/// surveyed yet in `after` by the item it holds from `before` (see the module
/// documentation), so the same Xcodes, or the same not surveyed yet, give nothing.
#[must_use]
pub fn attention_changes(
    node: &str,
    before: &[XcodeView],
    after: &[XcodeView],
) -> Vec<(bool, String)> {
    type Key<'a> = (&'a str, &'a str, &'a str, &'a str);
    fn items(list: &[XcodeView]) -> Vec<(Key<'_>, String)> {
        list.iter()
            .filter_map(|x| {
                let x = x.counted();
                let key = (x.app.as_str(), x.build.as_str(), x.state, x.fix.as_str());
                Some((key, x.attention()?))
            })
            .collect()
    }
    let mut held = after.to_vec();
    hold(before, &mut held);
    let (before, after) = (items(before), items(&held));
    let has = |list: &[(Key<'_>, String)], key: &Key<'_>| list.iter().any(|(k, _)| k == key);
    let raised = after
        .iter()
        .filter(|(key, _)| !has(&before, key))
        .map(|(_, item)| (true, format!("node {node}: {item}")));
    let cleared = before
        .iter()
        .filter(|(key, _)| !has(&after, key))
        .map(|(_, item)| (false, format!("node {node}: resolved: {item}")));
    raised.chain(cleared).collect()
}

/// Keeps `status`, a `NodeStatus` of `worker` received at `received` (milliseconds since the
/// Unix epoch), as the machine's newest of the node, and returns the attention items it
/// raises or clears against the node's previous status, `true` for each raised; each is
/// logged once, at `WARN` for a raise and `INFO` for a clear, under the target
/// `kbf_server::attention`. An Xcode the status lists not surveyed yet keeps its previous
/// item (neither cleared now nor raised again by the survey). Both drivers of the farm take
/// a node's status through this.
pub fn take_node_status(
    machine: &mut FarmMachine,
    worker: &WorkerId,
    status: NodeStatus,
    received: u64,
) -> Vec<(bool, String)> {
    let mut view = SoftwareView::new(status, received);
    let before = machine
        .node(worker)
        .and_then(|n| n.status.as_ref())
        .map_or(&[][..], |s| &s.xcodes);
    view.hold_unsurveyed(before);
    let changes = attention_changes(worker.as_str(), before, &view.xcodes);
    for (raise, line) in &changes {
        if *raise {
            tracing::warn!(target: "kbf_server::attention", "{line}");
        } else {
            tracing::info!(target: "kbf_server::attention", "{line}");
        }
    }
    machine.report_status(worker, view);
    changes
}

/// The body of `GET /v1/nodes`: the server that answers, and every node registered
/// since it started, in node-id order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodesView {
    /// The server process that answers.
    pub server: ServerView,
    /// The nodes.
    pub nodes: Vec<NodeView>,
    /// The oldest protocol version among them, and how many name none.
    #[serde(flatten)]
    pub protocols: ProtocolSummary,
}

impl NodesView {
    /// `nodes`, as this server build lists them. The protocol summary covers every node
    /// listed, connected or not: one whose stream closed may come back on the version it
    /// last spoke.
    #[must_use]
    pub fn of_this_build(nodes: Vec<NodeView>) -> Self {
        Self {
            server: ServerView::this_build(),
            protocols: ProtocolSummary::of(nodes.iter().map(|node| node.protocol)),
            nodes,
        }
    }
}

/// What the protocol versions of a list of nodes come to, as the envelope of
/// `GET /v1/nodes` and of `GET /v1/members` states it: the input of the check that a
/// release may stop accepting the oldest version (`docs/design/frontend-proxy.md`
/// section 9, rule R1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ProtocolSummary {
    /// The least version among the nodes whose version is known; `None` when no node's
    /// is. A list with an unknown version in it says nothing about that node here, so
    /// the number alone is not "every node speaks at least this": read it with
    /// `protocol_unknown`.
    pub oldest_protocol: Option<u32>,
    /// How many nodes name no version. They are never counted in `oldest_protocol`.
    pub protocol_unknown: u32,
}

impl ProtocolSummary {
    /// The summary of nodes that speak `protocols` (`None`: names none).
    #[must_use]
    pub fn of(protocols: impl IntoIterator<Item = Option<u32>>) -> Self {
        let (mut oldest, mut unknown) = (None::<u32>, 0_u32);
        for protocol in protocols {
            match protocol {
                // Not `oldest.min(protocol)`: `None` is the least `Option`.
                Some(version) => oldest = Some(oldest.map_or(version, |o| o.min(version))),
                None => unknown = unknown.saturating_add(1),
            }
        }
        Self {
            oldest_protocol: oldest,
            protocol_unknown: unknown,
        }
    }
}

/// The `kbf-server` build that answers, so a deploy can check which commit runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServerView {
    /// As `--version` and the start line print it: [`crate::SERVER_VERSION`].
    pub version: String,
    /// The commit alone: [`crate::BUILD_COMMIT`].
    pub commit: String,
}

impl ServerView {
    /// This build.
    #[must_use]
    pub fn this_build() -> Self {
        Self {
            version: crate::SERVER_VERSION.to_owned(),
            commit: crate::BUILD_COMMIT.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xcode(build: &str, state: XcodeState, reason: &str, fix: &str) -> XcodeStatus {
        XcodeStatus {
            app: format!("/Applications/Xcode_{build}.app"),
            build: build.to_owned(),
            state: state.into(),
            reason: reason.to_owned(),
            fix: fix.to_owned(),
        }
    }

    /// Catches: a not-ready Xcode not listed for attention or listed without its reason
    /// or fix, a ready one or one not surveyed yet listed (nothing for a human to do),
    /// a state shown under another name, a state number this server does not know
    /// shown as ready, and an unknown build or fix left blank.
    #[test]
    fn every_xcode_not_ready_needs_attention() {
        let status = NodeStatus {
            xcodes: vec![
                xcode("16C5032a", XcodeState::Ready, "", ""),
                xcode(
                    "16B40",
                    XcodeState::LicenseNotAccepted,
                    "69",
                    "sudo x -license accept",
                ),
                xcode(
                    "16G1",
                    XcodeState::FirstLaunchNotRun,
                    "69",
                    "sudo x -runFirstLaunch",
                ),
                xcode(
                    "26A1",
                    XcodeState::MetalToolchainMissing,
                    "uninstalled",
                    "x -d",
                ),
                xcode("", XcodeState::Failed, "no build", ""),
                XcodeStatus {
                    state: 99,
                    ..xcode("9Z", XcodeState::Ready, "new", "")
                },
                xcode("8Y", XcodeState::Unspecified, "", ""),
                xcode("", XcodeState::NotSurveyed, "not surveyed yet", ""),
            ],
            ..NodeStatus::default()
        };
        let view = SoftwareView::new(status, 7);
        let states: Vec<&str> = view.xcodes.iter().map(|x| x.state).collect();
        assert_eq!(
            states,
            [
                "ready",
                "license_not_accepted",
                "first_launch_not_run",
                "metal_toolchain_missing",
                "failed",
                "unknown",
                "unknown",
                "not_surveyed"
            ]
        );
        assert_eq!(
            view.needs_attention(),
            [
                "Xcode 16B40 (/Applications/Xcode_16B40.app) installed but not ready: 69; \
                 fix: sudo x -license accept",
                "Xcode 16G1 (/Applications/Xcode_16G1.app) installed but not ready: 69; \
                 fix: sudo x -runFirstLaunch",
                "Xcode 26A1 (/Applications/Xcode_26A1.app) installed but not ready: \
                 uninstalled; fix: x -d",
                "Xcode (/Applications/Xcode_.app) installed but not ready: no build; \
                 fix: none known",
                "Xcode 9Z (/Applications/Xcode_9Z.app) installed but not ready: new; \
                 fix: none known",
                "Xcode 8Y (/Applications/Xcode_8Y.app) installed but not ready: ; \
                 fix: none known",
            ]
        );
    }

    /// Catches: an item raised again by a status that repeats it (every new stream
    /// sends one), or by one whose only difference is the reason (an NSLog line's time
    /// and pid, which `xcodebuild` prints anew each time it is asked); an item never
    /// raised or never cleared, one raised again when its state or build changed not
    /// raised, a ready Xcode raised, and a line that does not name the node.
    #[test]
    fn attention_is_raised_and_cleared_once() {
        let view = |build: &str, state: XcodeState, reason: &str| {
            XcodeView::new(xcode(build, state, reason, "sudo x -license accept"))
        };
        let licence = XcodeState::LicenseNotAccepted;
        let a = view(
            "1A",
            licence,
            "2026-10-09 12:00:01.123 xcodebuild[4321:9876] 69",
        );
        let restamped = view(
            "1A",
            licence,
            "2026-10-09 12:03:01.456 xcodebuild[5555:1234] 69",
        );
        let b = view("2B", licence, "69");
        let ready = view("3C", XcodeState::Ready, "");
        let same = std::slice::from_ref(&a);
        assert_eq!(attention_changes("n", same, same), []);
        assert_eq!(
            attention_changes("n", same, std::slice::from_ref(&restamped)),
            []
        );
        let first_launch = XcodeView {
            state: "first_launch_not_run",
            ..b.clone()
        };
        assert_eq!(
            attention_changes(
                "mac-1",
                &[a.clone(), b.clone()],
                &[first_launch.clone(), ready]
            ),
            [
                (
                    true,
                    format!("node mac-1: {}", first_launch.attention().expect("item"))
                ),
                (
                    false,
                    format!("node mac-1: resolved: {}", a.attention().expect("item"))
                ),
                (
                    false,
                    format!("node mac-1: resolved: {}", b.attention().expect("item"))
                ),
            ]
        );
    }

    /// Catches (review of PR #258): an Xcode that is not ready, reported as not
    /// surveyed yet (the first status of a restarted daemon), logged as resolved, or
    /// raised again when its survey reports it unchanged; and the item it held never
    /// cleared once the survey finds it ready.
    #[test]
    fn an_xcode_not_surveyed_yet_keeps_its_item() {
        let licence = XcodeView::new(xcode(
            "1A",
            XcodeState::LicenseNotAccepted,
            "agree",
            "sudo x",
        ));
        let pending = XcodeView::new(xcode("", XcodeState::NotSurveyed, "not surveyed yet", ""));
        let pending = XcodeView {
            app: licence.app.clone(),
            ..pending
        };
        let other = XcodeView::new(xcode("", XcodeState::NotSurveyed, "not surveyed yet", ""));
        let before = std::slice::from_ref(&licence);
        assert_eq!(
            attention_changes("mac-1", before, &[pending.clone(), other]),
            []
        );
        let mut held = SoftwareView::new(NodeStatus::default(), 1);
        held.xcodes = vec![pending];
        held.hold_unsurveyed(before);
        assert_eq!(held.needs_attention(), [licence.attention().expect("item")]);
        assert_eq!(attention_changes("mac-1", &held.xcodes, before), []);
        let ready = XcodeView::new(xcode("1A", XcodeState::Ready, "", ""));
        assert_eq!(
            attention_changes("mac-1", &held.xcodes, &[ready]),
            [(
                false,
                format!(
                    "node mac-1: resolved: {}",
                    licence.attention().expect("item")
                )
            )]
        );
    }

    /// Catches: the greatest version reported as the oldest (`max`), the first or the last
    /// one listed taken for it (the least is in the middle here, and in a different place
    /// from the unknown ones), a node that names none folded into the oldest as 0 or as a
    /// number below every version (`Option`'s order puts `None` first, so a
    /// `min` over the options themselves reports no version at all whenever one is
    /// unknown), an unknown node not counted, and a list with no known version reported
    /// as an oldest version of 0.
    #[test]
    fn the_oldest_protocol_ignores_the_unknown_and_counts_them() {
        let summary = |protocols: &[Option<u32>]| ProtocolSummary::of(protocols.iter().copied());
        assert_eq!(
            summary(&[Some(3), Some(2), Some(5)]),
            ProtocolSummary {
                oldest_protocol: Some(2),
                protocol_unknown: 0
            }
        );
        assert_eq!(
            summary(&[None, Some(4), None, Some(2), Some(9), None]),
            ProtocolSummary {
                oldest_protocol: Some(2),
                protocol_unknown: 3
            }
        );
        assert_eq!(
            summary(&[None, Some(3)]),
            ProtocolSummary {
                oldest_protocol: Some(3),
                protocol_unknown: 1
            }
        );
        assert_eq!(
            summary(&[None, None]),
            ProtocolSummary {
                oldest_protocol: None,
                protocol_unknown: 2
            }
        );
        assert_eq!(
            summary(&[]),
            ProtocolSummary {
                oldest_protocol: None,
                protocol_unknown: 0
            }
        );
    }
}
