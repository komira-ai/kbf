//! A host for the frontend tests: a real file tree under a scratch root (the links, the
//! state and `frontend.json` are real files) with systemd and the instances' `/readyz`
//! played by [`Sim`], which records every `systemctl` it is asked and answers from a model
//! of the two units. The executor can also be killed before or after any step.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::rc::Rc;

use kbf_host::testkit::{ScratchHost, reply};
use kbf_host::trust::Untrusted;
use kbf_host::{Cmd, Entry, Exec, HostPath, HttpUrl, Meta, Root, Step};
use kbf_update::{Facts, Locator, OrderRecord, Verb};

use crate::frontend::pending::Instance;
use crate::frontend::probe::HandoverProbe;
use crate::frontend::{EnvoyGate, Frontend, FrontendConfig};
use crate::helper::cycle::{Config, Helper};
use crate::helper::fetcher::fake::FakeFetcher;
use crate::helper::leases::fake::FakeLeases;
use crate::helper::restart::Restart;
use crate::layout::Layout;
use crate::state::{Locked, State};
use crate::testkit::{A, c};

pub const T0: u64 = 1_800_000_000;
pub const NODE_UNIT: &str = "kbf.service";
pub const PORT_A: u16 = 18_081;
pub const PORT_B: u16 = 18_082;

/// What a unit does when it is started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// `start` returns: the unit is active and its `/readyz` answers.
    Ready,
    /// `start` fails and the unit stays where it was.
    Fails,
    /// `start` returns and the unit is active, but its `/readyz` never answers.
    NeverReady,
}

pub struct Unit {
    /// systemd's `ActiveState` word.
    pub state: &'static str,
    pub ready: bool,
}

pub struct Sim {
    pub units: [Unit; 2],
    /// Every `systemctl` asked of it, `start a`, `stop b`, in order.
    pub asked: Vec<String>,
    pub start: [Start; 2],
    /// A stop that leaves the unit `deactivating` for good (an instance with streams that
    /// never end).
    pub stop_hangs: [bool; 2],
    /// Starting an instance ready makes the other stand down: still `active`, not ready,
    /// as a holder that handed over is.
    pub stands_down: bool,
    /// `systemctl show` of the instance fails (the bus is unreachable).
    pub unreadable: [bool; 2],
    /// Every command, as `systemctl` words, so a test can see that nothing else ran.
    pub other_runs: Vec<String>,
}

fn at(i: Instance) -> usize {
    match i {
        Instance::A => 0,
        Instance::B => 1,
    }
}

impl Sim {
    /// Instance `serving` serves; the other is inactive.
    pub fn new(serving: Instance) -> Sim {
        let mut sim = Sim {
            units: [
                Unit {
                    state: "inactive",
                    ready: false,
                },
                Unit {
                    state: "inactive",
                    ready: false,
                },
            ],
            asked: Vec::new(),
            start: [Start::Ready; 2],
            stop_hangs: [false; 2],
            stands_down: true,
            unreadable: [false; 2],
            other_runs: Vec::new(),
        };
        sim.units[at(serving)] = Unit {
            state: "active",
            ready: true,
        };
        sim
    }

    pub fn unit(&mut self, i: Instance) -> &mut Unit {
        &mut self.units[at(i)]
    }

    /// The instance dies (a crash, or the process ended on its own).
    pub fn die(&mut self, i: Instance) {
        *self.unit(i) = Unit {
            state: "failed",
            ready: false,
        };
    }

    /// A draining instance has closed its last stream and exited.
    pub fn exits(&mut self, i: Instance) {
        *self.unit(i) = Unit {
            state: "inactive",
            ready: false,
        };
    }

    pub fn count(&self, what: &str) -> usize {
        self.asked.iter().filter(|a| *a == what).count()
    }

    fn start_unit(&mut self, i: Instance) -> io::Result<()> {
        match self.start[at(i)] {
            Start::Fails => Err(io::Error::other("systemctl start failed")),
            Start::NeverReady => {
                *self.unit(i) = Unit {
                    state: "active",
                    ready: false,
                };
                Ok(())
            }
            Start::Ready => {
                *self.unit(i) = Unit {
                    state: "active",
                    ready: true,
                };
                if self.stands_down {
                    let other = self.unit(i.other());
                    if other.state == "active" {
                        other.ready = false;
                    }
                }
                Ok(())
            }
        }
    }

    fn stop_unit(&mut self, i: Instance) {
        let hangs = self.stop_hangs[at(i)];
        *self.unit(i) = Unit {
            state: if hangs { "deactivating" } else { "inactive" },
            ready: false,
        };
    }
}

fn instance_of(unit: &str) -> Option<Instance> {
    Instance::ALL.into_iter().find(|i| i.unit() == unit)
}

/// The host: real files, played systemd, and an optional kill point.
pub struct SimExec {
    pub host: Rc<RefCell<ScratchHost>>,
    pub sim: Rc<RefCell<Sim>>,
    pub steps: Rc<Cell<usize>>,
    /// 0 is never; `2n - 1` kills before step `n`, `2n` after it.
    pub kill_at: Rc<Cell<usize>>,
}

