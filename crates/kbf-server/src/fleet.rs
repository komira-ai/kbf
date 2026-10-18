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

use kbf_proto::update::UpdateStatus;
use kbf_proto::update::update_status::State as UpdateState;
use kbf_proto::worker::{NodeStatus, XcodeState, XcodeStatus};
use kbf_sched::NodePhase;
use serde::Serialize;

/// One node, as `GET /v1/nodes` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodeView {
    /// The node id its daemon registered with.
    pub node_id: String,
    /// Whether its newest stream is still open.
    pub connected: bool,
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
    /// What its update client reported last ([`UpdateView`]).
    pub update: UpdateView,
}

/// A node's update client, as `GET /v1/nodes` lists it
/// (`docs/design/pull-updates.md` sections 5 and 6.3). Serialized with `availability` as a
/// tag. The newest `UpdateStatus` it sent is kept per node from its current stream only, in
/// memory, like [`SoftwareView`]; each daemon sends it again after its next `Welcome`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum UpdateView {
    /// It never sent an `UpdateStatus`: a daemon that predates pull-based updates. A
    /// rollout halts at it.
    Unsupported,
    /// It sends them.
    Ok(UpdateReport),
}

/// An `UpdateStatus` as the operator API shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateReport {
    /// `idle`, `fetching`, `staged`, `activated_unconfirmed`, `confirmed`, `reverted`,
    /// `refused`, `manual`, or `unknown` for a state this server does not know.
    pub state: &'static str,
    /// The commit of the running process (12 hex or `unknown`).
    pub running_commit: String,
    /// The commit its helper installed (40 hex; empty if not known).
    pub installed_commit: String,
    /// The commit its helper staged (40 hex; empty if nothing is).
    pub staged_commit: String,
    /// The newest offer generation it saw.
    pub offered_generation: u64,
    /// The newest order id it acted on.
    pub acked_order_id: u64,
    /// Why it refused, when `state` is `refused`.
    pub refusal: String,
    /// Whether the privileged helper is on the host.
    pub helper_present: bool,
    /// What the staged release declares in `kbf --version --json` (0 when nothing is
    /// staged): its protocol, the oldest protocol it speaks, and its state schema.
    pub staged_protocol: u32,
    pub staged_min_protocol: u32,
    pub staged_state_schema: u32,
}

/// The longest refusal text kept, in bytes. `GET /v1/nodes` is open and a node's text is its own.
pub const MAX_REFUSAL_BYTES: usize = 512;

/// `text` if it is `len` lowercase hex digits (or `allowed`), else `fallback`.
fn hex_or(text: String, len: usize, allowed: &str, fallback: &str) -> String {
    let hex = text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if hex || text == allowed {
        text
    } else {
        fallback.to_owned()
    }
}

