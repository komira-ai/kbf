//! A scenario, the cell it builds, and the run loop that checks after every step.

use std::time::Duration;

use kbf_sim::{Chance, Faults, Node, NodeId, NodeInput, Output, Partition, Sim, SimRng, TraceHash};
use kbf_types::{Effect, FarmTime, Resources, StateMachine};

use crate::check::Checker;
use crate::{FakeClient, FakeDaemon, Msg, ObjectStore, Server, ServerFault, ServerPlan, action};

/// What the server meets in a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Nothing: only the network's faults. Every property is enforced.
    Calm,
    /// One or two process kills.
    Restart,
    /// A power cut, now or part-way through a later commit.
    Crash,
    /// An I/O error that stops the log, then a restart.
    IoError,
    /// One daemon cut off from the cell for longer than the scheduler's grace G.
    Partition,
}

/// The whole input of a run, drawn from its kind and seed.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub kind: Kind,
    pub seed: u64,
    pub faults: Faults,
    pub server: ServerPlan,
    pub daemons: usize,
    /// A daemon cut off from every other node from a time, for [`CUT_FOR`].
    pub partition: Option<(usize, FarmTime)>,
    /// Each client's actions, and when it starts on each.
    pub clients: Vec<Vec<(u64, FarmTime)>>,
    /// The run stops here. Every fault has ended long enough before it for every
    /// action to be answered.
    pub end: FarmTime,
}

/// How many distinct actions the clients draw theirs from, so some are shared.
const POOL: u64 = 10;
const ACTIONS_PER_CLIENT: usize = 6;
const LAST_FAULT_MS: u64 = 60_000;
const DAEMONS: usize = 3;
/// How long a [`Kind::Partition`] daemon is cut off: past the grace G (60 s).
pub const CUT_FOR: Duration = Duration::from_secs(70);

impl Scenario {
    /// The scenario `kind` draws from `seed`.
    #[must_use]
    pub fn new(kind: Kind, seed: u64) -> Self {
        let mut rng = SimRng::from_seed(seed ^ 0x5ce1_1ce1);
        let at = |rng: &mut SimRng| FarmTime::from_millis(rng.between(5_000, LAST_FAULT_MS));
        let mut faults = Vec::new();
        match kind {
            Kind::Calm => {}
            Kind::Restart => {
                for _ in 0..rng.between(1, 2) {
                    faults.push((at(&mut rng), ServerFault::Kill));
                }
            }
            Kind::Crash => {
                let keep = usize::try_from(rng.between(0, 64)).expect("small");
                let names = rng.next_u64();
                let fault = if rng.chance(Chance::percent(50)) {
                    ServerFault::PowerCut { keep, names }
                } else {
                    let after = rng.between(0, 30);
                    ServerFault::PowerCutAt { after, keep, names }
                };
                faults.push((at(&mut rng), fault));
            }
            Kind::IoError => {
                let after = rng.between(0, 30);
                faults.push((at(&mut rng), ServerFault::IoErrorAt { after }));
            }
            Kind::Partition => {}
        }
        let partition = (kind == Kind::Partition).then(|| {
            let daemon = usize::try_from(rng.between(0, DAEMONS as u64 - 1)).expect("small");
            (daemon, at(&mut rng))
        });
        faults.sort_by_key(|(t, _)| *t);
        let clients = (0..3)
            .map(|_| {
                (0..ACTIONS_PER_CLIENT)
                    .map(|_| {
                        let n = rng.between(0, POOL - 1);
                        (n, FarmTime::from_millis(rng.between(0, LAST_FAULT_MS)))
                    })
                    .collect()
            })
            .collect();
        Self {
            kind,
            seed,
            faults: Faults {
                min_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(50),
                drop: Chance::percent(1),
                duplicate: Chance::percent(3),
                reorder: Chance::percent(3),
            },
            server: ServerPlan {
                faults,
                down_for: Duration::from_secs(3),
            },
            daemons: DAEMONS,
            partition,
            clients,
            end: FarmTime::from_millis(LAST_FAULT_MS + 150_000),
        }
    }
}

