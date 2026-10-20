//! A thread-safe world behind every port of the swap, for tests of the supervisor. The
//! swap runs on a thread of its own and the test watches it from another, so the world is
//! behind a mutex and time is real, with limits of a few milliseconds. Every call is logged in
//! one ordered list, so a test can assert the order of calls across ports.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::super::model::{
    AdoptError, CrashPoint, Halt, Intent, Locator, Mode, OpenCounts, Phase, PortError, ProcIdent,
    Readiness, Retired, RoleInfo, RoleState, Slot, StagedFacts, Start, Timing, verify_ident,
};
use super::super::ports::{Child, ChildAdmin, Clock, Gate, Migrator, Recorder, RootOrders};
use super::super::record::FileRecorder;
use super::super::runtime::RequestQueue;
use super::{Config, Parts};
use crate::CommitId;
use crate::testkit::{A, B, c};

pub struct Proc {
    pub alive: bool,
    pub state: RoleState,
    pub holder: bool,
    pub ident: ProcIdent,
}

/// What a test changes while the swap runs.
pub struct Knobs {
    pub migrate_err: Option<String>,
    pub spawn_panics: bool,
    /// The candidate answers its three probes.
    pub ready: bool,
    pub takeover_err: bool,
    /// The staged commit `status.json` shows.
    pub staged: Option<CommitId>,
    /// `status.json` cannot be read: the staged release and the installed commit both fail.
    pub status_err: bool,
    /// The recorder refuses the record of this phase.
    pub fail_record: Option<Phase>,
    /// SIGTERM to a child fails.
    pub terminate_err: bool,
    /// Streams the proxy has bound to a slot, and the child counts the same.
    pub streams: [u64; 2],
}

pub struct St {
    pub procs: [Option<Proc>; 2],
    pub next_pid: u32,
    pub active: Slot,
    pub held: bool,
    pub knobs: Knobs,
    pub installed: Option<CommitId>,
    pub records: Vec<Intent>,
    pub log: Vec<String>,
}

pub struct Rig {
    st: Mutex<St>,
    /// `update.json` is written here too, so a second supervisor can read it.
    dir: Option<PathBuf>,
    stopped: AtomicBool,
    start: Instant,
}

fn ix(slot: Slot) -> usize {
    match slot {
        Slot::A => 0,
        Slot::B => 1,
    }
}

pub fn quick() -> Timing {
    Timing {
        early_exit: Duration::from_secs(30),
        ready_within: Duration::from_secs(5),
        hold_max: Duration::from_secs(2),
        quiesce: Duration::from_secs(2),
        takeover: Duration::from_secs(2),
        bake: Duration::ZERO,
        confirm_attempts: 3,
        confirm_backoff_max: Duration::from_millis(10),
        poll: Duration::from_millis(2),
        tick: Duration::from_millis(1),
    }
}

fn ident_of(pid: u32, commit: &CommitId) -> ProcIdent {
    ProcIdent {
        pid,
        start_time: u64::from(pid),
        exe: PathBuf::from(format!("/releases/{commit}/kbf")),
    }
}

impl Rig {
    /// Release A serves from slot A; release B is staged. `dir` is where `update.json` goes.
    pub fn new(dir: Option<PathBuf>) -> Arc<Rig> {
        let old = Proc {
            alive: true,
            state: RoleState::Active,
            holder: true,
            ident: ident_of(100, &c(A)),
        };
        Arc::new(Rig {
            st: Mutex::new(St {
                procs: [Some(old), None],
                next_pid: 101,
                active: Slot::A,
                held: false,
                knobs: Knobs {
                    migrate_err: None,
                    spawn_panics: false,
                    ready: true,
                    takeover_err: false,
                    staged: Some(c(B)),
                    status_err: false,
                    fail_record: None,
                    terminate_err: false,
                    streams: [0, 0],
                },
                installed: Some(c(A)),
                records: Vec::new(),
                log: Vec::new(),
            }),
            dir,
            stopped: AtomicBool::new(false),
            start: Instant::now(),
        })
    }

    pub fn st(&self) -> MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn knobs(&self, f: impl FnOnce(&mut Knobs)) {
        f(&mut self.st().knobs);
    }

    pub fn log(&self) -> Vec<String> {
        self.st().log.clone()
    }

    /// The index of the first log line that starts with `prefix`.
    pub fn pos(&self, prefix: &str) -> Option<usize> {
        self.st().log.iter().position(|l| l.starts_with(prefix))
    }

    pub fn count(&self, prefix: &str) -> usize {
        self.st()
            .log
            .iter()
            .filter(|l| l.starts_with(prefix))
            .count()
    }

    pub fn ident(&self, slot: Slot) -> Option<ProcIdent> {
        self.st().procs[ix(slot)].as_ref().map(|p| p.ident.clone())
    }

