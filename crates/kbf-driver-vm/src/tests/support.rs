//! A CAS in memory, actions stored in it, and a VM driver over a fake host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use crate::clock::{Clock, Sleep};
use crate::{FakeHost, VmConfig, VmDriver};
use kbf_daemon::Work;
use kbf_daemon::cas::{Cas, CasError, digest_of, label};
use kbf_proto::reapi::command::EnvironmentVariable;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{Action, Command, Digest, Directory, FileNode, Platform};
use kbf_types::{LeaseId, Resources};
use prost::Message;

pub const GIB: u64 = 1 << 30;

/// A CAS in memory; a gated one holds every `get` back, for ever, and counts them (to
/// kill a lease mid-fetch).
#[derive(Debug, Default)]
pub struct MemoryCas {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    gate: Option<Gate>,
}

/// A gate no one opens: a `get` that reaches it is counted and waits.
#[derive(Debug)]
struct Gate {
    held: AtomicUsize,
    shut: tokio::sync::Semaphore,
}

impl MemoryCas {
    /// A CAS whose every `get` waits at a gate that stays shut.
    pub fn gated() -> Self {
        Self {
            gate: Some(Gate {
                held: AtomicUsize::new(0),
                shut: tokio::sync::Semaphore::new(0),
            }),
            ..Self::default()
        }
    }

    /// How many `get`s wait at the gate.
    pub fn held(&self) -> usize {
        self.gate
            .as_ref()
            .map_or(0, |gate| gate.held.load(Ordering::SeqCst))
    }

    pub fn insert(&self, bytes: impl Into<Vec<u8>>) -> Digest {
        let bytes = bytes.into();
        let digest = digest_of(&bytes);
        self.lock().insert(label(&digest), bytes);
        digest
    }

    /// Replaces the bytes stored under `digest`, which no longer match it.
    pub fn corrupt(&self, digest: &Digest) {
        self.lock()
            .insert(label(digest), b"not these bytes".to_vec());
    }

    pub fn remove(&self, digest: &Digest) {
        self.lock().remove(&label(digest));
    }

