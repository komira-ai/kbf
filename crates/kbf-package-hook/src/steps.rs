//! The five steps, over the host and the record. See the crate documentation for what each does.

use std::fmt::Display;
use std::path::{Path, PathBuf};

use update_hook::{Class, Invocation, Outcome, Params, Report, Step};

use crate::host::{Check, Host, HostError, UnitState};
use crate::record::{Record, Store, StoreError};

/// The staged `kbf`, under the staged release directory.
pub const STAGED_KBF: &str = "tree/usr/bin/kbf";

/// Seconds a deferred `verify` is told to wait before it is run again.
pub const VERIFY_RETRY_SECS: u64 = 10;

/// How a step ended: the exit code and the last line.
#[derive(Debug, PartialEq, Eq)]
pub struct Done {
    pub outcome: Outcome,
    pub report: Report,
}

impl Done {
    fn ok() -> Done {
        Done {
            outcome: Outcome::Success,
            report: Report::default(),
        }
    }

    /// Failure. `reason` is a token of `[a-z0-9_]`.
    pub fn failed(reason: &str, class: Class, detail: impl Display) -> Done {
        Done {
            outcome: Outcome::Failure,
            report: Report {
                class,
                reason: Some(reason.to_owned()),
                detail: Some(detail.to_string()),
                ..Report::default()
            },
        }
    }

    fn release(reason: &str, detail: impl Display) -> Done {
        Done::failed(reason, Class::Release, detail)
    }

    /// Nothing irreversible happened; try again after `retry_after_secs`.
    fn deferred(reason: &str, retry_after_secs: u64, detail: impl Display) -> Done {
        Done {
            outcome: Outcome::Deferred,
            report: Report {
                class: Class::Environment,
                reason: Some(reason.to_owned()),
                retry_after_secs: Some(retry_after_secs),
                detail: Some(detail.to_string()),
                ..Report::default()
            },
        }
    }
}

impl From<StoreError> for Done {
    fn from(e: StoreError) -> Done {
        match e {
            StoreError::Busy => Done::failed("busy", Class::Environment, e),
            e => Done::failed("record_unusable", Class::Environment, e),
        }
    }
}

impl From<HostError> for Done {
    fn from(e: HostError) -> Done {
        Done::failed("host_failed", Class::Environment, e)
    }
}

type Res<T> = Result<T, Done>;

/// What a step acts on.
pub struct Ctx<'a> {
    pub host: &'a dyn Host,
    pub store: &'a Store,
    pub params: &'a Params,
    pub unit: &'a str,
    pub mode: &'a str,
}

/// Runs the step `inv` names.
pub fn step(ctx: &Ctx<'_>, inv: &Invocation) -> Done {
    let done = match inv.step {
        Step::Prepare => prepare(ctx, inv.artifact.as_deref()),
        Step::Cutover => cutover(ctx),
        Step::Verify => verify(ctx),
        Step::Rollback => rollback(ctx),
        Step::Cleanup => cleanup(ctx),
    };
    done.unwrap_or_else(|done| done)
}

fn bad_params(why: impl Display) -> Done {
    Done::release("bad_params", why)
}

fn target(ctx: &Ctx<'_>) -> Res<String> {
    ctx.params
        .target_version()
        .map(str::to_owned)
        .ok_or_else(|| bad_params("no updater.target_version"))
}

fn digest(ctx: &Ctx<'_>) -> String {
    ctx.params
        .artifact_digest()
        .ok()
        .flatten()
        .unwrap_or_default()
        .to_owned()
}

/// The record of this update, if one was made for it.
fn mine(ctx: &Ctx<'_>) -> Res<Option<Record>> {
    let target = target(ctx)?;
    let digest = digest(ctx);
    Ok(ctx
        .store
        .load()?
        .filter(|record| record.is_of(&target, &digest)))
}

fn running(state: &UnitState) -> bool {
    matches!(state, UnitState::Active | UnitState::Activating)
}

