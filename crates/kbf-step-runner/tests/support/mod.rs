//! Shared by the tests: a scratch directory, a clock that moves only when told to, a launcher
//! that plays a script, and a rig that stages two releases and builds the order.
#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kbf_step_runner::record::CrashGate;
use kbf_step_runner::{
    Bounds, Clock, LaunchError, RecordStore, Staged, StepCall, StepExit, StepLauncher, StepRunner,
    UpdateOrder,
};
use update_hook::{Params, Step};

/// A directory under the target directory, unique to the test, removed when it is dropped.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ksr-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A clock that moves only when a test or a `sleep_secs` moves it.
pub struct FakeClock {
    mono: AtomicU64,
    boot: Mutex<String>,
    pub sleeps: Mutex<Vec<u64>>,
}

impl FakeClock {
    pub fn new() -> Arc<FakeClock> {
        Arc::new(FakeClock {
            mono: AtomicU64::new(1000),
            boot: Mutex::new("boot-a".to_owned()),
            sleeps: Mutex::new(Vec::new()),
        })
    }

    pub fn advance(&self, secs: u64) {
        self.mono.fetch_add(secs, Ordering::SeqCst);
    }

    pub fn reboot(&self, id: &str) {
        *self.boot.lock().unwrap() = id.to_owned();
    }

    pub fn slept(&self) -> Vec<u64> {
        self.sleeps.lock().unwrap().clone()
    }
}

impl Clock for FakeClock {
    fn mono_secs(&self) -> u64 {
        self.mono.load(Ordering::SeqCst)
    }

    fn boot_id(&self) -> String {
        self.boot.lock().unwrap().clone()
    }

    fn sleep_secs(&self, secs: u64) {
        self.sleeps.lock().unwrap().push(secs);
        self.advance(secs);
    }
}

