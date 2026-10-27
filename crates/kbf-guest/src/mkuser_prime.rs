//! The simulator's root setup in the priming boot: what `mkuser` runs, as steps of its own,
//! before it writes the user's ready marker. See [`crate::sim_primed`] for why and for what
//! the record holds.
//!
//! The steps are the root steps of the probe ([`crate::sim_primed::root_steps`]: the same
//! ids and commands), in the probe's order, between two of mkuser's own:
//!
//! 1. `prime-reset` removes [`simpaths::PRIMED`], so that a run that stops part way never
//!    leaves a record that claims more than was done;
//! 2. `xcode-select`, `license`, `first-launch`, and one `runtime-add-<name>` or
//!    `runtime-import-<name>` for each runtime `staged.json` lists; a step the record read
//!    at the start of the run already covers (an earlier prime, with the same entry) is
//!    reported `ok` with the note `skipped: done at prime` and not run again, so that
//!    priming a restaged base imports only what is new;
//! 3. `prime-record` writes the record. It needs every step before it.
//!
//! The ready marker needs `prime-record`, so a failed step (a runtime that does not import)
//! leaves no marker, and a base that is not ready is never used by the probe.
//!
//! A setup that staged no Xcode (`staged.json` missing, unreadable or with `"xcode": null`)
//! has nothing to do: no step is added and nothing is recorded.
//!
//! A guest whose user is ready already but whose record does not cover everything runs these
//! steps alone, then powers off ([`crate::mkuser`]).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mkuser::{Config, MkUserHost};
use crate::sim::{Class, Status, StepFrame};
use crate::sim_primed::{
    MAX_READ_BYTES, Primed, SKIPPED_NOTE, Staged, fixed_argv, parse_staged, record_json,
    root_steps, xcode_identity,
};
use crate::simpaths;

/// The id of the step that removes the old record.
pub const RESET: &str = "prime-reset";
/// The id of the step that writes the record.
pub const RECORD: &str = "prime-record";

/// One root command of the probe, run in the priming boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepStep {
    pub id: String,
    pub argv: Vec<String>,
    pub class: Class,
    pub needs: Vec<String>,
    /// The record read at the start already covers it.
    pub done: bool,
}

/// What the priming boot is to do of the probe's root setup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prep {
    /// The commands, in order; empty when there is nothing to do.
    pub steps: Vec<PrepStep>,
    /// What `prime-record` writes.
    pub record: Option<Value>,
}

impl Prep {
    /// Whether some command has to run (an empty plan, or one whose every command is done
    /// already, needs nothing).
    #[must_use]
    pub fn needed(&self) -> bool {
        self.steps.iter().any(|s| !s.done)
    }

    /// The ids of every step a non-empty plan adds, in order: reset, the commands, the record.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        std::iter::once(RESET.to_owned())
            .chain(self.steps.iter().map(|s| s.id.clone()))
            .chain([RECORD.to_owned()])
            .collect()
    }
}

/// How many times, and how far apart, [`gate`] looks for the share before it concludes that
/// there is none.
pub const SHARE_POLLS: u32 = 40;
pub const SHARE_PAUSE: std::time::Duration = std::time::Duration::from_secs(3);

/// What the boot's look at the share found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Share {
    /// Not looked at: the setup staged no Xcode, so there was nothing to ask for.
    NotChecked,
    /// The directory never listed `results` within the wait (not mounted, or unreadable).
    NotListed,
    /// The share is mounted and lists no prime request: a clone's boot, or a priming boot
    /// whose request did not arrive.
    NoRequest,
    /// The share is mounted and lists the prime request.
    Requested,
}

/// What became of the simulator's root setup in this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootSetup {
    /// The steps ran (the summary's `ok` says whether they all succeeded), or the record
    /// covered all of them already.
    Done,
    /// The setup staged no Xcode: nothing to do.
    NotNeeded,
    /// Steps were left to do and not run, because the prime request was not seen. A priming
    /// boot that ends so did not do what the host asked.
    Skipped,
}

/// Looks at the share: [`simpaths::SHARE_DIR`] lists `results` once it is mounted, and
/// [`simpaths::PRIME_REQUEST_NAME`] when this is the base's priming boot. The disk is the same
/// in the base and in every clone, so nothing on it can tell them apart; the share is the
/// host's, and only `image prime` adds the file. The directory is listed, not stat-ed or read:
/// listing is what the share allows in a guest. The share is mounted a little after boot, so
/// this waits (at most [`SHARE_POLLS`] looks) for the listing; a listing that never comes is
/// [`Share::NotListed`].
pub fn look_at_share(host: &dyn MkUserHost) -> Share {
    for _ in 0..SHARE_POLLS {
        if let Ok(names) = host.list_dir(simpaths::SHARE_DIR)
            && names.iter().any(|n| n == simpaths::RESULTS_NAME)
        {
            return if names.iter().any(|n| n == simpaths::PRIME_REQUEST_NAME) {
                Share::Requested
            } else {
                Share::NoRequest
            };
        }
        host.pause(SHARE_PAUSE);
    }
    Share::NotListed
}