/// `text` cut to at most `max` bytes at a character boundary.
fn capped(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

impl UpdateReport {
    /// `status` as shown. The commits are shown only if they are what the wire says they are
    /// (12 lowercase hex digits or `unknown` for the running one, 40 or none for the helper's)
    /// and the refusal is cut at [`MAX_REFUSAL_BYTES`]: the node chose the bytes.
    #[must_use]
    pub fn new(status: UpdateStatus) -> Self {
        let state = match UpdateState::try_from(status.state) {
            Ok(UpdateState::Idle) => "idle",
            Ok(UpdateState::Fetching) => "fetching",
            Ok(UpdateState::Staged) => "staged",
            Ok(UpdateState::ActivatedUnconfirmed) => "activated_unconfirmed",
            Ok(UpdateState::Confirmed) => "confirmed",
            Ok(UpdateState::Reverted) => "reverted",
            Ok(UpdateState::Refused) => "refused",
            Ok(UpdateState::Manual) => "manual",
            Ok(UpdateState::Unspecified) | Err(_) => "unknown",
        };
        Self {
            state,
            running_commit: hex_or(status.running_commit, 12, "unknown", "unknown"),
            installed_commit: hex_or(status.installed_commit, 40, "", ""),
            staged_commit: hex_or(status.staged_commit, 40, "", ""),
            offered_generation: status.offered_generation,
            acked_order_id: status.acked_order_id,
            refusal: capped(status.refusal, MAX_REFUSAL_BYTES),
            helper_present: status.helper_present,
            staged_protocol: status.staged_protocol,
            staged_min_protocol: status.staged_min_protocol,
            staged_state_schema: status.staged_state_schema,
        }
    }
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
        /// epoch, server clock.
        deadline_unix_ms: u64,
        /// The leases it still holds, as `term.seq`.
        leases: Vec<String>,
    },
    /// Cordoned, and it holds no lease: it may be taken out of service.
    Drained,
    /// The deadline passed while leases still ran. They run on; nothing proceeds
    /// until an operator drains it again or uncordons it.
    DrainPaused {
        /// The deadline that passed (milliseconds since the Unix epoch).
        deadline_unix_ms: u64,
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

/// The body of `GET /v1/nodes`: the server that answers, and every node registered
/// since it started, in node-id order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodesView {
    /// The server process that answers.
    pub server: ServerView,
    /// The nodes.
    pub nodes: Vec<NodeView>,
}

impl NodesView {
    /// `nodes`, as this server build lists them.
    #[must_use]
    pub fn of_this_build(nodes: Vec<NodeView>) -> Self {
        Self {
            server: ServerView::this_build(),
            nodes,
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

    /// Catches a state shown as another, and a value this server does not know shown as
    /// one it does (an operator would read a refusal as an idle host).
    #[test]
    fn every_update_state_has_its_own_name() {
        let table = [
            (UpdateState::Idle, "idle"),
            (UpdateState::Fetching, "fetching"),
            (UpdateState::Staged, "staged"),
            (UpdateState::ActivatedUnconfirmed, "activated_unconfirmed"),
            (UpdateState::Confirmed, "confirmed"),
            (UpdateState::Reverted, "reverted"),
            (UpdateState::Refused, "refused"),
            (UpdateState::Manual, "manual"),
            (UpdateState::Unspecified, "unknown"),
        ];
        for (state, name) in table {
            let status = UpdateStatus {
                state: state as i32,
                ..UpdateStatus::default()
            };
            assert_eq!(UpdateReport::new(status).state, name);
        }
        let unknown = UpdateStatus {
            state: 99,
            ..UpdateStatus::default()
        };
        assert_eq!(UpdateReport::new(unknown).state, "unknown");
    }

    /// Catches a commit shown that is not a commit (a node's text on the open listing), and
    /// a refusal that is not cut: at a character boundary, not inside one.
    #[test]
    fn a_status_is_shown_only_in_the_shape_the_wire_gives_it() {
        let status = UpdateStatus {
            running_commit: "0123456789ab".to_owned(),
            installed_commit: "a".repeat(40),
            staged_commit: String::new(),
            refusal: "é".repeat(MAX_REFUSAL_BYTES),
            ..UpdateStatus::default()
        };
        let shown = UpdateReport::new(status);
        assert_eq!(shown.running_commit, "0123456789ab");
        assert_eq!(shown.installed_commit, "a".repeat(40));
        assert_eq!(shown.staged_commit, "");
        assert_eq!(shown.refusal.len(), MAX_REFUSAL_BYTES);
        let bad = UpdateStatus {
            running_commit: "<script>".to_owned(),
            installed_commit: "A".repeat(40),
            staged_commit: "a".repeat(39),
            refusal: "x".repeat(MAX_REFUSAL_BYTES + 1),
            ..UpdateStatus::default()
        };
        let shown = UpdateReport::new(bad);
        assert_eq!(shown.running_commit, "unknown");
        assert_eq!(
            (
                shown.installed_commit.as_str(),
                shown.staged_commit.as_str()
            ),
            ("", "")
        );
        assert_eq!(shown.refusal.len(), MAX_REFUSAL_BYTES);
    }
}