impl Exec for SimExec {
    fn euid(&self) -> u32 {
        0
    }

    fn inspect(&self, p: &HostPath) -> io::Result<Option<Entry>> {
        self.host.borrow().inspect(p)
    }

    fn stat(&self, p: &HostPath) -> io::Result<Option<Meta>> {
        self.host.borrow().stat(p)
    }

    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        // `systemctl show --property=ActiveState --value -- <unit>`
        let unit = cmd.args.last().map(String::as_str).unwrap_or_default();
        let i = instance_of(unit).expect("a frontend unit");
        let mut sim = self.sim.borrow_mut();
        if sim.unreadable[at(i)] {
            return Ok(reply(1, "", "no bus"));
        }
        let state = sim.unit(i).state;
        Ok(reply(0, &format!("{state}\n"), ""))
    }

    fn http_status(&self, url: &HttpUrl) -> io::Result<u16> {
        let i = if url.port() == PORT_A {
            Instance::A
        } else {
            Instance::B
        };
        // What the instance's own `/readyz` says: only a running one answers, and the model
        // keeps `ready` false for one that is not.
        Ok(if self.sim.borrow_mut().unit(i).ready {
            200
        } else {
            503
        })
    }

    fn apply(&mut self, step: &Step) -> io::Result<()> {
        let n = self.steps.get() + 1;
        self.steps.set(n);
        let at = self.kill_at.get();
        assert!(
            !(at != 0 && at % 2 == 1 && n == at.div_ceil(2)),
            "killed before step {n}"
        );
        let result = self.step(step);
        assert!(
            !(at != 0 && at.is_multiple_of(2) && n == at / 2),
            "killed after step {n}"
        );
        result
    }

    fn check_binary(&self) -> Result<(), Untrusted> {
        Ok(())
    }
}

impl SimExec {
    fn step(&mut self, step: &Step) -> io::Result<()> {
        let Step::Run(cmd) = step else {
            return self.host.borrow_mut().apply(step);
        };
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        let mut sim = self.sim.borrow_mut();
        match args.as_slice() {
            ["start", "--", unit] => {
                let i = instance_of(unit).expect("a frontend unit");
                sim.asked.push(format!("start {}", i.name()));
                sim.start_unit(i)
            }
            ["stop", "--no-block", "--", unit] => {
                let i = instance_of(unit).expect("a frontend unit");
                sim.asked.push(format!("stop {}", i.name()));
                sim.stop_unit(i);
                Ok(())
            }
            other => {
                sim.other_runs.push(other.join(" "));
                Ok(())
            }
        }
    }
}

/// An Envoy gate a test sets.
#[derive(Clone)]
pub struct FakeGate(pub Rc<RefCell<Result<(), String>>>);

impl EnvoyGate for FakeGate {
    fn check(&self) -> Result<(), String> {
        self.0.borrow().clone()
    }
}

/// A handover probe a test sets: the answer for a release's binary and for a slot's.
#[derive(Clone)]
pub struct FakeProbe {
    pub new: Rc<RefCell<Result<u64, String>>>,
    pub old: Rc<RefCell<Result<u64, String>>>,
    pub asked: Rc<RefCell<Vec<PathBuf>>>,
}

impl HandoverProbe for FakeProbe {
    fn handover_protocol(&self, binary: &Path) -> Result<u64, String> {
        self.asked.borrow_mut().push(binary.to_owned());
        let slot = binary.components().any(|c| c.as_os_str() == "slots");
        if slot {
            self.old.borrow().clone()
        } else {
            self.new.borrow().clone()
        }
    }
}

pub fn cfg() -> FrontendConfig {
    FrontendConfig {
        readyz_a: HttpUrl::new(&format!("http://127.0.0.1:{PORT_A}/readyz")).expect("url"),
        readyz_b: HttpUrl::new(&format!("http://127.0.0.1:{PORT_B}/readyz")).expect("url"),
        window_s: 600,
        looks: 3,
        revert_bound_s: 300,
    }
}

/// The record the frontend writes into its own host's spool.
pub fn record(id: u64, epoch: u64, generation: u64, commit: &str) -> OrderRecord {
    OrderRecord {
        order_id: id,
        epoch,
        generation,
        verb: Verb::FrontendUpdate,
        commit: commit.to_owned(),
        window_s: 600,
        components: vec![Locator {
            name: "kbf".to_owned(),
            sha256: "ab".repeat(32),
            size: 1000,
            attestation: "{\"bundle\":1}".to_owned(),
        }],
        expected: None::<Facts>,
    }
}