/// The node kinds of the cell.
pub enum CellNode {
    Server(Box<Server>),
    Daemon(Box<FakeDaemon>),
    Client(Box<FakeClient>),
    Store(ObjectStore),
}

impl CellNode {
    /// The node as a server, if it is one.
    #[must_use]
    pub fn server(&self) -> Option<&Server> {
        if let Self::Server(s) = self {
            Some(s)
        } else {
            None
        }
    }

    /// The node as a daemon, if it is one.
    #[must_use]
    pub fn daemon(&self) -> Option<&FakeDaemon> {
        if let Self::Daemon(d) = self {
            Some(d)
        } else {
            None
        }
    }

    /// The node as a client, if it is one.
    #[must_use]
    pub fn client(&self) -> Option<&FakeClient> {
        if let Self::Client(c) = self {
            Some(c)
        } else {
            None
        }
    }

    /// The node as the object store, if it is it.
    #[must_use]
    pub fn store(&self) -> Option<&ObjectStore> {
        if let Self::Store(s) = self {
            Some(s)
        } else {
            None
        }
    }
}

impl StateMachine for CellNode {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        match self {
            Self::Server(s) => s.apply(input),
            Self::Daemon(d) => d.apply(input),
            Self::Client(c) => c.apply(input),
            Self::Store(s) => s.apply(input),
        }
    }
}

impl Node for CellNode {
    type Msg = Msg;

    fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        match self {
            Self::Server(s) => s.take_outputs(),
            Self::Daemon(d) => d.take_outputs(),
            Self::Client(c) => c.take_outputs(),
            Self::Store(s) => s.take_outputs(),
        }
    }
}

pub(crate) fn server_id() -> NodeId {
    NodeId::from("server")
}

pub(crate) fn store_id() -> NodeId {
    NodeId::from("store")
}

/// What a run did, for the tests to read.
#[derive(Clone, Debug)]
pub struct RunReport {
    pub trace: TraceHash,
    /// How many times the server opened its log.
    pub starts: u64,
    /// How many results were accepted.
    pub accepted: usize,
    /// How many blobs were answered present.
    pub present: usize,
}

/// Runs `scenario` to its end, checking every property after every step.
///
/// # Panics
///
/// When a property does not hold: the message names the scenario, the seed, the
/// property and how it broke.
#[must_use]
pub fn run(scenario: &Scenario) -> RunReport {
    let mut sim = Sim::new(scenario.seed, scenario.faults.clone());
    sim.add_node(store_id(), CellNode::Store(ObjectStore::default()));
    let server = Server::new(scenario.server.clone(), store_id());
    sim.add_node(server_id(), CellNode::Server(Box::new(server)));
    for n in 0..scenario.daemons {
        let daemon = FakeDaemon::new(
            server_id(),
            format!("daemon-{n}-1"),
            Resources::new(2_000, 4 << 30),
        );
        sim.add_node(
            NodeId::new(format!("daemon-{n}")),
            CellNode::Daemon(Box::new(daemon)),
        );
    }
    for (n, jobs) in scenario.clients.iter().enumerate() {
        let jobs = jobs.iter().map(|(a, at)| (action(*a), *at));
        let client = FakeClient::new(server_id(), jobs);
        sim.add_node(
            NodeId::new(format!("client-{n}")),
            CellNode::Client(Box::new(client)),
        );
    }
    if let Some((n, at)) = scenario.partition {
        let cut = vec![NodeId::new(format!("daemon-{n}"))];
        sim.partition_at(at, Partition::new([cut]));
        sim.partition_at(at.saturating_add(CUT_FOR), Partition::none());
    }
    let mut checker = Checker::new(scenario);
    while sim.now() <= scenario.end {
        // The server's tick timer keeps the queue from ever running dry.
        let _ = sim.step();
        checker.after_step(&sim);
    }
    checker.at_end(&sim);
    let seen = &crate::check::server(&sim).seen;
    RunReport {
        trace: sim.trace_hash(),
        starts: seen.starts,
        accepted: seen.accepted.values().filter(|n| **n > 0).count(),
        present: seen.present.len(),
    }
}