fn prepare(ctx: &Ctx<'_>, artifact: Option<&Path>) -> Res<Done> {
    let target = target(ctx)?;
    let Some(staged) = ctx.params.staged_release() else {
        return Err(bad_params("no updater.staged_release"));
    };
    if artifact != Some(staged.as_path()) {
        return Err(Done::release(
            "staged_mismatch",
            format!(
                "the staged release {artifact:?} is not updater.staged_release {}",
                staged.display()
            ),
        ));
    }
    // A record of this update that is not rolled back is a run of `prepare` resumed (or run
    // again); a rolled-back one, or none, starts the update.
    let saved = mine(ctx)?.filter(|record| !record.rolled_back);
    let on_disk = saved.is_some();
    let mut record = saved.unwrap_or_else(|| {
        Record::new(
            &target,
            &digest(ctx),
            ctx.params.previous_release().is_none(),
        )
    });
    if record.prepared {
        return Ok(Done::ok());
    }
    let staged_kbf: PathBuf = staged.join(STAGED_KBF);
    match ctx.host.check(&staged_kbf, ctx.mode)? {
        Check::Ready | Check::NotReady => {}
        Check::Refused => {
            return Err(Done::release(
                "config_refused",
                format!(
                    "{} --mode={} --check refused this host's configuration",
                    staged_kbf.display(),
                    ctx.mode
                ),
            ));
        }
        Check::Failed(why) => return Err(Done::failed("check_failed", Class::Environment, why)),
    }
    let state = ctx.host.state(ctx.unit)?;
    // Written before the stop, and never read again from the unit: after the stop it is
    // inactive whatever it was.
    if !on_disk {
        record.was_active = running(&state);
        ctx.store.save(&record)?;
    }
    if state != UnitState::Inactive {
        ctx.host.stop(ctx.unit)?;
    }
    record.prepared = true;
    ctx.store.save(&record)?;
    Ok(Done::ok())
}

fn cutover(ctx: &Ctx<'_>) -> Res<Done> {
    let mut record = update_of(ctx)?;
    if !record.prepared {
        return Err(Done::release("not_prepared", "prepare has not finished"));
    }
    if !running(&ctx.host.state(ctx.unit)?) {
        ctx.host.start(ctx.unit)?;
    }
    if !record.started {
        record.started = true;
        ctx.store.save(&record)?;
    }
    Ok(Done::ok())
}

/// The record of the running update: one that exists, is this update's and is not rolled back.
fn update_of(ctx: &Ctx<'_>) -> Res<Record> {
    match mine(ctx)? {
        None => Err(Done::release("not_prepared", "no record of this update")),
        Some(record) if record.rolled_back => {
            Err(Done::release("rolled_back", "this update was rolled back"))
        }
        Some(record) => Ok(record),
    }
}

fn verify(ctx: &Ctx<'_>) -> Res<Done> {
    let mut record = update_of(ctx)?;
    match ctx.host.state(ctx.unit)? {
        UnitState::Active => {}
        UnitState::Activating => {
            return Ok(Done::deferred(
                "not_active_yet",
                VERIFY_RETRY_SECS,
                format!("{} is still activating", ctx.unit),
            ));
        }
        other => {
            return Err(Done::release(
                "not_active",
                format!("{} is {}", ctx.unit, other.word()),
            ));
        }
    }
    let installed = ctx.host.installed_version()?;
    if installed != record.target {
        return Err(Done::release(
            "version_mismatch",
            format!(
                "kbf reports version {installed}, the update installs {}",
                record.target
            ),
        ));
    }
    if !record.verified {
        record.verified = true;
        ctx.store.save(&record)?;
    }
    Ok(Done::ok())
}

fn rollback(ctx: &Ctx<'_>) -> Res<Done> {
    let Some(mut record) = mine(ctx)? else {
        return Ok(Done::ok());
    };
    if record.rolled_back {
        return Ok(Done::ok());
    }
    // The previous package is back: a unit that ran before this update runs it again; one that
    // did not (a first install, or a unit an operator had stopped) is left stopped.
    if record.was_active && !record.first_install {
        ctx.host.restart(ctx.unit)?;
    } else if ctx.host.state(ctx.unit)? != UnitState::Inactive {
        ctx.host.stop(ctx.unit)?;
    }
    record.rolled_back = true;
    ctx.store.save(&record)?;
    Ok(Done::ok())
}

fn cleanup(ctx: &Ctx<'_>) -> Res<Done> {
    let Some(record) = mine(ctx)? else {
        return Ok(Done::ok());
    };
    if !record.verified || record.rolled_back {
        return Err(Done::release(
            "not_verified",
            "cleanup runs only after verify succeeded",
        ));
    }
    ctx.store.remove()?;
    Ok(Done::ok())
}
