//! The frontend's own update, as the installer carries it out: a `frontend_update` record
//! starts the new release as the idle instance of `kbf-frontend@.service` while the other
//! serves, and the installer, not the frontend, decides whether to keep it.
//!
//! [`Frontend::begin`] is one pass handling the record, the way an activation is handled:
//!
//! 1. one update at a time: a record is refused (`frontend_busy`) while one is in flight or
//!    while either unit is changing or both are running (the old instance of an earlier
//!    update is still draining), and never queued; a node release on trial refuses it too;
//! 2. the release is fetched and verified by the [`Fetcher`] the node path uses, and its
//!    handover protocol must equal the serving instance's, or nothing is started;
//! 3. the idle instance is the other one than the serving one, checked against systemd
//!    (the state file is a hint, systemd is the fact; a disagreement is refused and
//!    reported). The pending record is written first, then `slots/<idle>` is pointed at the
//!    new release, then the idle unit is started.
//!
//! [`Frontend::advance`] is run every pass and moves a pending update one step, from the
//! record and what systemd and `/readyz` say now:
//!
//! - **Confirming**: the new unit active and its `/readyz` ok for [`FrontendConfig::looks`]
//!   passes in a row, and the old instance inactive or not ready, all before the window
//!   ends, then `current`, `previous` and `serving_instance` are flipped and the update is
//!   confirmed. The new instance's READY means the old one already stood down (the
//!   successor serves only after the holder's `released`); the installer never stops the old
//!   one, which exits itself when its streams close.
//! - **Reverting**, decided when the window ends or the new unit stopped: stop the new
//!   instance; if the old one still serves, put `slots/<idle>` back and nothing else (the old
//!   one is untouched); if it already stood down it cannot retake the store, so it is stopped
//!   (it ends its relays once the new one is gone) and started again, a cold start with a new
//!   epoch, and waited for. If it will not stop or come back within the bound the update is
//!   [`Progress::NeedsOperator`] and nothing is killed.
//!
//! The load balancer in front of the two instances is the operator's, set up once with both
//! instances' ports and an active health check on each one's `/readyz`. This step never edits,
//! reloads, restarts or checks it: it moves traffic by that health alone, because the old
//! instance goes unready when it stops scheduling and the new one is ready only after the
//! handover. The installer relies on `/readyz` for the same facts.
//!
//! A node `activate` never touches these units, and the order floors are kept per kind
//! ([`crate::helper::memory::Kind`]).

pub mod pending;
pub mod probe;
pub mod units;

use std::io;
use std::path::PathBuf;

use kbf_host::{Exec, HttpUrl, Perms, Step};

use crate::CommitId;
use crate::activate;
use crate::helper::fetcher::{Fetcher, InstalledRelease, ReleaseLocator};
use crate::helper::order::Order;
use crate::layout::{self, Layout};
use crate::state::{Locked, StateError};
use pending::{End, FileError, FrontendEnd, FrontendFile, FrontendPending, Instance, Phase};
use probe::HandoverProbe;
use units::UnitState;

/// How the frontend update is set up on a host, by the installer's own flags.
#[derive(Clone, Debug)]
pub struct FrontendConfig {
    /// Each instance's `/readyz`, on loopback.
    pub readyz_a: HttpUrl,
    pub readyz_b: HttpUrl,
    /// The confirm window, clamped to the host's bounds.
    pub window_s: u64,
    /// The passes in a row the new instance must be ready.
    pub looks: u32,
    /// The longest a revert waits for the old instance to stop and come back.
    pub revert_bound_s: u64,
}

impl FrontendConfig {
    fn readyz(&self, instance: Instance) -> &HttpUrl {
        match instance {
            Instance::A => &self.readyz_a,
            Instance::B => &self.readyz_b,
        }
    }
}

/// The word the status gives for a refusal that is the frontend's.
pub const BUSY: &str = "frontend_busy";

