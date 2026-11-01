//! The farm machine: the farm core's state beside the scheduler that every server of a
//! replicated farm will hold alike. It holds the record of each caller waiting on an
//! operation, the finished operations whose callers are still kept, the leases whose
//! `Start` was sent, and the node registry: each node that has registered, whether
//! its newest stream is open, and its newest `NodeStatus`.
//!
//! It is pure: its methods change it only from their arguments. Nothing here reads a
//! clock (a wall-clock time comes in as a value), a random source or a hashed map's
//! iteration order; this module denies clippy's `disallowed_methods` and
//! `disallowed_types` (`clippy.toml`) to keep it so. Two machines given the same calls
//! in the same order are equal, and iterate their maps in the same order.
//!
//! What one server alone holds stays in `crate::farm`: each caller's `watch` channel,
//! the worker streams and the heartbeat each `Start` names. A node's stream is one of
//! those, so the machine does not see it close: the farm tells it
//! ([`FarmMachine::disconnect`]) before it reads the registry. The scheduler is not part
//! of the machine yet; the methods that depend on it take what they need from it as
//! arguments.
//!
//! It is public so that `kbf-sim-cell` assembles its simulated server from it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::{BTreeMap, VecDeque};
use std::time::SystemTime;

use kbf_types::{ActionKey, Digest, LeaseId, LeaseKind, OperationId, WaiterId, WorkerId};

use crate::action_usage::Subject;
use crate::fleet::SoftwareView;
use crate::stamp::Stamp;
use crate::update_status::UpdateReport;

/// The REAPI name of operation `operation` in a farm over a state store:
/// `operations/<operation>`, the id in decimal.
///
/// A farm over a state store numbers its operations from blocks it reserves in the store,
/// so an id names one operation across frontends and restarts, and no term is needed to
/// keep the name from naming two (issue #154). Any frontend of the store can answer a
/// call for the name from the store's rows. A farm with no state store numbers from 0 in
/// every process and names its operations by [`FarmMachine::name`] instead.
#[must_use]
pub fn stored_name(operation: OperationId) -> String {
    format!("operations/{}", operation.0)
}

/// The operation a name of the form [`stored_name`] writes names: `None` for any other
/// spelling, a name of the form [`FarmMachine::name`] writes (it has a `-`) and a cached
/// result's name (`operations/cached/...`) among them, and for a spelling [`stored_name`]
/// does not write (signs, leading zeros, white space).
#[must_use]
pub fn parse_stored_name(name: &str) -> Option<OperationId> {
    canonical_u64(name.strip_prefix("operations/")?).map(OperationId)
}

/// The REAPI name of the operation `waiter` waits on, in the process of `term`:
/// `operations/{term}-{waiter}`.
///
/// Waiter ids count from 0 in every process, so the term is what keeps a name from
/// naming two operations: without it, a client's `WaitExecution` with a name from the
/// process before a restart attaches to whichever operation of the new process got
/// the same number, and hands it that action's result (issue #154). A farm over a state
/// store names its operations by [`stored_name`] instead, and answers a name of this form
/// only for a caller it holds.
fn operation_name(term: u64, waiter: WaiterId) -> String {
    format!("operations/{term}-{}", waiter.0)
}

/// The waiter an operation name of the process of `term` names: the inverse of
/// [`operation_name`]. `None` for a name of another term, and for any spelling
/// [`operation_name`] does not write (signs, leading zeros, other prefixes).
fn parse_operation_name(name: &str, term: u64) -> Option<WaiterId> {
    let (named_term, waiter) = name.strip_prefix("operations/")?.split_once('-')?;
    if canonical_u64(named_term)? != term {
        return None;
    }
    canonical_u64(waiter).map(WaiterId)
}

/// `text` as a `u64`, if it is that number's decimal spelling exactly (`u64`'s
/// `FromStr` also takes a `+` sign and leading zeros).
fn canonical_u64(text: &str) -> Option<u64> {
    let n: u64 = text.parse().ok()?;
    (n.to_string() == text).then_some(n)
}