    pub fn blob(&self, digest: &Digest) -> Vec<u8> {
        self.lock()
            .get(&label(digest))
            .cloned()
            .unwrap_or_else(|| panic!("blob {} is stored", label(digest)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.blobs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Cas for MemoryCas {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        if let Some(gate) = &self.gate {
            gate.held.fetch_add(1, Ordering::SeqCst);
            // Shut for good: this ends only when the `get` is dropped.
            let _ = gate.shut.acquire().await;
        }
        self.lock()
            .get(&label(digest))
            .cloned()
            .ok_or_else(|| CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        Ok(self.insert(bytes))
    }
}

/// An action to store: `sh -c <script>` with an empty input root unless changed.
#[derive(Clone, Debug, Default)]
pub struct Spec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub outputs: Vec<String>,
    pub working_directory: String,
    pub inputs: Vec<(String, Vec<u8>)>,
    pub properties: Vec<(String, String)>,
    pub timeout: Option<Duration>,
}

impl Spec {
    /// The test binary's own act steps (`kbf-testkit`'s `kbf-act`), as the action's
    /// command: no shell, and the same program on a CI runner and on a Mac.
    pub fn act(steps: &[&str]) -> Self {
        let mut argv = vec![act_path()];
        argv.extend(steps.iter().map(|s| (*s).to_owned()));
        Self {
            argv,
            ..Self::argv(&[])
        }
    }

    /// A command of lease `1.seq` that waits until the test opens its gate
    /// ([`open_gate`]) and then succeeds: it runs for exactly as long as the test says.
    /// The hour it waits at most is not a time the tests depend on: it ends the
    /// command if a test never opens the gate.
    pub fn held(config: &VmConfig, seq: u64) -> Self {
        let gate = gate(config, seq);
        Self::act(&["wait-for", &gate.display().to_string(), "3600"])
    }

    /// An action that does nothing and succeeds.
    pub fn ok() -> Self {
        Self::act(&["exit", "0"])
    }

    pub fn argv(argv: &[&str]) -> Self {
        Self {
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            // A VM lease runs without the network, which a lease that says nothing
            // has: so these actions say so.
            properties: vec![("network".to_owned(), "off".to_owned())],
            ..Self::default()
        }
    }

    pub fn env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn outputs(mut self, outputs: &[&str]) -> Self {
        self.outputs = outputs.iter().map(|o| (*o).to_owned()).collect();
        self
    }

    pub fn working_directory(mut self, dir: &str) -> Self {
        dir.clone_into(&mut self.working_directory);
        self
    }

    pub fn input(mut self, name: &str, bytes: &[u8]) -> Self {
        self.inputs.push((name.to_owned(), bytes.to_vec()));
        self
    }

    pub fn property(mut self, name: &str, value: &str) -> Self {
        self.properties.retain(|(have, _)| have != name);
        self.properties.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Stores the action in `cas` and returns its digest.
    pub fn store(&self, cas: &MemoryCas) -> Digest {
        let root = Directory {
            files: (self.inputs.iter())
                .map(|(name, bytes)| FileNode {
                    name: name.clone(),
                    digest: Some(cas.insert(bytes.clone())),
                    ..FileNode::default()
                })
                .collect(),
            ..Directory::default()
        };
        let command = Command {
            arguments: self.argv.clone(),
            environment_variables: (self.env.iter())
                .map(|(name, value)| EnvironmentVariable {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            output_paths: self.outputs.clone(),
            working_directory: self.working_directory.clone(),
            ..Command::default()
        };
        let action = Action {
            command_digest: Some(cas.insert(command.encode_to_vec())),
            input_root_digest: Some(cas.insert(root.encode_to_vec())),
            timeout: self.timeout.map(|t| prost_types::Duration {
                seconds: i64::try_from(t.as_secs()).expect("seconds"),
                nanos: i32::try_from(t.subsec_nanos()).expect("nanos"),
            }),
            platform: (!self.properties.is_empty()).then(|| Platform {
                properties: (self.properties.iter())
                    .map(|(name, value)| Property {
                        name: name.clone(),
                        value: value.clone(),
                    })
                    .collect(),
            }),
            ..Action::default()
        };
        cas.insert(action.encode_to_vec())
    }
}

/// The directory the tests' scratch directories are made in, beside the test binary
/// (inside the target directory).
fn unit_root() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    exe.parent()
        .expect("deps directory")
        .join("kbf-driver-vm-unit")
}

/// A link to the test binary named for the act steps, made once per run of the binary
/// (the guest runs the command by this path).
pub fn act_path() -> String {
    static LINK: OnceLock<PathBuf> = OnceLock::new();
    LINK.get_or_init(|| {
        let dir = unit_root()
            .join("kbf-driver-vm")
            .join(format!("act-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("act directory");
        let link = dir.join(kbf_testkit::act::NAME);
        let _ = std::fs::remove_file(&link);
        kbf_testkit::act::link_self(&link).expect("link the test binary");
        link
    })
    .display()
    .to_string()
}

/// A fresh scratch directory for one test, holding a golden image (`image/`, one disk
/// file) and the driver's scratch root (`vm/`).
pub fn scratch(name: &str) -> PathBuf {
    let dir = unit_root()
        .join("kbf-driver-vm")
        .join(format!("{name}-{}", std::process::id()));
    if std::fs::symlink_metadata(&dir).is_ok() {
        kbf_outputs::remove_tree(&dir).expect("clear old scratch");
    }
    std::fs::create_dir_all(dir.join("image/data")).expect("image");
    std::fs::write(dir.join("image/disk.img"), b"golden").expect("disk");
    std::fs::write(dir.join("image/data/aux.img"), b"aux").expect("aux");
    dir
}

/// A configuration for tests: a fast boot poll and no free-space floor (a CI runner's
/// disk is small).
pub fn config(scratch: &Path) -> VmConfig {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mut config = VmConfig::new(scratch.join("image"), scratch.join("vm"));
    config.boot_poll = Duration::from_millis(10);
    config.min_free_bytes = 0;
    config
}

pub type Driver = VmDriver<FakeHost, MemoryCas>;

pub fn driver(config: VmConfig, host: &Arc<FakeHost>, cas: &Arc<MemoryCas>) -> Arc<Driver> {
    Arc::new(VmDriver::new(config, Arc::clone(host), Arc::clone(cas)).expect("driver"))
}

/// A driver whose boot poll and report watchdog read `clock`.
pub fn driver_on(
    config: VmConfig,
    host: &Arc<FakeHost>,
    cas: &Arc<MemoryCas>,
    clock: &Arc<TestClock>,
) -> Arc<Driver> {
    let clock = Arc::clone(clock);
    let driver = VmDriver::with_clock(config, Arc::clone(host), Arc::clone(cas), clock);
    Arc::new(driver.expect("driver"))
}

/// Called with the number of each pause, just before it is made.
type OnPause = Box<dyn Fn(u32) + Send + Sync>;

/// A clock whose time is a counter. A pause moves it on by the pause, at once, unless the
/// clock is held, when the pause waits for [`TestClock::release`] and time stands still;
/// a sleep ends when a test has moved the time to its deadline ([`TestClock::advance`]).
/// It records every pause and the span of every sleep asked of it, so a test reads what
/// the driver waited for instead of timing it.
#[derive(Default)]
pub struct TestClock {
    state: Mutex<ClockState>,
    released: Condvar,
    on_pause: Mutex<Option<OnPause>>,
}

#[derive(Default)]
struct ClockState {
    now: Duration,
    held: bool,
    pauses: u32,
    sleeps: Vec<Duration>,
    waiting: Vec<(Duration, tokio::sync::oneshot::Sender<()>)>,
}

impl std::fmt::Debug for TestClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestClock").finish_non_exhaustive()
    }
}

impl TestClock {
    /// A clock on which a pause passes at once.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A clock on which a pause waits, with time standing still, for [`release`].
    ///
    /// [`release`]: TestClock::release
    pub fn held() -> Arc<Self> {
        let clock = Self::default();
        clock.lock().held = true;
        Arc::new(clock)
    }

    /// Lets every pause, waiting or later, pass at once.
    pub fn release(&self) {
        self.lock().held = false;
        self.released.notify_all();
    }

    /// Calls `hook` with the number of each pause just before it is made.
    pub fn on_pause(&self, hook: impl Fn(u32) + Send + Sync + 'static) {
        *self.on_pause.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// Moves time on by `by` and ends every sleep whose deadline it reaches.
    pub fn advance(&self, by: Duration) {
        let mut state = self.lock();
        state.now += by;
        let now = state.now;
        let (due, later) = std::mem::take(&mut state.waiting)
            .into_iter()
            .partition(|(deadline, _)| *deadline <= now);
        state.waiting = later;
        for (_, wake) in due {
            let _ = wake.send(());
        }
    }

    /// How many pauses were made or are waiting.
    pub fn pauses(&self) -> u32 {
        self.lock().pauses
    }

    /// The span of every sleep asked for, in order.
    pub fn sleeps(&self) -> Vec<Duration> {
        self.lock().sleeps.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ClockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Clock for TestClock {
    fn now(&self) -> Duration {
        self.lock().now
    }

    fn pause(&self, d: Duration) {
        let pause = {
            let mut state = self.lock();
            state.pauses += 1;
            state.pauses
        };
        if let Some(hook) = &*self.on_pause.lock().unwrap_or_else(PoisonError::into_inner) {
            hook(pause);
        }
        let mut state = self.lock();
        while state.held {
            state = self
                .released
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(state);
        self.advance(d);
    }

    fn sleep(&self, d: Duration) -> Sleep {
        let mut state = self.lock();
        state.sleeps.push(d);
        let (wake, woken) = tokio::sync::oneshot::channel();
        let deadline = state.now + d;
        if deadline <= state.now {
            let _ = wake.send(());
        } else {
            state.waiting.push((deadline, wake));
        }
        Box::pin(async move {
            if woken.await.is_err() {
                std::future::pending::<()>().await;
            }
        })
    }
}

/// A `vm` lease booked at the default VM size: 6 cores and 17 GiB.
pub fn work(seq: u64, action_digest: Digest) -> Work {
    Work {
        lease_id: LeaseId::new(1, seq),
        kind: "vm".to_owned(),
        action_digest,
        resources: Resources::new(6000, 17 * GIB),
    }
}

/// The name lease `1.seq` has on the host and on disk.
pub fn name(seq: u64) -> String {
    format!("lease-1-{seq}")
}

/// The outputs share of lease `1.seq`, which the fake guest sees at its host path.
pub fn outputs_share(config: &VmConfig, seq: u64) -> String {
    config
        .leases()
        .join(name(seq))
        .join("outputs")
        .display()
        .to_string()
}

/// Everything in the lease and clone directories.
pub fn leftovers(config: &VmConfig) -> Vec<PathBuf> {
    [config.leases(), config.clones()]
        .iter()
        .flat_map(|dir| std::fs::read_dir(dir).expect("read dir"))
        .map(|entry| entry.expect("entry").path())
        .collect()
}

/// Waits until `done` holds. It has no deadline: the wait is for an event the test has
/// caused, and a test whose event never comes is ended by the test runner, not by a
/// clock of its own. `what` names the event in the log.
pub async fn until(what: &str, done: impl Fn() -> bool) {
    while !done() {
        tracing::debug!(what, "waiting");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// The file whose creation lets a [`Spec::held`] command of lease `1.seq` go on.
pub fn gate(config: &VmConfig, seq: u64) -> PathBuf {
    PathBuf::from(outputs_share(config, seq)).join("go")
}

/// Lets the held command of lease `1.seq` go on.
pub fn open_gate(config: &VmConfig, seq: u64) {
    std::fs::write(gate(config, seq), b"").expect("open the gate");
}

/// Waits until lease `1.seq`'s command has started in its guest: the guest has made its
/// stdout file in the outputs share.
pub async fn until_running(config: &VmConfig, seq: u64) {
    let stdout = PathBuf::from(outputs_share(config, seq)).join("stdout");
    until("the command started", || stdout.exists()).await;
}