/// What the fake hook does in one run.
#[derive(Clone, Debug)]
pub enum Reply {
    /// Exits with the code, the last line on standard output.
    Exit(i32, Option<&'static str>),
    /// Killed by its bound or a signal: no code.
    Killed,
    /// The launcher cannot start the hook.
    NoStart,
    /// The updater dies while the step's unit lives: the step's work is done, the launcher
    /// reports the failure of the updater's own side.
    Lost,
}

pub const OK: Reply = Reply::Exit(0, None);
pub const FAIL: Reply = Reply::Exit(1, None);
pub const DEFER: Reply = Reply::Exit(75, Some(r#"{"retry_after_secs":100}"#));
pub const HALT: Reply = Reply::Exit(76, None);
pub const IRREVERSIBLE_OK: Reply = Reply::Exit(0, Some(r#"{"reversible":false}"#));
pub const PENDING: Reply = Reply::Exit(0, Some(r#"{"promotion_pending":true}"#));
pub const ENV_FAIL: Reply =
    Reply::Exit(1, Some(r#"{"class":"environment","reason":"remote_down"}"#));

/// One thing the runner asked of the launcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Run {
        step: Step,
        unit: String,
        executable: PathBuf,
        timeout_secs: u64,
        artifact: Option<PathBuf>,
        params_file: PathBuf,
        params: String,
    },
    Stop(String),
}

type Effect = Box<dyn Fn(&StepCall<'_>) + Send + Sync>;
type Decide = Box<dyn Fn(Step) -> Option<Reply> + Send + Sync>;

/// A launcher that plays a script: per step, the replies in order, then success.
#[derive(Default)]
pub struct Fake {
    pub log: Mutex<Vec<Event>>,
    script: Mutex<BTreeMap<Step, VecDeque<Reply>>>,
    effect: Mutex<Option<Effect>>,
    decide: Mutex<Option<Decide>>,
    stop_failures: Mutex<u32>,
    /// Hands out run numbers so a test can name the run that is lost.
    runs: AtomicU64,
    pub lose_run: AtomicU64,
}

impl Fake {
    pub fn new() -> Arc<Fake> {
        Arc::new(Fake::default())
    }

    pub fn say(&self, step: Step, replies: &[Reply]) {
        self.script
            .lock()
            .unwrap()
            .insert(step, replies.iter().cloned().collect());
    }

    /// Runs `f` at the start of every run (it may change the world or the clock).
    pub fn on_run(&self, f: impl Fn(&StepCall<'_>) + Send + Sync + 'static) {
        *self.effect.lock().unwrap() = Some(Box::new(f));
    }

    /// Answers a step from the state of the world; `None` falls back to the script.
    pub fn reply_with(&self, f: impl Fn(Step) -> Option<Reply> + Send + Sync + 'static) {
        *self.decide.lock().unwrap() = Some(Box::new(f));
    }

    pub fn fail_stops(&self, n: u32) {
        *self.stop_failures.lock().unwrap() = n;
    }

    pub fn events(&self) -> Vec<Event> {
        self.log.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.log.lock().unwrap().clear();
    }

    /// The steps run, in order.
    pub fn steps(&self) -> Vec<Step> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Run { step, .. } => Some(step),
                Event::Stop(_) => None,
            })
            .collect()
    }

    pub fn stops(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Stop(unit) => Some(unit),
                Event::Run { .. } => None,
            })
            .collect()
    }

    pub fn runs(&self) -> Vec<Event> {
        self.events()
            .into_iter()
            .filter(|e| matches!(e, Event::Run { .. }))
            .collect()
    }
}

impl StepLauncher for Fake {
    fn run(&self, call: &StepCall<'_>) -> Result<StepExit, LaunchError> {
        let n = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        let params_file = call.invocation.params.clone().expect("a params file");
        self.log.lock().unwrap().push(Event::Run {
            step: call.invocation.step,
            unit: call.unit.to_owned(),
            executable: call.executable.to_path_buf(),
            timeout_secs: call.timeout_secs,
            artifact: call.invocation.artifact.clone(),
            params: fs::read_to_string(&params_file).expect("the params file exists"),
            params_file,
        });
        if let Some(effect) = &*self.effect.lock().unwrap() {
            effect(call);
        }
        if self.lose_run.load(Ordering::SeqCst) == n {
            return Err(LaunchError::Launcher("the updater died".to_owned()));
        }
        let step = call.invocation.step;
        let decided = self.decide.lock().unwrap().as_ref().and_then(|f| f(step));
        let reply = decided
            .or_else(|| {
                let mut script = self.script.lock().unwrap();
                script.get_mut(&step).and_then(VecDeque::pop_front)
            })
            .unwrap_or(OK);
        match reply {
            Reply::Exit(code, line) => Ok(StepExit {
                code: Some(code),
                last_line: line.map(str::to_owned),
            }),
            Reply::Killed => Ok(StepExit {
                code: None,
                last_line: None,
            }),
            Reply::NoStart => Err(LaunchError::NoStart("no such file".to_owned())),
            Reply::Lost => Err(LaunchError::Launcher("the updater died".to_owned())),
        }
    }

    fn stop(&self, unit: &str) -> Result<(), LaunchError> {
        self.log.lock().unwrap().push(Event::Stop(unit.to_owned()));
        let mut left = self.stop_failures.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return Err(LaunchError::Launcher("systemd refused".to_owned()));
        }
        Ok(())
    }
}

/// Two staged releases, a state directory, a clock and a launcher.
pub struct Rig {
    pub scratch: Scratch,
    pub clock: Arc<FakeClock>,
    pub fake: Arc<Fake>,
    pub new_dir: PathBuf,
    pub old_dir: PathBuf,
    pub state: PathBuf,
}

impl Rig {
    pub fn new(name: &str) -> Rig {
        let scratch = Scratch::new(name);
        let root = scratch.path().to_path_buf();
        let (new_dir, old_dir, state) = (root.join("new"), root.join("old"), root.join("state"));
        for (dir, text) in [(&new_dir, "new"), (&old_dir, "old")] {
            fs::create_dir_all(dir.join("lib")).unwrap();
            fs::write(dir.join("hook"), format!("#!hook {text}\n")).unwrap();
            fs::write(dir.join("lib").join("data"), text).unwrap();
        }
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        Rig {
            scratch,
            clock: FakeClock::new(),
            fake: Fake::new(),
            new_dir,
            old_dir,
            state,
        }
    }

    pub fn bounds() -> Bounds {
        Bounds {
            prepare_secs: 600,
            cutover_secs: 300,
            verify_secs: 1000,
            rollback_secs: 600,
            cleanup_secs: 1300,
            verify_deadline_secs: 1600,
            promotion_ceiling_secs: 3600,
            cleanup_retries: 3,
        }
    }

    pub fn order(&self) -> UpdateOrder {
        let mut declared = Params::default();
        declared.insert("promotion_bound_secs", update_hook::Scalar::Integer(900));
        UpdateOrder {
            service: "svc".to_owned(),
            target_version: "2.0.0".to_owned(),
            previous_version: Some("1.0.0".to_owned()),
            artifact_digest: "ab".repeat(32),
            staged: Staged::seal(&self.new_dir).unwrap(),
            previous: Some(Staged::seal(&self.old_dir).unwrap()),
            hook_binary: PathBuf::from("hook"),
            bounds: Rig::bounds(),
            declared_params: declared,
            min_updater_protocol: None,
        }
    }

    pub fn store(&self) -> RecordStore {
        RecordStore::new(&self.state)
    }

    pub fn runner(&self) -> StepRunner {
        self.runner_with(self.store())
    }

    pub fn runner_gated(&self, gate: CrashGate) -> StepRunner {
        self.runner_with(self.store().with_crash_gate(gate))
    }

    fn runner_with(&self, store: RecordStore) -> StepRunner {
        StepRunner::new(self.fake.clone(), self.clock.clone(), store)
    }
}

pub fn io_err() -> io::Error {
    io::Error::other("failure")
}