/// The root setup as this boot is to do it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gated {
    /// The steps to run: `prep` when this is the priming boot (or there is nothing to do),
    /// else none. A clone's boot runs `mkuser` too, and the root steps are then `sim`'s (it
    /// runs the ones the record does not cover), never `mkuser`'s: it would race `sim` and
    /// power the clone off mid-probe.
    pub prep: Prep,
    pub share: Share,
    pub root_setup: RootSetup,
}

/// Decides, from the share, whether `prep` runs in this boot.
pub fn gate(host: &dyn MkUserHost, prep: Prep) -> Gated {
    if prep.steps.is_empty() {
        return Gated {
            prep,
            share: Share::NotChecked,
            root_setup: RootSetup::NotNeeded,
        };
    }
    let share = look_at_share(host);
    if share == Share::Requested {
        return Gated {
            prep,
            share,
            root_setup: RootSetup::Done,
        };
    }
    let root_setup = if prep.needed() {
        RootSetup::Skipped
    } else {
        RootSetup::Done
    };
    Gated {
        prep: Prep::default(),
        share,
        root_setup,
    }
}

/// Plans the root setup from the files the setup staged.
#[must_use]
pub fn plan_prep(host: &dyn MkUserHost) -> Prep {
    let read = |path: &str, cap: usize| host.read_file(path, cap);
    let staged = read(simpaths::MANIFEST, MAX_READ_BYTES)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|t| parse_staged(&t).ok());
    let identity = xcode_identity(&read);
    let Some(record) = staged
        .as_ref()
        .and_then(|s| record_json(s, identity.as_deref()))
    else {
        return Prep::default();
    };
    let staged: Staged = staged.unwrap_or_default();
    let old = Primed::load(&read);
    // The first step comes after the removal of the old record.
    let steps: Vec<PrepStep> = root_steps(&staged)
        .into_iter()
        .enumerate()
        .map(|(i, s)| PrepStep {
            done: old.covers(&s.id),
            needs: s
                .needs
                .into_iter()
                .chain((i == 0).then(|| RESET.to_owned()))
                .collect(),
            class: s.class,
            argv: fixed_argv(s.argv),
            id: s.id,
        })
        .collect();
    Prep {
        steps,
        record: Some(record),
    }
}

/// Runs the command of `step` into `f`, or notes that the prime did it before.
pub fn run_step(
    host: &dyn MkUserHost,
    cfg: &Config,
    step: &PrepStep,
    password: &str,
    f: &mut StepFrame,
) {
    f.argv.clone_from(&step.argv);
    f.deadline_secs = cfg.deadline(step.class).as_secs();
    if step.done {
        f.status = Status::Ok;
        f.note = Some(SKIPPED_NOTE.to_owned());
        return;
    }
    let out = host.run(&step.argv, cfg.deadline(step.class));
    crate::mkuser::record(f, &out, cfg, password);
}

/// Removes the record (`prime-reset`).
pub fn reset(host: &dyn MkUserHost, f: &mut StepFrame) {
    f.argv = vec!["remove".to_owned(), simpaths::PRIMED.to_owned()];
    match host.remove_file(simpaths::PRIMED) {
        Ok(()) => {
            f.status = Status::Ok;
            f.exit = Some(0);
        }
        Err(e) => {
            f.status = Status::Error;
            f.note = Some(format!("remove {}: {e}", simpaths::PRIMED));
        }
    }
}

/// Writes the record (`prime-record`), root-owned, readable by all, like `staged.json`.
pub fn write_record(host: &dyn MkUserHost, f: &mut StepFrame, record: &Value) {
    f.argv = vec![
        "write".to_owned(),
        simpaths::PRIMED.to_owned(),
        "0644".to_owned(),
        "root".to_owned(),
    ];
    let text = format!("{record:#}\n");
    match host.write_file(simpaths::PRIMED, text.as_bytes(), 0o644) {
        Ok(()) => {
            f.status = Status::Ok;
            f.exit = Some(0);
        }
        Err(e) => {
            f.status = Status::Error;
            f.note = Some(format!("write {}: {e}", simpaths::PRIMED));
        }
    }
}