/// A host where instance `serving` serves release A, both slots and `current` are on A
/// (the idle one on its prior release), and release B is the one to update to.
pub struct World {
    pub exec_host: Rc<RefCell<ScratchHost>>,
    pub sim: Rc<RefCell<Sim>>,
    pub root: PathBuf,
    pub layout: Layout,
    pub steps: Rc<Cell<usize>>,
    pub kill_at: Rc<Cell<usize>>,
    pub fetcher: FakeFetcher,
    pub leases: FakeLeases,
    pub gate: FakeGate,
    pub probe: FakeProbe,
}

pub const UPDATE: &str = "var/lib/kbf-installer";

impl World {
    pub fn new(name: &str, serving: Instance) -> World {
        let host = ScratchHost::new(&format!("frontend-{name}"));
        let root = host.root.clone();
        for sub in [
            "run",
            "var/lib/kbf-deploy",
            &format!("{UPDATE}/orders"),
            "usr/local/kbf/releases",
            "usr/local/kbf/slots",
        ] {
            fs::create_dir_all(root.join(sub)).expect("seed");
        }
        let releases = root.join("usr/local/kbf/releases");
        let dir = releases.join(A);
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), b"binary").expect("binary");
        let slots = root.join("usr/local/kbf/slots");
        let to_a = format!("../releases/{A}");
        std::os::unix::fs::symlink(&to_a, slots.join(serving.name())).expect("slot");
        std::os::unix::fs::symlink(&to_a, slots.join(serving.other().name())).expect("slot");
        let layout = Layout::linux(Root::new(&root).expect("absolute"));
        Locked::acquire(&layout)
            .expect("lock")
            .write(&State {
                current: Some(c(A)),
                ..State::default()
            })
            .expect("state");
        World {
            fetcher: FakeFetcher::new(&layout),
            leases: FakeLeases::default(),
            gate: FakeGate(Rc::new(RefCell::new(Ok(())))),
            probe: FakeProbe {
                new: Rc::new(RefCell::new(Ok(1))),
                old: Rc::new(RefCell::new(Ok(1))),
                asked: Rc::default(),
            },
            exec_host: Rc::new(RefCell::new(host)),
            sim: Rc::new(RefCell::new(Sim::new(serving))),
            root,
            layout,
            steps: Rc::new(Cell::new(0)),
            kill_at: Rc::new(Cell::new(0)),
        }
    }

    /// A helper (a fresh process) on this host.
    pub fn helper(&self) -> Helper<SimExec> {
        let exec = SimExec {
            host: self.exec_host.clone(),
            sim: self.sim.clone(),
            steps: self.steps.clone(),
            kill_at: self.kill_at.clone(),
        };
        let config = Config {
            update_dir: HostPath::new("/var/lib/kbf-installer").expect("path"),
            orders_dir: HostPath::new("/var/lib/kbf-installer/orders").expect("path"),
            restart: Restart::Systemd(NODE_UNIT.to_owned()),
            window_secs: 600,
            starts: 3,
        };
        Helper::new(
            self.layout.clone(),
            exec,
            config,
            Box::new(self.fetcher.clone()),
            Box::new(self.leases.clone()),
        )
        .with_frontend(Frontend::new(
            cfg(),
            Box::new(self.gate.clone()),
            Box::new(self.probe.clone()),
        ))
    }

    pub fn put(&self, r: &OrderRecord) {
        fs::write(
            self.root.join(UPDATE).join("orders").join(r.file_name()),
            r.to_bytes().expect("bytes"),
        )
        .expect("record");
    }

    pub fn put_node_activate(&self, id: u64, commit: &str) {
        let mut r = record(id, 1, 1, commit);
        r.verb = Verb::Activate;
        r.expected = Some(Facts {
            platform: "linux-x86_64".to_owned(),
            protocol: 7,
            min_protocol: 6,
            state_schema: 3,
            profile: "release".to_owned(),
        });
        self.put(&r);
    }

    pub fn left(&self) -> usize {
        fs::read_dir(self.root.join(UPDATE).join("orders"))
            .expect("dir")
            .count()
    }

    pub fn slot(&self, i: Instance) -> Option<String> {
        fs::read_link(self.root.join("usr/local/kbf/slots").join(i.name()))
            .ok()
            .map(|p| p.display().to_string())
    }

    pub fn state(&self) -> State {
        crate::state::read_unlocked(&self.layout)
            .expect("reads")
            .expect("there")
    }

    pub fn file(&self) -> crate::frontend::pending::FrontendFile {
        crate::frontend::pending::load(&self.layout).expect("frontend.json")
    }

    pub fn status(&self) -> serde_json::Value {
        let bytes = fs::read(self.root.join(UPDATE).join("status.json")).expect("status");
        serde_json::from_slice(&bytes).expect("json")
    }

    pub fn refusal(&self) -> String {
        self.status()["refusal"].as_str().expect("text").to_owned()
    }

    /// Release B as a fetch leaves it is the fetcher's doing; this says it is wanted.
    pub fn asked(&self) -> Vec<String> {
        self.sim.borrow().asked.clone()
    }
}

pub fn to(commit: &str) -> String {
    format!("../releases/{commit}")
}