/// Why a pass could not go on: the host's own files or systemd are unusable. A refused
/// order is not one.
#[derive(Debug, thiserror::Error)]
pub enum FrontendError {
    #[error(transparent)]
    File(#[from] FileError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Release(#[from] crate::release::ReleaseError),
}

/// What [`Frontend::begin`] did with a record.
#[derive(Debug, PartialEq, Eq)]
pub enum Begun {
    /// Refused, with the reason (it begins with the stable word).
    Refused(String),
    /// The pending record is written; [`Frontend::advance`] takes it from here. Also the
    /// answer to the same record read again.
    Started,
    /// The serving instance runs this release already.
    AlreadyRunning,
}

/// What one pass of [`Frontend::advance`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Progress {
    /// Nothing is pending.
    Idle,
    /// An update is in flight and has not reached its end.
    Waiting,
    /// The update was kept: the new instance serves.
    Confirmed(CommitId),
    /// The update was undone, with why.
    Reverted(String),
    /// The old instance would not stop or come back in time; an operator is needed.
    NeedsOperator(String),
}

impl Progress {
    /// The exit status a one-shot run ends with: 5 when an operator is needed.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Progress::NeedsOperator(_) => 5,
            _ => 0,
        }
    }
}

/// The frontend half of the installer.
pub struct Frontend {
    cfg: FrontendConfig,
    probe: Box<dyn HandoverProbe>,
}

/// The two units' states, and which of them serves.
enum Units {
    Serving(Instance),
    /// Both running, or one changing: an earlier update still draining, or in motion.
    Busy(String),
    /// Neither runs.
    Down,
}

fn refused(code: &str, why: impl std::fmt::Display) -> Begun {
    Begun::Refused(format!("{code}: {why}"))
}

impl Frontend {
    #[must_use]
    pub fn new(cfg: FrontendConfig, probe: Box<dyn HandoverProbe>) -> Frontend {
        Frontend { cfg, probe }
    }

    fn units<E: Exec>(exec: &E) -> io::Result<Units> {
        let a = units::state(exec, Instance::A)?;
        let b = units::state(exec, Instance::B)?;
        Ok(match (a, b) {
            (UnitState::Active, UnitState::Stopped) => Units::Serving(Instance::A),
            (UnitState::Stopped, UnitState::Active) => Units::Serving(Instance::B),
            (UnitState::Stopped, UnitState::Stopped) => Units::Down,
            (a, b) => Units::Busy(format!("instance a is {a:?} and instance b is {b:?}")),
        })
    }

    /// Take one `frontend_update` order: the checks, the fetch, then the first writes.
    ///
    /// # Errors
    /// The host's files or systemd could not be used (see [`FrontendError`]).
    pub fn begin<E: Exec>(
        &self,
        exec: &mut E,
        host: &Layout,
        fetcher: &dyn Fetcher,
        order: &Order,
        now: u64,
    ) -> Result<Begun, FrontendError> {
        let mut file = pending::load(host)?;
        let this = (order.epoch, order.id);
        if let Some(p) = &file.frontend_pending {
            // The same record read again (the pass that wrote the record died before it was
            // acknowledged) is the update already in flight.
            return Ok(if (p.epoch, p.order_id) == this {
                Begun::Started
            } else {
                refused(
                    BUSY,
                    format!("{} is being started; one update at a time", p.commit),
                )
            });
        }
        let state = crate::state::read_unlocked(host)?.unwrap_or_default();
        if let Some(p) = &state.pending_confirm {
            return Ok(refused(
                "pending",
                format!("{} is on trial and not yet confirmed or reverted", p.commit),
            ));
        }
        let names: Vec<&str> = order.components.iter().map(|c| c.name.as_str()).collect();
        if names != [layout::RELEASE_BINARY] {
            return Ok(refused(
                "components",
                format!("a frontend update names exactly [\"kbf\"], not {names:?}"),
            ));
        }
        let serving = match Self::units(exec) {
            Ok(Units::Serving(i)) => i,
            Ok(Units::Busy(why)) => return Ok(refused(BUSY, why)),
            Ok(Units::Down) => {
                return Ok(refused(
                    "frontend_down",
                    "neither instance runs, so there is nothing to hand over from",
                ));
            }
            Err(e) => return Ok(refused("units", format!("cannot read the units: {e}"))),
        };
        if let Some(hint) = file.serving_instance
            && hint != serving
        {
            return Ok(refused(
                "serving_mismatch",
                format!(
                    "the installer last recorded instance {} as serving; systemd says {}",
                    hint.name(),
                    serving.name()
                ),
            ));
        }
        let idle = serving.other();
        let serving_slot = match slot(exec, serving) {
            Ok(target) => target,
            Err(why) => return Ok(refused("slot", why)),
        };
        let prior_slot = match slot(exec, idle) {
            Ok(target) => target,
            Err(why) => return Ok(refused("slot", why)),
        };
        if serving_slot.as_deref() == Some(slot_text(&order.commit).as_str()) {
            return Ok(Begun::AlreadyRunning);
        }
        let got = match fetcher.fetch(&ReleaseLocator {
            commit: order.commit.clone(),
            components: order.components.clone(),
        }) {
            Ok(got) => got,
            Err(e) => return Ok(refused("fetch", e)),
        };
        if let Err(why) = matches_order(&got, order) {
            return Ok(refused(
                "facts",
                format!("the fetched release is not the one ordered: {why}"),
            ));
        }
        if let Some(why) = self.protocols_differ(host, &order.commit, serving) {
            return Ok(refused("handover_protocol", why));
        }

        // State first: the record of what is about to be done, then the slot, then the
        // start. A pass killed anywhere after this finds the record and goes on from what
        // the host shows.
        file.serving_instance.get_or_insert(serving);
        file.frontend_pending = Some(FrontendPending {
            epoch: order.epoch,
            order_id: order.id,
            commit: order.commit.clone(),
            idle,
            old: serving,
            prior_slot,
            deadline_s: now + self.window(order),
            phase: Phase::Confirming { looks: 0 },
            why: String::new(),
        });
        pending::save(exec, host, &file)?;
        exec.apply(&Step::Mkdir {
            path: layout::slots_dir(),
            perms: Perms::new(0o755),
            owner: None,
        })?;
        exec.apply(&Step::Symlink {
            path: layout::slot_link(idle.name()),
            target: layout::slot_target(&order.commit),
        })?;
        // The start returns on READY. A failure is not decided here: the next pass reads the
        // unit and goes down the revert branch the old instance's state calls for.
        let _ = units::start(exec, idle);
        Ok(Begun::Started)
    }

