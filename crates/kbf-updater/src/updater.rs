//! The roll state machine.
//!
//! One roll at a time, over the hosts of a plan: the frontend first, then one host at a time.
//! Per host the turn is `pending`, `checking` (safe-to-stop, nodes only), `installing`,
//! `waiting_healthy`, `done`. Every phase change is written to `roll.json` before the work of
//! the new phase starts, so a restart finds the host where its work began and never earlier.
//! The first failure stops the roll and no later host is asked about, let alone touched.
//!
//! The machine is deterministic: no clock but the [`Clock`] port, no randomness, no threads of
//! its own. [`Updater::drive`] runs one roll to the end on the caller's thread; the other
//! methods only read or write the record and return at once, so a caller may answer
//! `GET` and `abort` while another thread drives.
//!
//! Memory never runs ahead of disk: a change is applied to a copy, written, and kept only if
//! the write succeeded. A roll whose record cannot be written stops where it is
//! (`state_lost`) rather than act on a phase it could not remember.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::api::{ApiError, ErrorCode, Failure, HostRoll, Phase, Roll, RollRequest, RollState};
use crate::inventory::{Host, Inventory, Role};
use crate::ports::{Clock, Health, HealthCheck, Installed, Installer, SafeToStop, Stop};
use crate::record::{FORMAT, Record, RollStore, StoreError, roll_id};
use crate::version::Version;

/// How long the roll waits for each thing. The defaults are the package-update design's
/// (node health 300 s, daemon 780 s, no answer 120 s); the frontend's bound, the safe-to-stop
/// bound and the cap on an install in progress are this crate's choices.
#[derive(Clone, Debug)]
pub struct Timing {
    /// Between two asks of the same port.
    pub poll: Duration,
    /// How long a host may stay unsafe to stop (or safe-to-stop unanswered) before the roll
    /// fails at it.
    pub safe_bound: Duration,
    /// How long a restart waits for a host that answers "install in progress".
    pub install_bound: Duration,
    /// How long a restart waits for a host that does not answer a query at all.
    pub query_bound: Duration,
    pub healthy_frontend: Duration,
    pub healthy_storage: Duration,
    pub healthy_daemon: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            poll: Duration::from_secs(5),
            safe_bound: Duration::from_secs(300),
            install_bound: Duration::from_secs(900),
            query_bound: Duration::from_secs(120),
            healthy_frontend: Duration::from_secs(300),
            healthy_storage: Duration::from_secs(300),
            healthy_daemon: Duration::from_secs(780),
        }
    }
}

impl Timing {
    fn healthy(&self, role: Role) -> Duration {
        match role {
            Role::Frontend => self.healthy_frontend,
            Role::Storage => self.healthy_storage,
            Role::Daemon => self.healthy_daemon,
        }
    }
}

/// The four ports of a roll.
pub struct Ports {
    pub safe: Box<dyn SafeToStop>,
    pub installer: Box<dyn Installer>,
    pub health: Box<dyn HealthCheck>,
    pub clock: Box<dyn Clock>,
}

/// The longest detail kept, in characters.
const DETAIL_MAX: usize = 200;

/// One line of at most [`DETAIL_MAX`] characters: control characters become spaces, so a
/// port's text cannot forge a line of the audit log or carry a screenful of apt output.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(DETAIL_MAX)
        .collect()
}

/// What `POST` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    pub roll: Roll,
    /// False when the idempotency key named a roll that already exists (a 200, not a 202).
    pub created: bool,
}

struct State {
    rec: Option<Record>,
    /// A thread is inside `drive`.
    driving: bool,
    /// The roll was found `running` at start-up and has not been resumed yet.
    recovering: bool,
}

/// Why a host's turn ended early.
enum Halt {
    Fail(ErrorCode, String),
    Interrupt(String),
    Store(ApiError),
}

impl From<ApiError> for Halt {
    fn from(e: ApiError) -> Halt {
        Halt::Store(e)
    }
}

pub struct Updater {
    inventory: Inventory,
    store: RollStore,
    ports: Ports,
    timing: Timing,
    state: Mutex<State>,
}

/// Clears `driving` when `drive` returns, however it returns.
struct Driving<'a>(&'a Updater);

impl Drop for Driving<'_> {
    fn drop(&mut self) {
        self.0.lock().driving = false;
    }
}