/// A caller waiting on an operation. Its name is not kept: it is
/// [`FarmMachine::name`] of its id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiter {
    /// The instance name and action it submitted.
    pub key: ActionKey,
    pub kind: LeaseKind,
    pub do_not_cache: bool,
    /// When it was submitted, on the wall clock.
    pub queued: SystemTime,
    /// What the server's `action usage:` line says the action is; the first caller's
    /// names the operation.
    pub usage: Subject,
    /// The OS family and ISA the action asked for, for the lease lines.
    pub platform: Platform,
}

/// The operating system and architecture an action asked for, as kbf parsed them from
/// its platform (`kbf_caps::Request::from_platform`), for the `lease granted` line. A
/// part the action names none of is `-`. Only parsed values are kept, so a platform
/// property's raw text never reaches the log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Platform {
    /// `linux` or `macos` (a daemon runs on no other); `-` for none, and for a value
    /// that is neither.
    pub os_family: &'static str,
    /// The architecture's report name (`x86_64`, `arm64`); `-` for none.
    pub isa: String,
}

impl Platform {
    /// What `needs` asks for.
    #[must_use]
    pub fn of(needs: &kbf_caps::Request) -> Self {
        let os_family = needs
            .exact("os")
            .and_then(|os| kbf_caps::DAEMON_OSES.iter().find(|known| **known == os))
            .copied()
            .unwrap_or("-");
        let isa = needs
            .properties()
            .into_iter()
            .find_map(|(key, value)| (key == "arch").then_some(value))
            .unwrap_or_else(|| "-".to_owned());
        Self { os_family, isa }
    }
}

/// What a lease was granted, as its `Start` carried it: the booking after the
/// memory ladder and the floors, not the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Booked {
    /// Thousandths of a CPU.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
}

/// A lease whose `Start` was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sent {
    pub operation: OperationId,
    /// The action the `Start` named.
    pub action: Digest,
    /// When the operation was queued and the `Start` sent.
    pub stamp: Stamp,
    /// What the lease was granted; `None` for a lease taken up from the state store,
    /// which was never started here.
    pub booked: Option<Booked>,
}

/// A node that has registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// Whether its newest stream is open, as the farm last told the machine.
    pub connected: bool,
    /// The newest `NodeStatus` it sent, on any of its streams; `None` before the first.
    pub status: Option<SoftwareView>,
    /// The newest `UpdateStatus` it sent, on any of its streams; `None` before the first.
    pub update: Option<UpdateReport>,
    /// Whether `update` came on the node's newest stream: a stream that registers
    /// clears it until that stream sends its own.
    pub update_live: bool,
}

/// The waiter records, the started table and the node registry of one farm (see the
/// module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FarmMachine {
    /// The scheduler term every operation name carries.
    term: u64,
    /// The id the next caller gets.
    next_waiter: u64,
    /// The callers of operations the scheduler holds: unfinished ones, and finished
    /// ones for the scheduler's finished retention, so that a WaitExecution on one
    /// gets its result (issue #165).
    waiters: BTreeMap<WaiterId, Waiter>,
    /// Finished operations whose callers are still kept, in the order they finished,
    /// with those callers. Each leaves once the scheduler has dropped its operation.
    finished: VecDeque<(OperationId, Vec<WaiterId>)>,
    /// Leases whose `Start` was sent.
    started: BTreeMap<LeaseId, Sent>,
    /// Every node that has registered, kept after its stream closes (issue #235).
    nodes: BTreeMap<WorkerId, Node>,
}

impl FarmMachine {
    /// An empty machine whose operation names carry `term`.
    pub const fn new(term: u64) -> Self {
        Self {
            term,
            next_waiter: 0,
            waiters: BTreeMap::new(),
            finished: VecDeque::new(),
            started: BTreeMap::new(),
            nodes: BTreeMap::new(),
        }
    }

    /// Records a new caller and returns its id, the next in order.
    pub fn submit(&mut self, waiter: Waiter) -> WaiterId {
        let id = WaiterId(self.next_waiter);
        self.next_waiter += 1;
        self.waiters.insert(id, waiter);
        id
    }

    /// Records a caller that the scheduler core numbered (`id`), where
    /// [`FarmMachine::submit`] numbers its own. A machine takes its callers from one or the
    /// other, never both: `submit` does not skip the ids inserted here.
    pub fn insert(&mut self, id: WaiterId, waiter: Waiter) {
        self.waiters.insert(id, waiter);
    }