    pub fn alive(&self, slot: Slot) -> bool {
        self.st().procs[ix(slot)].as_ref().is_some_and(|p| p.alive)
    }

    /// The process in `slot` dies on its own.
    pub fn kill(&self, slot: Slot) {
        if let Some(p) = self.st().procs[ix(slot)].as_mut() {
            p.alive = false;
        }
    }

    pub fn phases(&self) -> Vec<super::super::model::Phase> {
        self.st().records.iter().map(|i| i.phase).collect()
    }

    /// While `on`, every wait of the swap ends with `Interrupted`: the proxy is going away.
    pub fn stop(&self, on: bool) {
        self.stopped.store(on, Ordering::SeqCst);
    }

    fn say(&self, text: impl Into<String>) {
        self.st().log.push(text.into());
    }

    /// The parts of a supervisor over this rig.
    pub fn parts(
        self: &Arc<Rig>,
        requests: &Arc<RequestQueue>,
        on_death: impl Fn(&str) + Send + Sync + 'static,
    ) -> Parts {
        let rig = Arc::clone(self);
        Parts {
            child: self.clone(),
            admin: self.clone(),
            gate: self.clone(),
            migrator: self.clone(),
            recorder: self.clone(),
            orders: self.clone(),
            clock: self.clone(),
            requests: Arc::clone(requests),
            old_proc: Box::new(move |slot| rig.ident(slot)),
            on_death: Box::new(on_death),
        }
    }

    pub fn config(dir: PathBuf) -> Config {
        Config {
            mode: Mode::Frontend,
            timing: quick(),
            state_dir: dir,
            active: Slot::A,
            rollback_after_switch: false,
        }
    }
}

/// The live process in `slot`, or the error a port gives for a slot nothing answers in.
fn proc_mut(st: &mut St, slot: Slot) -> Result<&mut Proc, PortError> {
    st.procs[ix(slot)]
        .as_mut()
        .filter(|p| p.alive)
        .ok_or_else(|| PortError(format!("nothing answers in slot {slot:?}")))
}

/// `slot`'s process holds the generation, and no other does.
fn hold(st: &mut St, slot: Slot) -> Result<(), PortError> {
    let _ = proc_mut(st, slot.other()).map(|p| p.holder = false);
    let p = proc_mut(st, slot)?;
    p.holder = true;
    p.state = RoleState::Active;
    Ok(())
}

impl Child for Rig {
    fn spawn(&self, slot: Slot, release: &CommitId, _start: Start) -> Result<ProcIdent, PortError> {
        assert!(
            !self.st().knobs.spawn_panics,
            "the spawn exploded in the test"
        );
        let mut st = self.st();
        let pid = st.next_pid;
        st.next_pid += 1;
        let ident = ident_of(pid, release);
        // The frontend swap starts candidates only; a start that serves at once is the cold
        // start of a host with no child, which this code does not do.
        st.procs[ix(slot)] = Some(Proc {
            alive: true,
            state: RoleState::Candidate,
            holder: false,
            ident: ident.clone(),
        });
        st.log.push(format!("spawn {slot:?} Candidate"));
        Ok(ident)
    }

    fn adopt(&self, slot: Slot, recorded: &ProcIdent) -> Result<(), AdoptError> {
        match self.st().procs[ix(slot)].as_ref().filter(|p| p.alive) {
            None => Err(AdoptError::Gone(recorded.pid)),
            Some(p) => verify_ident(recorded, &p.ident),
        }
    }

    fn exited(&self, ident: &ProcIdent) -> Option<i32> {
        let live = self
            .st()
            .procs
            .iter()
            .flatten()
            .any(|p| p.alive && p.ident.pid == ident.pid);
        (!live).then_some(0)
    }

    fn terminate(&self, ident: &ProcIdent) -> Result<(), PortError> {
        self.end(ident, "terminate");
        if self.st().knobs.terminate_err {
            Err(PortError("no such process".into()))
        } else {
            Ok(())
        }
    }

    fn kill_unserved(&self, ident: &ProcIdent) -> Result<(), PortError> {
        self.end(ident, "kill");
        Ok(())
    }
}

impl Rig {
    /// The process `ident` names ends, and the call is logged.
    fn end(&self, ident: &ProcIdent, how: &str) {
        let mut st = self.st();
        st.log.push(format!("{how} pid {}", ident.pid));
        for p in st.procs.iter_mut().flatten() {
            if p.ident.pid == ident.pid {
                p.alive = false;
            }
        }
    }
}

impl ChildAdmin for Rig {
    fn role(&self, slot: Slot) -> Result<RoleInfo, PortError> {
        proc_mut(&mut self.st(), slot).map(|p| RoleInfo {
            holder: p.holder,
            state: p.state,
        })
    }

    fn takeover(&self, slot: Slot, _limit: Duration) -> Result<(), PortError> {
        self.say(format!("admin:takeover {slot:?}"));
        let mut st = self.st();
        if st.knobs.takeover_err {
            return Err(PortError("takeover refused".into()));
        }
        hold(&mut st, slot)
    }

