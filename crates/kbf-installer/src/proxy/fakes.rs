//! One in-memory world behind every port, on a virtual clock. It records each call in one
//! ordered log, so a test can assert the order of calls across ports (kill before resume,
//! takeover before release). Time moves only when the swap sleeps or a fake call takes time.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use crate::CommitId;
use crate::testkit::{A, B, c};

use super::model::*;
use super::ports::*;

pub const START: Duration = Duration::from_secs(1_000_000);

pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// How a spawned process behaves.
#[derive(Clone, Copy)]
pub struct Script {
    pub ready_after: Option<Duration>,
    pub exit_after: Option<Duration>,
}

impl Default for Script {
    fn default() -> Script {
        Script {
            ready_after: Some(secs(2)),
            exit_after: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Takeover {
    Ok,
    Err,
    /// Succeeds after this long.
    Slow(Duration),
    /// Returns Ok without taking the generation (the call had not landed).
    NoEffect,
}

/// A number that holds until a time, then is zero (until None: for ever).
#[derive(Clone, Copy)]
pub struct Until(pub u64, pub Option<Duration>);

impl Until {
    fn at(&self, now: Duration) -> u64 {
        match self.1 {
            Some(t) if now >= t => 0,
            _ => self.0,
        }
    }
}

pub struct Proc {
    pub slot: Slot,
    pub commit: CommitId,
    pub alive: bool,
    pub start_time: u64,
    pub spawned_at: Duration,
    pub script: Script,
    pub state: RoleState,
    pub holder: bool,
    pub quiesce_at: Option<Duration>,
    pub executions: Until,
    pub queued: Until,
}

pub struct State {
    pub procs: Vec<Proc>,
    pub scripts: VecDeque<Script>,
    pub migrate_err: Option<String>,
    pub takeover: Takeover,
    pub quiesce_takes: Duration,
    pub open_counts_err: bool,
    pub desired_err: bool,
    pub resume_fails: Vec<Slot>,
    /// How long a process takes to exit after SIGTERM.
    pub term_takes: Duration,
    pub confirm_err: bool,
    /// The next this many confirm orders fail, then they succeed.
    pub confirm_fails: u32,
    /// The hold orders from this one on (1 is the first) are refused.
    pub hold_fails_from: Option<u32>,
    pub holds: u32,
    /// One of the three readiness checks (0 readyz, 1 health, 2 capabilities) stays false.
    pub ready_gap: Option<u8>,
    pub staged: Option<CommitId>,
    pub permits: BTreeMap<Slot, Until>,
    pub held: Vec<Duration>,
    pub hold_max: Duration,
    pub hold_on: bool,
    pub fed: Vec<(Slot, usize)>,
    pub expired: usize,
    pub active: Slot,
    pub requests: Vec<(Duration, Request)>,
    pub events: Vec<(Duration, String)>,
    pub limit: Duration,
}

/// Decides, for each record written, whether the proxy dies right after persisting it.
pub type CrashWhen = Box<dyn Fn(&Intent) -> bool>;

pub struct World {
    now: Cell<Duration>,
    pub st: RefCell<State>,
    pub records: RefCell<Vec<Intent>>,
    /// Die (after persisting) at the first record this returns true for.
    pub crash_on_record: RefCell<Option<CrashWhen>>,
    pub crash_at: Cell<Option<CrashPoint>>,
}

pub fn exe_of(commit: &CommitId) -> PathBuf {
    PathBuf::from(format!("/var/lib/kbf-installer/releases/{commit}/kbf"))
}

impl World {
    pub fn new() -> World {
        World {
            now: Cell::new(START),
            st: RefCell::new(State {
                procs: Vec::new(),
                scripts: VecDeque::new(),
                migrate_err: None,
                takeover: Takeover::Ok,
                quiesce_takes: Duration::from_millis(200),
                open_counts_err: false,
                desired_err: false,
                resume_fails: Vec::new(),
                term_takes: Duration::ZERO,
                confirm_err: false,
                confirm_fails: 0,
                hold_fails_from: None,
                holds: 0,
                ready_gap: None,
                staged: Some(c(B)),
                permits: BTreeMap::new(),
                held: Vec::new(),
                hold_max: secs(15),
                hold_on: false,
                fed: Vec::new(),
                expired: 0,
                active: Slot::A,
                requests: Vec::new(),
                events: Vec::new(),
                limit: START + secs(48 * 3600),
            }),
            records: RefCell::new(Vec::new()),
            crash_on_record: RefCell::new(None),
            crash_at: Cell::new(None),
        }
    }

    /// The serving child: slot A, release A, the generation holder, ready.
    pub fn with_old() -> (World, ProcIdent) {
        let w = World::new();
        let ident = w.install(Slot::A, c(A));
        (w, ident)
    }

    pub fn install(&self, slot: Slot, commit: CommitId) -> ProcIdent {
        let ident = self.push(slot, commit, Script::default());
        let mut st = self.st.borrow_mut();
        let p = st.procs.last_mut().expect("just pushed");
        p.state = RoleState::Active;
        p.holder = true;
        ident
    }

    fn push(&self, slot: Slot, commit: CommitId, script: Script) -> ProcIdent {
        let mut st = self.st.borrow_mut();
        let pid = 100 + st.procs.len() as u32;
        st.procs.push(Proc {
            slot,
            commit: commit.clone(),
            alive: true,
            start_time: 5_000 + u64::from(pid),
            spawned_at: self.now.get(),
            script,
            state: RoleState::Candidate,
            holder: false,
            quiesce_at: None,
            executions: Until(0, None),
            queued: Until(0, None),
        });
        ProcIdent {
            pid,
            start_time: 5_000 + u64::from(pid),
            exe: exe_of(&commit),
        }
    }

    pub fn held(&self) -> usize {
        self.st.borrow().held.len()
    }

    pub fn elapsed(&self) -> Duration {
        self.now.get() - START
    }

    pub fn log_event(&self, text: impl Into<String>) {
        let t = self.now.get();
        self.st.borrow_mut().events.push((t, text.into()));
    }

    pub fn log(&self) -> Vec<String> {
        self.st
            .borrow()
            .events
            .iter()
            .map(|e| e.1.clone())
            .collect()
    }

    /// Index of the first event starting with `prefix`.
    pub fn pos(&self, prefix: &str) -> Option<usize> {
        self.st
            .borrow()
            .events
            .iter()
            .position(|e| e.1.starts_with(prefix))
    }

    pub fn count(&self, prefix: &str) -> usize {
        self.st
            .borrow()
            .events
            .iter()
            .filter(|e| e.1.starts_with(prefix))
            .count()
    }

    /// Time since START of the first event starting with `prefix`.
    pub fn when(&self, prefix: &str) -> Option<Duration> {
        self.st
            .borrow()
            .events
            .iter()
            .find(|e| e.1.starts_with(prefix))
            .map(|e| e.0 - START)
    }

    pub fn advance(&self, d: Duration) {
        self.now.set(self.now.get() + d);
    }

    pub fn set_limit(&self, d: Duration) {
        self.st.borrow_mut().limit = START + d;
    }

    pub fn script(&self, s: Script) {
        self.st.borrow_mut().scripts.push_back(s);
    }

    /// Make the serving child hold `n` open executions until `until` (since START).
    pub fn busy(&self, slot: Slot, n: u64, until: Option<Duration>) {
        let mut st = self.st.borrow_mut();
        for p in st.procs.iter_mut().filter(|p| p.slot == slot) {
            p.executions = Until(n, until.map(|d| START + d));
        }
    }

    pub fn queued(&self, slot: Slot, n: u64, until: Option<Duration>) {
        let mut st = self.st.borrow_mut();
        for p in st.procs.iter_mut().filter(|p| p.slot == slot) {
            p.queued = Until(n, until.map(|d| START + d));
        }
    }

    pub fn permits(&self, slot: Slot, n: u64, until: Option<Duration>) {
        self.st
            .borrow_mut()
            .permits
            .insert(slot, Until(n, until.map(|d| START + d)));
    }

    pub fn hold_streams(&self, n: usize) {
        let t = self.now.get();
        let mut st = self.st.borrow_mut();
        for _ in 0..n {
            st.held.push(t);
        }
    }

    /// `n` streams that have been held for `age` already.
    pub fn hold_streams_aged(&self, n: usize, age: Duration) {
        let t = self.now.get() - age;
        let mut st = self.st.borrow_mut();
        for _ in 0..n {
            st.held.push(t);
        }
    }

    pub fn request_at(&self, after: Duration, r: Request) {
        self.st.borrow_mut().requests.push((START + after, r));
    }

    pub fn kill_pid(&self, pid: u32) {
        let mut st = self.st.borrow_mut();
        if let Some(p) = st.procs.get_mut(pid as usize - 100) {
            p.alive = false;
        }
    }

    /// The pid now belongs to another process.
    pub fn recycle(&self, pid: u32) {
        let mut st = self.st.borrow_mut();
        if let Some(p) = st.procs.get_mut(pid as usize - 100) {
            p.start_time += 1;
        }
    }

    pub fn holders(&self) -> Vec<Slot> {
        let st = self.st.borrow();
        st.procs
            .iter()
            .filter(|p| p.alive && p.holder)
            .map(|p| p.slot)
            .collect()
    }

    pub fn alive(&self, pid: u32) -> bool {
        self.st
            .borrow()
            .procs
            .get(pid as usize - 100)
            .is_some_and(|p| p.alive)
    }

    pub fn state_of(&self, pid: u32) -> RoleState {
        self.st.borrow().procs[pid as usize - 100].state
    }

    /// The proxy process dies: its memory (the hold) is gone, the children live on.
    pub fn proxy_restart(&self) {
        {
            let mut st = self.st.borrow_mut();
            st.held.clear();
            st.hold_on = false;
        }
        self.log_event("proxy restarted");
        *self.crash_on_record.borrow_mut() = None;
        self.crash_at.set(None);
    }

    pub fn last_intent(&self) -> Intent {
        self.records
            .borrow()
            .last()
            .cloned()
            .expect("a record was written")
    }

    pub fn phases(&self) -> Vec<Phase> {
        let mut out: Vec<Phase> = Vec::new();
        for r in self.records.borrow().iter() {
            if out.last() != Some(&r.phase) {
                out.push(r.phase);
            }
        }
        out
    }

    pub fn ports(&self) -> Ports<'_> {
        Ports {
            child: self,
            admin: self,
            gate: self,
            migrator: self,
            recorder: self,
            orders: self,
            clock: self,
            requests: self,
        }
    }

    fn refresh(&self) {
        let now = self.now.get();
        let mut st = self.st.borrow_mut();
        for p in st.procs.iter_mut() {
            if p.alive && p.script.exit_after.is_some_and(|d| now >= p.spawned_at + d) {
                p.alive = false;
            }
        }
    }

    fn with_slot<T>(&self, slot: Slot, f: impl FnOnce(&mut Proc, Duration) -> T) -> Option<T> {
        self.refresh();
        let now = self.now.get();
        let mut st = self.st.borrow_mut();
        st.procs
            .iter_mut()
            .rev()
            .find(|p| p.slot == slot && p.alive)
            .map(|p| f(p, now))
    }
}

fn gone(slot: Slot) -> PortError {
    PortError(format!("nothing answers in slot {slot:?}"))
}

impl Child for World {
    fn spawn(&self, slot: Slot, release: &CommitId) -> Result<ProcIdent, PortError> {
        let script = self.st.borrow_mut().scripts.pop_front().unwrap_or_default();
        let ident = self.push(slot, release.clone(), script);
        self.log_event(format!(
            "spawn {slot:?} {} pid {}",
            &release.as_str()[..4],
            ident.pid
        ));
        Ok(ident)
    }

    fn adopt(&self, _slot: Slot, recorded: &ProcIdent) -> Result<(), AdoptError> {
        self.refresh();
        let st = self.st.borrow();
        let live = st
            .procs
            .get((recorded.pid as usize).wrapping_sub(100))
            .filter(|p| p.alive)
            .ok_or(AdoptError::Gone(recorded.pid))?;
        let ident = ProcIdent {
            pid: recorded.pid,
            start_time: live.start_time,
            exe: exe_of(&live.commit),
        };
        verify_ident(recorded, &ident)
    }

    fn exited(&self, ident: &ProcIdent) -> Option<i32> {
        self.refresh();
        let st = self.st.borrow();
        match st.procs.get((ident.pid as usize).wrapping_sub(100)) {
            Some(p) if p.alive => None,
            _ => Some(0),
        }
    }

    fn terminate(&self, ident: &ProcIdent) -> Result<(), PortError> {
        self.log_event(format!("terminate pid {}", ident.pid));
        let takes = self.st.borrow().term_takes;
        if takes.is_zero() {
            self.kill_pid(ident.pid);
        } else {
            let now = self.now.get();
            let mut st = self.st.borrow_mut();
            if let Some(p) = st.procs.get_mut(ident.pid as usize - 100) {
                p.script.exit_after = Some(now - p.spawned_at + takes);
            }
        }
        Ok(())
    }

    fn kill_unserved(&self, ident: &ProcIdent) -> Result<(), PortError> {
        self.log_event(format!("kill pid {}", ident.pid));
        self.kill_pid(ident.pid);
        Ok(())
    }
}

impl ChildAdmin for World {
    fn role(&self, slot: Slot) -> Result<RoleInfo, PortError> {
        self.log_event(format!("admin:role {slot:?}"));
        let takes = self.st.borrow().quiesce_takes;
        self.with_slot(slot, |p, now| {
            if p.state == RoleState::Quiescing && p.quiesce_at.is_some_and(|t| now >= t + takes) {
                p.state = RoleState::Quiesced;
            }
            RoleInfo {
                holder: p.holder,
                state: p.state,
            }
        })
        .ok_or_else(|| gone(slot))
    }

    fn takeover(&self, slot: Slot, _limit: Duration) -> Result<(), PortError> {
        self.log_event(format!("admin:takeover {slot:?}"));
        let mode = self.st.borrow().takeover;
        match mode {
            Takeover::Err => return Err(PortError("takeover refused".into())),
            Takeover::Slow(d) => self.advance(d),
            Takeover::NoEffect => return Ok(()),
            Takeover::Ok => {}
        }
        self.refresh();
        let mut st = self.st.borrow_mut();
        for p in st.procs.iter_mut() {
            if p.alive && p.slot != slot {
                p.holder = false;
            }
        }
        match st
            .procs
            .iter_mut()
            .rev()
            .find(|p| p.slot == slot && p.alive)
        {
            Some(p) => {
                p.holder = true;
                p.state = RoleState::Active;
                Ok(())
            }
            None => Err(gone(slot)),
        }
    }

    fn quiesce(&self, slot: Slot) -> Result<(), PortError> {
        self.log_event(format!("admin:quiesce {slot:?}"));
        self.with_slot(slot, |p, now| {
            p.state = RoleState::Quiescing;
            p.quiesce_at = Some(now);
        })
        .ok_or_else(|| gone(slot))
    }

    fn stand_down(&self, slot: Slot) -> Result<(), PortError> {
        self.log_event(format!("admin:stand_down {slot:?}"));
        self.with_slot(slot, |p, _| p.state = RoleState::StoodDown)
            .ok_or_else(|| gone(slot))
    }

    fn resume(&self, slot: Slot) -> Result<(), PortError> {
        self.log_event(format!("admin:resume {slot:?}"));
        if self.st.borrow().resume_fails.contains(&slot) {
            return Err(PortError("resume refused".into()));
        }
        self.refresh();
        let mut st = self.st.borrow_mut();
        // Re-take the generation only if the holder is dead or has stood down (agrees).
        let blocked = st
            .procs
            .iter()
            .any(|p| p.alive && p.slot != slot && p.holder && p.state != RoleState::StoodDown);
        for p in st.procs.iter_mut() {
            if p.alive && p.slot != slot && p.holder && !blocked {
                p.holder = false;
            }
        }
        match st
            .procs
            .iter_mut()
            .rev()
            .find(|p| p.slot == slot && p.alive)
        {
            Some(p) => {
                p.state = RoleState::Active;
                p.quiesce_at = None;
                p.holder = p.holder || !blocked;
                Ok(())
            }
            None => Err(gone(slot)),
        }
    }

    fn ready(&self, slot: Slot) -> Result<Readiness, PortError> {
        let ok = self
            .with_slot(slot, |p, now| {
                p.script
                    .ready_after
                    .is_some_and(|d| now >= p.spawned_at + d)
            })
            .unwrap_or(false);
        let gap = self.st.borrow().ready_gap;
        Ok(Readiness {
            readyz: ok && gap != Some(0),
            health: ok && gap != Some(1),
            capabilities: ok && gap != Some(2),
        })
    }

    fn open_counts(&self, slot: Slot) -> Result<OpenCounts, PortError> {
        if self.st.borrow().open_counts_err {
            return Err(PortError("admin port closed".into()));
        }
        self.with_slot(slot, |p, now| OpenCounts {
            open_executions: p.executions.at(now),
            queued_operations: p.queued.at(now),
        })
        .ok_or_else(|| gone(slot))
    }

    fn post_desired(&self, slot: Slot, commit: &CommitId) -> Result<(), PortError> {
        self.log_event(format!("admin:desired {slot:?} {}", &commit.as_str()[..4]));
        if self.st.borrow().desired_err {
            return Err(PortError("desired refused".into()));
        }
        Ok(())
    }
}

impl Gate for World {
    fn hold(&self, max: Duration) -> Result<(), PortError> {
        self.log_event("hold");
        let mut st = self.st.borrow_mut();
        st.holds += 1;
        if st.hold_fails_from.is_some_and(|n| st.holds >= n) {
            return Err(PortError("hold refused".into()));
        }
        st.hold_on = true;
        st.hold_max = max;
        Ok(())
    }

    fn release_into(&self, slot: Slot) -> Result<(), PortError> {
        let n = {
            let mut st = self.st.borrow_mut();
            let n = st.held.len();
            st.held.clear();
            st.hold_on = false;
            st.active = slot;
            st.fed.push((slot, n));
            n
        };
        self.log_event(format!("release_into {slot:?} {n}"));
        Ok(())
    }

    fn expire(&self) -> usize {
        let now = self.now.get();
        let n = {
            let mut st = self.st.borrow_mut();
            if !st.hold_on {
                return 0;
            }
            let max = st.hold_max;
            let before = st.held.len();
            st.held.retain(|t| now - *t < max);
            let n = before - st.held.len();
            st.expired += n;
            n
        };
        if n > 0 {
            self.log_event(format!("gate:expired {n}"));
        }
        n
    }

    fn open_streams(&self, slot: Slot) -> u64 {
        let now = self.now.get();
        self.st.borrow().permits.get(&slot).map_or(0, |u| u.at(now))
    }
}

impl Migrator for World {
    fn migrate_forward(&self, release: &CommitId) -> Result<(), PortError> {
        self.log_event(format!("migrate {}", &release.as_str()[..4]));
        match self.st.borrow().migrate_err.clone() {
            Some(e) => Err(PortError(e)),
            None => Ok(()),
        }
    }
}

impl Recorder for World {
    fn record(&self, intent: &Intent) -> Result<(), Halt> {
        self.records.borrow_mut().push(intent.clone());
        self.log_event(format!("record {:?}", intent.phase));
        let hit = self
            .crash_on_record
            .borrow()
            .as_ref()
            .is_some_and(|f| f(intent));
        if hit {
            return Err(Halt::Crashed(format!("after recording {:?}", intent.phase)));
        }
        Ok(())
    }

    fn crash_point(&self, point: CrashPoint) -> Result<(), Halt> {
        if self.crash_at.get() == Some(point) {
            return Err(Halt::Crashed(format!("{point:?}")));
        }
        Ok(())
    }
}

impl RootOrders for World {
    fn frontend_update(&self, locator: &Locator) -> Result<(), PortError> {
        self.log_event(format!("order frontend_update {}", locator.order_id));
        Ok(())
    }

    fn staged(&self) -> Result<Option<StagedFacts>, PortError> {
        Ok(self.st.borrow().staged.clone().map(|commit| StagedFacts {
            commit,
            protocol: 7,
            min_protocol: 5,
            state_schema: 3,
            profile: Some("frontend".into()),
        }))
    }

    fn confirm(&self, commit: &CommitId) -> Result<(), PortError> {
        self.log_event(format!("order confirm {}", &commit.as_str()[..4]));
        let mut st = self.st.borrow_mut();
        if st.confirm_err || st.confirm_fails > 0 {
            st.confirm_fails = st.confirm_fails.saturating_sub(1);
            return Err(PortError("confirm refused".into()));
        }
        Ok(())
    }

    fn revert(&self, commit: &CommitId) -> Result<(), PortError> {
        self.log_event(format!("order revert {}", &commit.as_str()[..4]));
        Ok(())
    }
}

impl Clock for World {
    fn now(&self) -> Duration {
        self.now.get()
    }

    fn sleep(&self, d: Duration) -> Result<(), Halt> {
        if self.now.get() + d > self.st.borrow().limit {
            return Err(Halt::Interrupted);
        }
        self.advance(d);
        Ok(())
    }
}

impl Requests for World {
    fn next(&self) -> Option<Request> {
        let now = self.now.get();
        let mut st = self.st.borrow_mut();
        let i = st.requests.iter().position(|(t, _)| now >= *t)?;
        Some(st.requests.remove(i).1)
    }
}

/// The plan of the standard update: release A to release B, slot A serving.
pub fn plan(mode: Mode) -> Plan {
    Plan {
        update_id: "u1".into(),
        request_id: "r1".into(),
        mode,
        from: c(A),
        to: c(B),
        nodes: true,
        rollback: false,
        old_slot: Slot::A,
        timing: match mode {
            Mode::Frontend => Timing::frontend(),
            Mode::AllInOne => Timing::all_in_one(),
        },
    }
}