    /// The caller `id`, while it is kept.
    pub fn waiter(&self, id: WaiterId) -> Option<&Waiter> {
        self.waiters.get(&id)
    }

    /// The ids of the kept callers, in id order.
    pub fn waiter_ids(&self) -> impl Iterator<Item = WaiterId> + '_ {
        self.waiters.keys().copied()
    }

    /// The REAPI name of the operation caller `id` waits on.
    pub fn name(&self, id: WaiterId) -> String {
        operation_name(self.term, id)
    }

    /// The kept caller `name` names, if this machine wrote that name.
    pub fn named(&self, name: &str) -> Option<(WaiterId, &Waiter)> {
        let id = parse_operation_name(name, self.term)?;
        self.waiter(id).map(|waiter| (id, waiter))
    }

    /// Records that the `Start` of `lease` was sent.
    pub fn start(&mut self, lease: LeaseId, sent: Sent) {
        self.started.insert(lease, sent);
    }

    /// What was sent for `lease`, if its `Start` was sent and it is not forgotten.
    pub fn sent(&self, lease: LeaseId) -> Option<Sent> {
        self.started.get(&lease).copied()
    }

    /// The operation of each lease whose `Start` was sent, in lease order.
    pub fn started_operations(&self) -> impl Iterator<Item = OperationId> + '_ {
        self.started.values().map(|sent| sent.operation)
    }

    /// Forgets every lease of a finished `operation` whose `Start` was sent.
    pub fn forget_leases(&mut self, operation: OperationId) {
        self.started.retain(|_, sent| sent.operation != operation);
    }

    /// Records that `worker` registered on a new stream: it is connected, and keeps
    /// the status it sent on an earlier one.
    pub fn register(&mut self, worker: &WorkerId) {
        let node = self.nodes.entry(worker.clone()).or_insert(Node {
            connected: true,
            status: None,
            update: None,
            update_live: false,
        });
        node.connected = true;
        node.update_live = false;
    }

    /// Records that the newest stream of `worker` has closed. The node stays listed.
    pub fn disconnect(&mut self, worker: &WorkerId) {
        if let Some(node) = self.nodes.get_mut(worker) {
            node.connected = false;
        }
    }

    /// Keeps `status` as the newest of `worker`, if it has registered.
    pub fn report_status(&mut self, worker: &WorkerId, status: SoftwareView) {
        if let Some(node) = self.nodes.get_mut(worker) {
            node.status = Some(status);
        }
    }

    /// Keeps `report` as the newest update status of `worker`, from its newest stream, if
    /// it has registered.
    pub fn report_update(&mut self, worker: &WorkerId, report: UpdateReport) {
        if let Some(node) = self.nodes.get_mut(worker) {
            node.update = Some(report);
            node.update_live = true;
        }
    }

    /// The node `worker`, if it has registered.
    pub fn node(&self, worker: &WorkerId) -> Option<&Node> {
        self.nodes.get(worker)
    }

    /// Every node that has registered, in node-id order.
    pub fn nodes(&self) -> impl Iterator<Item = &WorkerId> + '_ {
        self.nodes.keys()
    }

    /// Keeps `waiters`, the callers of `operation`, which has just finished, until
    /// [`Self::forget_dropped`] finds the scheduler has dropped it.
    pub fn keep_finished(&mut self, operation: OperationId, waiters: Vec<WaiterId>) {
        self.finished.push_back((operation, waiters));
    }

    /// Forgets the callers of each finished operation the scheduler has dropped, from
    /// the oldest up to the first that `held` says it still holds, and returns them.
    ///
    /// The scheduler drops them in the order they finished, and only those that
    /// finished at one farm time can be kept here in another order, so stopping at the
    /// first it still holds leaves none behind for longer than the input that drops it.
    pub fn forget_dropped(&mut self, held: impl Fn(OperationId) -> bool) -> Vec<WaiterId> {
        let dropped = self
            .finished
            .iter()
            .position(|(operation, _)| held(*operation))
            .unwrap_or(self.finished.len());
        let forgotten: Vec<WaiterId> = self
            .finished
            .drain(..dropped)
            .flat_map(|(_, waiters)| waiters)
            .collect();
        for id in &forgotten {
            self.waiters.remove(id);
        }
        forgotten
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use kbf_sim::SimRng;
    use kbf_types::DigestFunction;

    use super::*;

    const TERM: u64 = 0x0199_8a6b_2c3d_4e5f;

    fn at(millis: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(millis)
    }

    fn digest(n: u8) -> Digest {
        Digest {
            function: DigestFunction::Sha256,
            hash: [n; 32],
            size_bytes: u64::from(n),
        }
    }

    /// Catches: a platform line that logs the request's raw text (an `os` that is not
    /// one a daemon runs on is `-`), or a part the request does not name that is not `-`.
    #[test]
    fn a_platform_is_the_parsed_os_family_and_isa_or_dashes() {
        let of = |properties: &[(&str, &str)]| {
            let request = kbf_caps::Request::parse(properties.iter().copied()).expect("parses");
            let platform = Platform::of(&request);
            (platform.os_family, platform.isa)
        };
        assert_eq!(of(&[]), ("-", "-".to_owned()));
        assert_eq!(
            of(&[("os", "macos"), ("arch", "arm64")]),
            ("macos", "arm64".to_owned())
        );
        assert_eq!(of(&[("os", "linux")]), ("linux", "-".to_owned()));
        assert_eq!(of(&[("os", "x\nINFO forged")]), ("-", "-".to_owned()));
    }

    fn waiter(n: u8) -> Waiter {
        Waiter {
            key: ActionKey {
                instance: format!("instance-{}", n % 2),
                action: digest(n),
            },
            kind: if n.is_multiple_of(3) {
                LeaseKind::WholeMachine
            } else {
                LeaseKind::Action
            },
            do_not_cache: n.is_multiple_of(5),
            queued: at(u64::from(n)),
            usage: Subject::of(kbf_front::Described::default()).0,
            platform: Platform::of(&kbf_caps::Request::default()),
        }
    }

    fn node(n: u64) -> WorkerId {
        WorkerId::new(format!("node-{n}"))
    }

    fn status(n: u8) -> SoftwareView {
        SoftwareView {
            os_name: "Linux".to_owned(),
            os_version: format!("{n}"),
            os_build: String::new(),
            kernel: "6.8".to_owned(),
            daemon_version: "0.1.0".to_owned(),
            xcode_builds: Vec::new(),
            xcodes: Vec::new(),
            container_images: Vec::new(),
            received_at_unix_ms: u64::from(n),
        }
    }

    fn update(n: u8) -> UpdateReport {
        UpdateReport {
            running_commit: format!("{n:012x}"),
            installed_commit: String::new(),
            acked_order_id: 0,
            kind: kbf_proto::update::update_status::State::Idle,
            available: None,
            release_source: crate::update_status::ReleaseSource::Unreported,
        }
    }

    fn sent(operation: u64, n: u8) -> Sent {
        Sent {
            operation: OperationId(operation),
            action: digest(n),
            stamp: Stamp {
                queued: at(u64::from(n)),
                started: at(u64::from(n) + 1),
            },
            booked: None,
        }
    }

    /// Applies one call, drawn from `rng`, to `machine`. Every value the call takes
    /// is drawn from `rng` too, so a seed names exactly one call sequence.
    fn step(machine: &mut FarmMachine, rng: &mut SimRng) {
        let small = |rng: &mut SimRng| u8::try_from(rng.below(32)).unwrap_or_default();
        match rng.below(9) {
            0 | 1 => {
                machine.submit(waiter(small(rng)));
            }
            5 => machine.register(&node(rng.below(32))),
            6 => machine.disconnect(&node(rng.below(32))),
            7 => machine.report_status(&node(rng.below(32)), status(small(rng))),
            8 => machine.report_update(&node(rng.below(32)), update(small(rng))),
            2 => {
                let lease = LeaseId::new(rng.between(1, 3), rng.below(64));
                machine.start(lease, sent(rng.below(8), small(rng)));
            }
            3 => {
                let operation = OperationId(rng.below(8));
                machine.forget_leases(operation);
                let waiters = (0..rng.below(4)).map(|_| WaiterId(rng.below(64)));
                machine.keep_finished(operation, waiters.collect());
            }
            _ => {
                let still_held = rng.below(8);
                machine.forget_dropped(|operation| operation.0 >= still_held);
            }
        }
    }

    /// Catches a machine that is not a function of its calls: a hashed map in it (its
    /// iteration order is drawn per map, so two equal machines list their callers,
    /// leases or nodes in different orders, and a snapshot or digest of them differs),
    /// and a call that reads a clock or a random source instead of its arguments. Over
    /// a sweep of seeds, two machines given the same generated calls are equal after
    /// every call, and list their callers, leases and nodes in the same, ascending
    /// order.
    #[test]
    fn machines_given_the_same_calls_are_equal_and_list_in_key_order() {
        let mut busiest = 0;
        for seed in 0..64 {
            let mut calls = SimRng::from_seed(seed);
            let mut replay = SimRng::from_seed(seed);
            let (mut a, mut b) = (FarmMachine::new(TERM), FarmMachine::new(TERM));
            for _ in 0..200 {
                step(&mut a, &mut calls);
                step(&mut b, &mut replay);
                assert_eq!(a, b, "seed {seed}");
            }
            let order = |m: &FarmMachine| {
                let waiters: Vec<WaiterId> = m.waiters.keys().copied().collect();
                let leases: Vec<LeaseId> = m.started.keys().copied().collect();
                let nodes: Vec<WorkerId> = m.nodes().cloned().collect();
                (waiters, leases, nodes)
            };
            let (waiters, leases, nodes) = order(&a);
            let listed = (waiters.clone(), leases.clone(), nodes.clone());
            assert_eq!(listed, order(&b), "seed {seed}");
            assert!(waiters.is_sorted(), "seed {seed}: callers {waiters:?}");
            assert!(leases.is_sorted(), "seed {seed}: leases {leases:?}");
            assert!(nodes.is_sorted(), "seed {seed}: nodes {nodes:?}");
            busiest = busiest.max(waiters.len().min(leases.len()).min(nodes.len()));
        }
        // The sweep is only a check if the maps it orders hold several entries.
        assert!(busiest >= 8, "no seed filled every map: {busiest}");
    }

    /// Catches an equality that leaves a field out (a hand-written `PartialEq` that
    /// skips one, or a field kept outside the struct): two replicas that differ only
    /// there would compare equal, so a divergence there would never be seen. Each
    /// field is changed in turn, and the machine must then differ from the original.
    #[test]
    fn equality_covers_every_field() {
        let mut base = FarmMachine::new(TERM);
        let first = base.submit(waiter(1));
        base.submit(waiter(2));
        base.keep_finished(OperationId(4), vec![first]);
        base.start(LeaseId::new(1, 7), sent(4, 9));
        base.register(&node(1));
        base.report_status(&node(1), status(1));
        base.report_update(&node(1), update(1));
        let lease = LeaseId::new(1, 7);
        type Change = fn(&mut FarmMachine);
        let changes: [(&str, Change); 20] = [
            ("term", |m| m.term += 1),
            ("next_waiter", |m| m.next_waiter += 1),
            ("waiters: one more", |m| {
                m.waiters.insert(WaiterId(9), waiter(1));
            }),
            ("waiter.key.instance", |m| {
                m.waiters
                    .values_mut()
                    .for_each(|w| w.key.instance.push('x'));
            }),
            ("waiter.key.action", |m| {
                m.waiters
                    .values_mut()
                    .for_each(|w| w.key.action = digest(3));
            }),
            ("waiter.kind", |m| {
                m.waiters
                    .values_mut()
                    .for_each(|w| w.kind = LeaseKind::WholeMachine);
            }),
            ("waiter.do_not_cache", |m| {
                m.waiters.values_mut().for_each(|w| w.do_not_cache = true);
            }),
            ("waiter.queued", |m| {
                m.waiters.values_mut().for_each(|w| w.queued = at(99));
            }),
            ("finished: operation", |m| m.finished[0].0 = OperationId(5)),
            ("finished: callers", |m| m.finished[0].1.push(WaiterId(1))),
            ("started: lease", |m| {
                let sent = m.started.remove(&LeaseId::new(1, 7));
                m.started.extend(sent.map(|s| (LeaseId::new(1, 8), s)));
            }),
            ("sent.operation", |m| {
                m.started
                    .values_mut()
                    .for_each(|s| s.operation = OperationId(5));
            }),
            ("sent.action", |m| {
                m.started.values_mut().for_each(|s| s.action = digest(3));
            }),
            ("sent.stamp.queued", |m| {
                m.started.values_mut().for_each(|s| s.stamp.queued = at(99));
            }),
            ("sent.stamp.started", |m| {
                m.started
                    .values_mut()
                    .for_each(|s| s.stamp.started = at(99));
            }),
            ("nodes: one more", |m| m.register(&node(2))),
            ("node.connected", |m| m.disconnect(&node(1))),
            ("node.status", |m| m.report_status(&node(1), status(2))),
            ("node.update", |m| m.report_update(&node(1), update(2))),
            ("node.update_live", |m| m.register(&node(1))),
        ];
        for (field, change) in changes {
            let mut changed = base.clone();
            change(&mut changed);
            assert_ne!(changed, base, "a change to {field} is not seen");
        }
        assert_eq!(base.sent(lease), Some(sent(4, 9)));
    }

    /// Catches a registry that forgets a node whose stream closed (#235: it stays
    /// listed, not connected), one that drops a node's status or update report when it
    /// registers again (both are kept across streams), one that leaves the report live
    /// for a stream that has sent none (a host that restarted onto another release
    /// would be judged by its previous process), one whose registering again leaves it
    /// not connected, and one that lists a node that never registered because a
    /// status or a closed stream named it.
    #[test]
    fn the_registry_keeps_each_node_across_its_streams() {
        let mut m = FarmMachine::new(TERM);
        m.disconnect(&node(1));
        m.report_status(&node(1), status(1));
        m.report_update(&node(1), update(1));
        assert_eq!(m.node(&node(1)), None);
        assert_eq!(m.nodes().count(), 0);
        m.register(&node(1));
        m.report_status(&node(1), status(1));
        m.report_update(&node(1), update(1));
        m.disconnect(&node(1));
        let left = Node {
            connected: false,
            status: Some(status(1)),
            update: Some(update(1)),
            update_live: true,
        };
        assert_eq!(m.node(&node(1)), Some(&left));
        m.register(&node(1));
        // The report is kept, but no longer live: the new stream has not sent its own.
        let back = Node {
            connected: true,
            status: Some(status(1)),
            update: Some(update(1)),
            update_live: false,
        };
        assert_eq!(m.node(&node(1)), Some(&back));
        assert_eq!(m.nodes().collect::<Vec<_>>(), vec![&node(1)]);
    }

    /// Catches `forget_dropped` forgetting past the first operation the scheduler still
    /// holds (a caller of a kept operation would get NOT_FOUND), or keeping one it has
    /// dropped; and the callers it forgets not being returned, so the farm would keep
    /// their channels.
    #[test]
    fn forget_dropped_stops_at_the_first_held_operation() {
        let mut m = FarmMachine::new(TERM);
        let ids: Vec<WaiterId> = (0..4).map(|n| m.submit(waiter(n))).collect();
        m.keep_finished(OperationId(1), vec![ids[0], ids[1]]);
        m.keep_finished(OperationId(2), vec![ids[2]]);
        m.keep_finished(OperationId(3), vec![ids[3]]);
        // Operation 1 is dropped, 2 is held, 3 is dropped but finished after 2.
        let forgotten = m.forget_dropped(|operation| operation == OperationId(2));
        assert_eq!(forgotten, vec![ids[0], ids[1]]);
        assert_eq!(m.waiter(ids[0]), None);
        assert_eq!(m.waiter_ids().collect::<Vec<_>>(), [ids[2], ids[3]]);
        assert_eq!(m.waiter(ids[3]), Some(&waiter(3)));
        assert_eq!(m.forget_dropped(|_| false), vec![ids[2], ids[3]]);
        assert_eq!(m.waiter_ids().next(), None);
        assert_eq!(m.finished, VecDeque::new());
    }

    /// Catches a lookup by name that finds a caller under a name the machine never
    /// wrote (another id, or the same id in another term), or misses one it did:
    /// `named` is the inverse of `name` for kept callers.
    #[test]
    fn a_kept_caller_is_found_by_its_name_only() {
        let mut m = FarmMachine::new(TERM);
        let id = m.submit(waiter(1));
        let name = m.name(id);
        assert_eq!(name, format!("operations/{TERM}-0"));
        assert_eq!(m.named(&name), Some((id, &waiter(1))));
        assert_eq!(m.named(&format!("operations/{TERM}-1")), None);
        // Another term's machine keeps a caller with the same id, so a lookup that
        // ignores the term would find it.
        let mut other = FarmMachine::new(TERM + 1);
        assert_eq!(other.submit(waiter(2)), id);
        assert_eq!(other.named(&name), None);
    }

    /// Catches a name parser that is not the exact inverse of [`operation_name`]: one
    /// that ignores the term or accepts another (issue #154), and one that accepts a
    /// spelling the server never writes, which would give one operation several names.
    #[test]
    fn operation_names_parse_only_as_this_term_writes_them() {
        let term = TERM;
        for n in [0, 1, 42, u64::MAX] {
            let name = operation_name(term, WaiterId(n));
            assert_eq!(
                parse_operation_name(&name, term),
                Some(WaiterId(n)),
                "{name}"
            );
            for other in [0, term - 1, term + 1, u64::MAX] {
                assert_eq!(
                    parse_operation_name(&name, other),
                    None,
                    "{name} as {other}"
                );
            }
        }
        assert_eq!(operation_name(7, WaiterId(3)), "operations/7-3");
        for refused in [
            "",
            "operations/",
            "operations/7",
            "operations/7-",
            "operations/-3",
            "operations/7--3",
            "operations/7-3-1",
            "operations/07-3",
            "operations/7-03",
            "operations/+7-3",
            "operations/7-+3",
            "operations/7-3 ",
            " operations/7-3",
            "operations/7-18446744073709551616",
            "Operations/7-3",
            "operation/7-3",
            "operations/7_3",
            "7-3",
            "operations/3",
            "operations/cached/7-3",
        ] {
            assert_eq!(parse_operation_name(refused, 7), None, "{refused:?}");
        }
        assert_eq!(parse_operation_name("operations/7-0", 7), Some(WaiterId(0)));
    }

    /// Asserts that a stored name is the operation id in decimal, that it parses back to
    /// the id, and that no other spelling parses: not a name of the form with a term (which
    /// would name two operations), not a cached result's name, not a signed, padded or
    /// overlong number.
    #[test]
    fn a_stored_name_is_the_operation_id_and_no_other_spelling_parses() {
        for n in [0, 1, 42, u64::MAX] {
            let name = stored_name(OperationId(n));
            assert_eq!(name, format!("operations/{n}"));
            assert_eq!(parse_stored_name(&name), Some(OperationId(n)), "{name}");
        }
        for refused in [
            "",
            "operations/",
            "operations",
            "operations/-1",
            "operations/+7",
            "operations/07",
            "operations/7 ",
            " operations/7",
            "operations/18446744073709551616",
            "Operations/7",
            "operation/7",
            "operations/7-3",
            "operations/cached/7",
            "operations/cached/ab-3",
            "7",
        ] {
            assert_eq!(parse_stored_name(refused), None, "{refused:?}");
        }
    }

    /// Asserts that a name of the form a machine with a term writes is not a stored name,
    /// and a stored name is not found as a kept caller of a machine: the two forms never
    /// name each other's operations.
    #[test]
    fn the_two_name_forms_never_name_each_other() {
        let mut m = FarmMachine::new(TERM);
        let id = m.submit(waiter(1));
        assert_eq!(parse_stored_name(&m.name(id)), None);
        assert_eq!(m.named(&stored_name(OperationId(0))), None);
    }

    /// Asserts that a caller inserted under an id someone else chose is kept under that id
    /// and forgotten with its finished operation like any other.
    #[test]
    fn a_caller_inserted_under_a_chosen_id_is_kept_and_forgotten_like_any_other() {
        let mut m = FarmMachine::new(TERM);
        m.insert(WaiterId(41), waiter(2));
        assert_eq!(m.waiter(WaiterId(41)), Some(&waiter(2)));
        m.keep_finished(OperationId(9), vec![WaiterId(41)]);
        assert_eq!(m.forget_dropped(|_| false), vec![WaiterId(41)]);
        assert_eq!(m.waiter(WaiterId(41)), None);
    }
}