impl Updater {
    /// Open the updater over a store. Fails if `roll.json` exists and cannot be trusted:
    /// the updater does not start on a record it cannot read (fail closed). A roll left
    /// `running` is resumed by the next [`Updater::drive`] of its id.
    pub fn open(
        inventory: Inventory,
        store: RollStore,
        ports: Ports,
        timing: Timing,
    ) -> Result<Updater, StoreError> {
        let rec = store.load()?;
        let recovering = rec
            .as_ref()
            .is_some_and(|r| r.roll.state == RollState::Running);
        Ok(Updater {
            inventory,
            store,
            ports,
            timing,
            state: Mutex::new(State {
                rec,
                driving: false,
                recovering,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Write `next` and, only if that worked, keep it.
    fn commit(&self, st: &mut State, next: Record) -> Result<Roll, ApiError> {
        self.store.save(&next).map_err(|e| {
            ApiError::new(
                ErrorCode::StateLost,
                format!("roll.json cannot be written: {e}"),
            )
        })?;
        let roll = next.roll.clone();
        st.rec = Some(next);
        Ok(roll)
    }

    /// Change the current record by `f`, durably.
    fn update(&self, f: impl FnOnce(&mut Record)) -> Result<Roll, ApiError> {
        let mut st = self.lock();
        let mut next = st
            .rec
            .clone()
            .ok_or_else(|| ApiError::new(ErrorCode::UnknownRoll, "there is no roll"))?;
        f(&mut next);
        self.commit(&mut st, next)
    }

    fn snapshot(&self) -> Result<Record, ApiError> {
        self.lock()
            .rec
            .clone()
            .ok_or_else(|| ApiError::new(ErrorCode::UnknownRoll, "there is no roll"))
    }

    /// The current (latest) roll, if any has run.
    pub fn current(&self) -> Option<Roll> {
        self.lock().rec.as_ref().map(|r| r.roll.clone())
    }

    pub fn get(&self, id: &str) -> Result<Roll, ApiError> {
        self.current()
            .filter(|r| r.id == id)
            .ok_or_else(|| ApiError::new(ErrorCode::UnknownRoll, format!("no roll {id}")))
    }

    /// Begin a roll: validate the aliases, apply the idempotency key and the one-roll rule,
    /// and write the record. Nothing runs until [`Updater::drive`].
    pub fn start(&self, req: &RollRequest) -> Result<Started, ApiError> {
        let mut st = self.lock();
        if let Some(rec) = &st.rec {
            if rec.request.idempotency_key == req.idempotency_key {
                if rec.request == *req {
                    return Ok(Started {
                        roll: rec.roll.clone(),
                        created: false,
                    });
                }
                return Err(ApiError::new(
                    ErrorCode::KeyReused,
                    "this idempotency_key was used with a different body",
                )
                .with_roll(&rec.roll.id));
            }
            if !rec.roll.state.is_terminal() {
                return Err(ApiError::new(
                    ErrorCode::RollActive,
                    format!("roll {} is {:?}", rec.roll.id, rec.roll.state),
                )
                .with_roll(&rec.roll.id));
            }
        }
        let plan = self.inventory.plan(&req.hosts)?;
        let seq = st.rec.as_ref().map_or(1, |r| r.seq + 1);
        let next = Record {
            format: FORMAT,
            seq,
            request: req.clone(),
            roll: Roll {
                id: roll_id(seq),
                state: RollState::Running,
                hosts: plan
                    .iter()
                    .map(|h| HostRoll {
                        alias: h.alias.clone(),
                        phase: Phase::Pending,
                        attempts: 0,
                        detail: None,
                    })
                    .collect(),
                failure: None,
            },
            resumes: 0,
            abort_requested: false,
        };
        st.recovering = false;
        let roll = self.commit(&mut st, next)?;
        Ok(Started {
            roll,
            created: true,
        })
    }

    /// Abort a roll. A running roll that a thread is driving stops after the host in hand
    /// (between hosts); one nobody drives, and an interrupted one, is aborted at once. A
    /// finished roll is a 409.
    pub fn abort(&self, id: &str) -> Result<Roll, ApiError> {
        let mut st = self.lock();
        let mut next = st
            .rec
            .clone()
            .filter(|r| r.roll.id == id)
            .ok_or_else(|| ApiError::new(ErrorCode::UnknownRoll, format!("no roll {id}")))?;
        if next.roll.state.is_terminal() {
            return Err(ApiError::new(
                ErrorCode::RollTerminal,
                format!("roll {id} is {:?}", next.roll.state),
            ));
        }
        if st.driving {
            next.abort_requested = true;
        } else {
            next.roll.state = RollState::Aborted;
        }
        self.commit(&mut st, next)
    }

    /// Run the roll `id` to the end, or to the first failure, interruption or abort, and
    /// return it. A roll that is not running is returned as it is. The first `drive` after a
    /// restart resumes: it bumps the record's resume counter and picks each host up where its
    /// phase says (see the host's phase handling in `step`).
    pub fn drive(&self, id: &str) -> Result<Roll, ApiError> {
        {
            let mut st = self.lock();
            let rec =
                st.rec.clone().filter(|r| r.roll.id == id).ok_or_else(|| {
                    ApiError::new(ErrorCode::UnknownRoll, format!("no roll {id}"))
                })?;
            if rec.roll.state != RollState::Running {
                return Ok(rec.roll);
            }
            if st.driving {
                return Err(ApiError::new(
                    ErrorCode::RollActive,
                    format!("roll {id} is already being driven"),
                )
                .with_roll(id));
            }
            st.driving = true;
            if st.recovering {
                st.recovering = false;
                let mut next = rec;
                next.resumes += 1;
                if let Err(e) = self.commit(&mut st, next) {
                    st.driving = false;
                    return Err(e);
                }
            }
        }
        let _driving = Driving(self);
        loop {
            let rec = self.snapshot()?;
            let Some(idx) = rec.roll.hosts.iter().position(|h| h.phase != Phase::Done) else {
                return self.update(|r| r.roll.state = RollState::Done);
            };
            if rec.abort_requested {
                return self.update(|r| r.roll.state = RollState::Aborted);
            }
            match self.step(idx, &rec.request.version) {
                Ok(()) => {}
                Err(Halt::Fail(code, detail)) => {
                    let detail = clean(&detail);
                    return self.update(|r| {
                        let alias = r.roll.hosts[idx].alias.clone();
                        r.roll.hosts[idx].phase = Phase::Failed;
                        r.roll.hosts[idx].detail = Some(detail.clone());
                        r.roll.state = RollState::Failed;
                        r.roll.failure = Some(Failure {
                            alias,
                            code,
                            detail,
                        });
                    });
                }
                Err(Halt::Interrupt(detail)) => {
                    let detail = clean(&detail);
                    return self.update(|r| {
                        r.roll.hosts[idx].detail = Some(detail);
                        r.roll.state = RollState::Interrupted;
                    });
                }
                Err(Halt::Store(e)) => return Err(e),
            }
        }
    }

    fn mark(&self, idx: usize, phase: Phase, attempt: bool) -> Result<(), ApiError> {
        self.update(|r| {
            let h = &mut r.roll.hosts[idx];
            h.phase = phase;
            h.detail = None;
            h.attempts += u32::from(attempt);
        })
        .map(drop)
    }

    /// One host's turn, from the phase the record holds. A phase reached here at the top of
    /// the loop is one a restart found: `pending` and `checking` begin (or repeat) the check,
    /// `installing` asks the host what happened to the install, `waiting_healthy` resumes
    /// the wait with a fresh bound, and a `failed` host is never resumed past.
    fn step(&self, idx: usize, target: &Version) -> Result<(), Halt> {
        let rec = self.snapshot()?;
        let h = &rec.roll.hosts[idx];
        let host = self.inventory.host(&h.alias).ok_or_else(|| {
            Halt::Fail(
                ErrorCode::UnknownAlias,
                format!("host {} is no longer in the inventory", h.alias),
            )
        })?;
        match h.phase {
            Phase::Failed => {
                return Err(Halt::Fail(
                    ErrorCode::StateLost,
                    "the record holds a failed host on a running roll".to_owned(),
                ));
            }
            Phase::Pending | Phase::Checking => {
                if host.role != Role::Frontend {
                    self.mark(idx, Phase::Checking, false)?;
                    self.check_safe(host)?;
                }
                self.mark(idx, Phase::Installing, true)?;
                self.install(host, target)?;
                self.mark(idx, Phase::WaitingHealthy, false)?;
            }
            Phase::Installing => self.recover_install(idx, host, target)?,
            Phase::WaitingHealthy | Phase::Done => {}
        }
        self.wait_healthy(host, target)?;
        self.mark(idx, Phase::Done, false)?;
        Ok(())
    }

    fn now(&self) -> Duration {
        self.ports.clock.now()
    }

    fn since(&self, start: Duration) -> Duration {
        self.now().saturating_sub(start)
    }

    fn check_safe(&self, host: &Host) -> Result<(), Halt> {
        let start = self.now();
        loop {
            let (code, why) = match self.ports.safe.check(host) {
                Ok(Stop::Unknown) => {
                    return Err(Halt::Fail(
                        ErrorCode::UnknownMember,
                        format!("the cluster does not know {}", host.alias),
                    ));
                }
                Ok(Stop::Answer(v)) if v.safe => return Ok(()),
                Ok(Stop::Answer(v)) => (ErrorCode::UnsafeToStop, v.reasons.join("; ")),
                Err(e) => (ErrorCode::SafeToStopUnavailable, e.to_string()),
            };
            if self.since(start) >= self.timing.safe_bound {
                return Err(Halt::Fail(code, why));
            }
            self.ports.clock.sleep(self.timing.poll);
        }
    }

    fn install(&self, host: &Host, target: &Version) -> Result<(), Halt> {
        self.ports
            .installer
            .install(host, target)
            .map_err(|e| Halt::Fail(ErrorCode::InstallFailed, e.to_string()))
    }

    /// A host found `installing`: ask, do not install blindly. The target means the install
    /// finished; another version means it never started or did not finish, and one more
    /// install is safe (it is idempotent and refuses to go older); "in progress" waits it
    /// out; silence for [`Timing::query_bound`] interrupts the roll.
    fn recover_install(&self, idx: usize, host: &Host, target: &Version) -> Result<(), Halt> {
        let start = self.now();
        let mut last_answer = start;
        loop {
            match self.ports.installer.query(host) {
                Ok(Installed::Version(v)) => {
                    if v != target.as_str() {
                        self.mark(idx, Phase::Installing, true)?;
                        self.install(host, target)?;
                    }
                    self.mark(idx, Phase::WaitingHealthy, false)?;
                    return Ok(());
                }
                Ok(Installed::InProgress) => {
                    last_answer = self.now();
                    if self.since(start) >= self.timing.install_bound {
                        return Err(Halt::Interrupt(format!(
                            "an install on {} was still in progress after {} s",
                            host.alias,
                            self.timing.install_bound.as_secs()
                        )));
                    }
                }
                Err(e) => {
                    if self.since(last_answer) >= self.timing.query_bound {
                        return Err(Halt::Interrupt(format!(
                            "{} did not answer for {} s: {e}",
                            host.alias,
                            self.timing.query_bound.as_secs()
                        )));
                    }
                }
            }
            self.ports.clock.sleep(self.timing.poll);
        }
    }

    fn wait_healthy(&self, host: &Host, target: &Version) -> Result<(), Halt> {
        let start = self.now();
        let bound = self.timing.healthy(host.role);
        loop {
            let last = match self.ports.health.check(host, target) {
                Ok(Health::Healthy) => return Ok(()),
                Ok(Health::Abandoned(why)) => {
                    return Err(Halt::Fail(ErrorCode::SwapAbandoned, why));
                }
                Ok(Health::Waiting(why)) => why,
                Err(e) => e.to_string(),
            };
            if self.since(start) >= bound {
                return Err(Halt::Fail(
                    ErrorCode::HealthTimeout,
                    format!("{} s: {last}", bound.as_secs()),
                ));
            }
            self.ports.clock.sleep(self.timing.poll);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater_tests::{Rig, all};

    /// Catches a roll that resumes past a failed host if a record ever holds one on a running
    /// roll (the loader refuses such a file, so this is the second lock on the door): the
    /// roll fails with `state_lost` and no port is called.
    #[test]
    fn a_failed_host_on_a_running_roll_is_never_resumed_past() {
        let rig = Rig::new("failed-on-running");
        let u = rig.open();
        u.start(&all("2.0", "k")).expect("start");
        u.lock().rec.as_mut().expect("rec").roll.hosts[0].phase = Phase::Failed;
        let roll = u.drive("roll-1").expect("drive");
        assert_eq!(roll.state, RollState::Failed);
        assert_eq!(roll.failure.expect("f").code, ErrorCode::StateLost);
        assert!(rig.log.events().is_empty());
    }

    /// Catches port text reaching the record raw: a newline would forge a log line, and an
    /// unbounded string would put apt's output in a public GET.
    #[test]
    fn detail_is_one_short_line() {
        assert_eq!(clean("a\nb\r\0c\td"), "a b  c d");
        assert_eq!(clean(&"x".repeat(500)).chars().count(), DETAIL_MAX);
        assert_eq!(clean(&"\u{e9}".repeat(500)).chars().count(), DETAIL_MAX);
    }
}