    /// The window the order asks for, clamped to the host's bounds.
    fn window(&self, order: &Order) -> u64 {
        let asked = if order.window_s == 0 {
            self.cfg.window_s
        } else {
            u64::from(order.window_s)
        };
        asked.clamp(activate::WINDOW_MIN_SECS, activate::WINDOW_MAX_SECS)
    }

    /// Why the handover protocols of the serving instance and the new release are not
    /// equal (or not known); `None` when they are.
    fn protocols_differ(
        &self,
        host: &Layout,
        commit: &CommitId,
        serving: Instance,
    ) -> Option<String> {
        let new_binary = host.join(&layout::release_file(commit, layout::RELEASE_BINARY));
        let old_binary = host
            .join(&layout::slot_link(serving.name()))
            .join(layout::RELEASE_BINARY);
        let new = match self.probe.handover_protocol(&new_binary) {
            Ok(n) => n,
            Err(e) => return Some(format!("the new release: {e}")),
        };
        let old = match self.probe.handover_protocol(&old_binary) {
            Ok(n) => n,
            Err(e) => return Some(format!("the serving instance: {e}")),
        };
        (new != old).then(|| {
            format!(
                "the new release speaks handover protocol {new}, the serving instance {old}: \
                 they cannot hand over"
            )
        })
    }