    fn quiesce(&self, slot: Slot) -> Result<(), PortError> {
        self.say(format!("admin:quiesce {slot:?}"));
        proc_mut(&mut self.st(), slot).map(|p| p.state = RoleState::Quiesced)
    }

    fn stand_down(&self, slot: Slot) -> Result<(), PortError> {
        self.say(format!("admin:stand_down {slot:?}"));
        proc_mut(&mut self.st(), slot).map(|p| {
            p.state = RoleState::StoodDown;
            p.holder = false;
        })
    }

    fn resume(&self, slot: Slot) -> Result<(), PortError> {
        self.say(format!("admin:resume {slot:?}"));
        hold(&mut self.st(), slot)
    }

    fn ready(&self, slot: Slot) -> Result<Readiness, PortError> {
        let mut st = self.st();
        let ready = st.knobs.ready;
        let up = proc_mut(&mut st, slot).is_ok() && ready;
        Ok(Readiness {
            readyz: up,
            health: up,
            capabilities: up,
        })
    }

    fn open_counts(&self, slot: Slot) -> Result<OpenCounts, PortError> {
        let mut st = self.st();
        let open_streams = st.knobs.streams[ix(slot)];
        proc_mut(&mut st, slot).map(|_| OpenCounts {
            open_streams,
            open_sessions: 0,
        })
    }

    fn post_desired(&self, slot: Slot, commit: &CommitId) -> Result<(), PortError> {
        self.say(format!("admin:desired {slot:?} {commit}"));
        Ok(())
    }
}

impl Gate for Rig {
    fn hold(&self, _max: Duration) -> Result<(), PortError> {
        let mut st = self.st();
        st.held = true;
        st.log.push("hold".into());
        Ok(())
    }

    fn release_into(&self, slot: Slot) -> Result<(), PortError> {
        let mut st = self.st();
        st.held = false;
        st.active = slot;
        st.log.push(format!("release_into {slot:?}"));
        Ok(())
    }

    fn expire(&self) -> usize {
        0
    }

    fn open_streams(&self, slot: Slot) -> u64 {
        self.st().knobs.streams[ix(slot)]
    }

    fn clear_active(&self) {
        self.say("clear_active");
    }

    fn retire(&self, slot: Slot) -> Retired {
        if slot == self.st().active {
            Retired::Current
        } else {
            Retired::Closed
        }
    }
}

impl Migrator for Rig {
    fn migrate_forward(&self, release: &CommitId) -> Result<(), PortError> {
        self.say(format!("migrate {release}"));
        match self.st().knobs.migrate_err.clone() {
            Some(e) => Err(PortError(e)),
            None => Ok(()),
        }
    }
}

impl Recorder for Rig {
    fn record(&self, intent: &Intent) -> Result<(), Halt> {
        if self.st().knobs.fail_record == Some(intent.phase) {
            return Err(Halt::Port("update.json refused the record".into()));
        }
        if let Some(dir) = &self.dir {
            FileRecorder::new(dir).record(intent)?;
        }
        self.st().records.push(intent.clone());
        Ok(())
    }

    fn crash_point(&self, _point: CrashPoint) -> Result<(), Halt> {
        Ok(())
    }
}

impl RootOrders for Rig {
    fn frontend_update(&self, locator: &Locator) -> Result<(), PortError> {
        self.say(format!("order frontend_update {}", locator.order_id));
        Ok(())
    }

    fn staged(&self) -> Result<Option<StagedFacts>, PortError> {
        let st = self.st();
        if st.knobs.status_err {
            return Err(PortError("status.json unreadable".into()));
        }
        Ok(st.knobs.staged.clone().map(|commit| StagedFacts {
            commit,
            protocol: 7,
            min_protocol: 5,
            state_schema: 3,
            profile: Some("frontend".into()),
        }))
    }

    fn installed(&self) -> Result<Option<CommitId>, PortError> {
        let st = self.st();
        if st.knobs.status_err {
            return Err(PortError("status.json unreadable".into()));
        }
        Ok(st.installed.clone())
    }

    fn confirm(&self, commit: &CommitId) -> Result<(), PortError> {
        let mut st = self.st();
        st.log.push(format!("order confirm {commit}"));
        st.installed = Some(commit.clone());
        Ok(())
    }

    fn revert(&self, commit: &CommitId) -> Result<(), PortError> {
        self.say(format!("order revert {commit}"));
        Ok(())
    }
}

impl Clock for Rig {
    fn now(&self) -> Duration {
        Duration::from_secs(1_000_000) + self.start.elapsed()
    }

    fn sleep(&self, d: Duration) -> Result<(), Halt> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(Halt::Interrupted);
        }
        std::thread::sleep(d);
        Ok(())
    }
}
