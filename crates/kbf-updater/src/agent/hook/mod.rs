//! `kbf update-hook <step>`: today's update of a host, split at the five steps of the hook
//! contract (`update_hook`, `docs/design/host-updates.md`, sections 3.2 and 4.3).
//!
//! The agent's update (`agent::update`) fetches B, drains the node, switches, waits for health,
//! confirms or reverts, and gives the node back, in one function. The same calls are made here, in the
//! same order, by the generic updater's step runner:
//!
//! - [`steps`]: the steps over the ports (`Local`, `Drain`, `HealthCheck`, `Clock`), which are
//!   the agent's own, with the agent's own fakes in the tests;
//! - [`record`]: the hook's durable record, kept in `--record-dir`, which every step decides
//!   from;
//! - [`flags`]: the flags before the step, the declaration's `hook_args`;
//! - [`real`]: the real ports, built from those flags.
//!
//! A step prints one JSON line last on standard output ([`update_hook::Report`]) and exits `0`,
//! `1`, `75` (the node still holds leases after `--drain-deadline-secs`: nothing was switched
//! and the node is left drained, `retry_after_secs` is `--retry-secs`) or `76` (never: no check
//! of this hook is one no version can repair). The log goes to standard error.

pub mod flags;
pub mod real;
pub mod record;
pub mod steps;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use update_hook::{Class, Invocation, Outcome, Params};

use self::flags::Flags;
use self::record::{Gate, Store, StoreError};
use self::steps::{Done, Hook, Run};
use super::local::Local;
use crate::inventory::Host;
use crate::ports::{Clock, Drain, HealthCheck};
use crate::version::Version;

#[cfg(test)]
mod flags_tests;
#[cfg(test)]
mod paths_tests;
#[cfg(test)]
mod real_tests;
#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod resume_tests;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod steps_tests;
#[cfg(test)]
mod testing;

/// What a step acts on: this host, and the ports to the helper, the frontend and the clock.
pub struct HookWorld {
    pub host: Host,
    pub local: Arc<dyn Local>,
    pub drain: Arc<dyn Drain>,
    pub health: Arc<dyn HealthCheck>,
    pub clock: Arc<dyn Clock>,
    /// Unix seconds now: the helper's deadline is in them.
    pub unix_now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

/// Where a run gets its world, and what it does before each write of the record.
pub struct Setup<'a> {
    /// Builds the world the flags describe.
    pub world: &'a dyn Fn(&Flags) -> Result<HookWorld, String>,
    /// Called before every write of the record. A test stops the step here
    /// (`update_hook::conformance::gate::point`); a real run has none.
    pub before_write: Option<Gate>,
}

/// One run of the hook: `args` are everything after `update-hook`. The last line of the step
/// goes to `out`, its log to `err`; what comes back is the exit code.
pub fn run(
    args: &[String],
    setup: &Setup<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Outcome {
    let done = attempt(args, setup, err);
    // A standard output nobody reads is no reason to change the exit code.
    let _ = done.report.write_to(out);
    done.outcome
}

fn bad(reason: &str, why: impl std::fmt::Display) -> Done {
    Done::failed(reason, Class::Release, why)
}

fn attempt(args: &[String], setup: &Setup<'_>, err: &mut dyn Write) -> Done {
    let (flags, rest) = match flags::split(args) {
        Ok(split) => split,
        Err(why) => return bad("bad_arguments", why),
    };
    let inv = match Invocation::parse(rest.iter().map(OsString::from)) {
        Ok(inv) => inv,
        Err(e) => return bad("bad_arguments", e),
    };
    let Some(file) = &inv.params else {
        return bad(
            "bad_arguments",
            "no --params file: the updater always gives one",
        );
    };
    let params = match Params::read(file) {
        Ok(params) => params,
        Err(e) => return bad("bad_params", e),
    };
    let target = match params.target_version().map(Version::parse) {
        Some(Ok(target)) => target,
        Some(Err(e)) => return bad("bad_params", format!("updater.target_version: {e}")),
        None => return bad("bad_params", "no updater.target_version"),
    };
    // `Params::read` has refused a digest of the wrong shape; one that is not there is refused here.
    let Some(digest) = params.artifact_digest().ok().flatten().map(str::to_owned) else {
        return bad("bad_params", "no updater.artifact_digest");
    };
    // The record directory is locked before anything else is touched: a second copy of a step
    // refuses, and changes nothing.
    let store = Store::new(flags.record_dir.clone(), setup.before_write.clone());
    let _lock = match store.lock() {
        Ok(lock) => lock,
        Err(StoreError::Busy) => {
            return Done::failed("busy", Class::Environment, StoreError::Busy);
        }
        Err(e) => return Done::failed("record_unusable", Class::Environment, e),
    };
    let world = match (setup.world)(&flags) {
        Ok(world) => world,
        Err(why) => return Done::failed("world", Class::Environment, why),
    };
    Hook::new(
        &world,
        Run {
            store: &store,
            params: &params,
            channel: &flags.channel,
            timing: flags.timing,
            target,
            digest,
            err,
        },
    )
    .step(&inv)
}

/// The `kbf update-hook` entry: the real ports, standard output and standard error.
#[must_use]
pub fn main(args: Vec<OsString>) -> ExitCode {
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let Ok(args) = args
        .into_iter()
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
    else {
        let done = bad("bad_arguments", "an argument is not UTF-8");
        let _ = done.report.write_to(&mut out);
        return done.outcome.into();
    };
    let setup = Setup {
        world: &real::world,
        before_write: None,
    };
    run(&args, &setup, &mut out, &mut err).into()
}