    /// Move a pending update one step. Cheap when nothing is pending.
    ///
    /// # Errors
    /// The host's files could not be used.
    pub fn advance<E: Exec>(
        &self,
        exec: &mut E,
        host: &Layout,
        now: u64,
        out: &mut dyn std::io::Write,
    ) -> Result<Progress, FrontendError> {
        let file = pending::load(host)?;
        let Some(p) = file.frontend_pending.clone() else {
            return Ok(Progress::Idle);
        };
        match p.phase.clone() {
            Phase::Confirming { looks } => self.confirming(exec, host, file, p, looks, now, out),
            Phase::Flipping => self.flip(exec, host, file, &p, out),
            Phase::Reverting {
                deadline,
                old_started,
            } => self.reverting(exec, host, file, p, (deadline, old_started), now),
            Phase::NeedsOperator { reason } => Ok(Progress::NeedsOperator(reason)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn confirming<E: Exec>(
        &self,
        exec: &mut E,
        host: &Layout,
        mut file: FrontendFile,
        mut p: FrontendPending,
        looks: u32,
        now: u64,
        out: &mut dyn std::io::Write,
    ) -> Result<Progress, FrontendError> {
        if now > p.deadline_s {
            return self.revert(
                exec,
                host,
                file,
                p,
                now,
                "the new instance was not confirmed within the window",
            );
        }
        // A unit that cannot be read this pass is not evidence of anything.
        let Ok(new) = units::state(exec, p.idle) else {
            return Ok(Progress::Waiting);
        };
        if new == UnitState::Stopped {
            return self.revert(exec, host, file, p, now, "the new instance stopped");
        }
        let new_ready = new == UnitState::Active && probe::ready(exec, self.cfg.readyz(p.idle));
        let old_ready = units::state(exec, p.old).is_ok_and(|s| s == UnitState::Active)
            && probe::ready(exec, self.cfg.readyz(p.old));
        let seen = if new_ready { looks + 1 } else { 0 };
        if seen >= self.cfg.looks && !old_ready {
            p.phase = Phase::Flipping;
            file.frontend_pending = Some(p.clone());
            pending::save(exec, host, &file)?;
            return self.flip(exec, host, file, &p, out);
        }
        if seen != looks {
            p.phase = Phase::Confirming { looks: seen };
            file.frontend_pending = Some(p);
            pending::save(exec, host, &file)?;
        }
        Ok(Progress::Waiting)
    }

    /// Keep the update: `current`, `previous`, the links, then `serving_instance` and the end
    /// of the pending record. Safe to run again from any point.
    fn flip<E: Exec>(
        &self,
        exec: &mut E,
        host: &Layout,
        mut file: FrontendFile,
        p: &FrontendPending,
        out: &mut dyn std::io::Write,
    ) -> Result<Progress, FrontendError> {
        {
            let lock = Locked::acquire(host)?;
            let state = lock.read()?.unwrap_or_default();
            if state.current.as_ref() != Some(&p.commit) {
                let next = crate::state::State {
                    previous: state.current.clone(),
                    current: Some(p.commit.clone()),
                    staged: state.staged.clone().filter(|c| *c != p.commit),
                    ..state
                };
                lock.write(&next)?;
            }
        }
        activate::reconcile(exec, host, out)?;
        file.serving_instance = Some(p.idle);
        file.frontend_pending = None;
        file.last = Some(ended(p, End::Confirmed, String::new()));
        pending::save(exec, host, &file)?;
        Ok(Progress::Confirmed(p.commit.clone()))
    }

    fn revert<E: Exec>(
        &self,
        exec: &mut E,
        host: &Layout,
        mut file: FrontendFile,
        mut p: FrontendPending,
        now: u64,
        why: &str,
    ) -> Result<Progress, FrontendError> {
        let bound = (now + self.cfg.revert_bound_s, false);
        p.phase = Phase::Reverting {
            deadline: bound.0,
            old_started: bound.1,
        };
        why.clone_into(&mut p.why);
        file.frontend_pending = Some(p.clone());
        pending::save(exec, host, &file)?;
        self.reverting(exec, host, file, p, bound, now)
    }

    /// One step back, from what the units show: stop the new instance; then the old one is
    /// either serving (done), standing down (stop it so it ends its relays), stopped (start
    /// it) or changing (wait). The slot goes back last.
    fn reverting<E: Exec>(
        &self,
        exec: &mut E,
        host: &Layout,
        file: FrontendFile,
        p: FrontendPending,
        (deadline, old_started): (u64, bool),
        now: u64,
    ) -> Result<Progress, FrontendError> {
        let waiting = Progress::Waiting;
        let Ok(new) = units::state(exec, p.idle) else {
            return Ok(waiting);
        };
        match new {
            UnitState::Active => {
                // Asked again every pass until it is down; a stop is idempotent.
                let _ = units::stop(exec, p.idle);
                return self.within(exec, host, file, p, deadline, now);
            }
            UnitState::Changing(_) => return self.within(exec, host, file, p, deadline, now),
            UnitState::Stopped => {}
        }
        let Ok(old) = units::state(exec, p.old) else {
            return Ok(waiting);
        };
        let old_ready = old == UnitState::Active && probe::ready(exec, self.cfg.readyz(p.old));
        match (old, old_ready) {
            (UnitState::Active, true) => self.restore_slot(exec, host, file, p),
            (UnitState::Active, false) if !old_started => {
                // Stood down: it cannot retake the store, and it ends its relays once the
                // new instance is gone. Ask it to stop; it is started on a later pass.
                let _ = units::stop(exec, p.old);
                self.within(exec, host, file, p, deadline, now)
            }
            (UnitState::Stopped, _) => {
                // A cold start: a new epoch, the daemons rerun their leases. The record
                // says so first.
                let mut file = file;
                let mut p = p;
                p.phase = Phase::Reverting {
                    deadline,
                    old_started: true,
                };
                file.frontend_pending = Some(p.clone());
                pending::save(exec, host, &file)?;
                let _ = units::start(exec, p.old);
                self.within(exec, host, file, p, deadline, now)
            }
            // Active and not answering after our own start, or changing: wait for it.
            (UnitState::Active | UnitState::Changing(_), _) => {
                self.within(exec, host, file, p, deadline, now)
            }
        }
    }

    /// The old instance serves: put the slot back, then end the record. Nothing else.
    fn restore_slot<E: Exec>(
        &self,
        exec: &mut E,
        host: &Layout,
        mut file: FrontendFile,
        p: FrontendPending,
    ) -> Result<Progress, FrontendError> {
        match &p.prior_slot {
            Some(target) => exec.apply(&Step::Symlink {
                path: layout::slot_link(p.idle.name()),
                target: PathBuf::from(target),
            })?,
            None => exec.apply(&Step::Remove {
                path: layout::slot_link(p.idle.name()),
            })?,
        }
        file.frontend_pending = None;
        file.last = Some(ended(&p, End::Reverted, p.why.clone()));
        pending::save(exec, host, &file)?;
        Ok(Progress::Reverted(p.why))
    }

    /// Wait while the revert is inside its bound; past it, an operator is needed.
    fn within<E: Exec>(
        &self,
        exec: &mut E,
        host: &Layout,
        mut file: FrontendFile,
        mut p: FrontendPending,
        deadline: u64,
        now: u64,
    ) -> Result<Progress, FrontendError> {
        if now < deadline {
            return Ok(Progress::Waiting);
        }
        let reason = format!(
            "instance {} did not stop or come back within {} s while undoing the update to {}; \
             nothing was killed",
            p.old.name(),
            self.cfg.revert_bound_s,
            p.commit
        );
        p.phase = Phase::NeedsOperator {
            reason: reason.clone(),
        };
        file.frontend_pending = Some(p);
        pending::save(exec, host, &file)?;
        Ok(Progress::NeedsOperator(reason))
    }
}

fn ended(p: &FrontendPending, end: End, why: String) -> FrontendEnd {
    FrontendEnd {
        epoch: p.epoch,
        order_id: p.order_id,
        commit: p.commit.clone(),
        end,
        why,
    }
}

/// Record that `order` ended without a pending record: refused before anything moved, or
/// already running. A pending update of another order is left alone.
///
/// # Errors
/// The host's files could not be used.
pub fn record_end<E: Exec>(
    exec: &mut E,
    host: &Layout,
    order: &Order,
    end: End,
    why: &str,
) -> Result<(), FrontendError> {
    let mut file = pending::load(host)?;
    if file.frontend_pending.is_some() {
        return Ok(());
    }
    file.last = Some(FrontendEnd {
        epoch: order.epoch,
        order_id: order.id,
        commit: order.commit.clone(),
        end,
        why: why.to_owned(),
    });
    pending::save(exec, host, &file)?;
    Ok(())
}

/// The text of the link `slots/<instance>`: `Ok(None)` when there is no link, and why not
/// when something else is there or it cannot be read.
fn slot<E: Exec>(exec: &E, instance: Instance) -> Result<Option<String>, String> {
    let name = instance.name();
    match exec.inspect(&layout::slot_link(name)) {
        Ok(None) => Ok(None),
        Ok(Some(kbf_host::Entry::Link { target })) => {
            Ok(Some(target.to_string_lossy().into_owned()))
        }
        Ok(Some(_)) | Err(_) => Err(format!("slots/{name} is not a link that can be read")),
    }
}

fn slot_text(commit: &CommitId) -> String {
    layout::slot_target(commit).to_string_lossy().into_owned()
}

/// Whether the release the fetcher installed is the one the order named; and, when the
/// record carried the facts, that it declares them.
fn matches_order(got: &InstalledRelease, order: &Order) -> Result<(), String> {
    if got.commit != order.commit {
        return Err(format!("it is commit {}", got.commit.as_str()));
    }
    match &order.claims {
        Some(claims) => crate::helper::cycle::matches_claims(got, order, claims),
        None => Ok(()),
    }
}

#[cfg(test)]
mod crash_tests;
#[cfg(test)]
mod sim;
#[cfg(test)]
mod status_tests;
#[cfg(test)]
mod tests;
