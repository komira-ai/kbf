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

use serde_json::Value;

use crate::mkuser::{Config, MkUserHost};
use crate::sim::{Class, Status, StepFrame};
use crate::sim_primed::{
    MAX_READ_BYTES, Primed, SKIPPED_NOTE, Staged, fixed_argv, parse_staged, record_json, root_steps,
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

/// Plans the root setup from the files the setup staged.
#[must_use]
pub fn plan_prep(host: &dyn MkUserHost) -> Prep {
    let read = |path: &str, cap: usize| host.read_file(path, cap);
    let staged = read(simpaths::MANIFEST, MAX_READ_BYTES)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|t| parse_staged(&t).ok());
    let Some(record) = staged.as_ref().and_then(record_json) else {
        return Prep::default();
    };
    let staged: Staged = staged.unwrap_or_default();
    let old = Primed::load(&read);
    let mut steps: Vec<PrepStep> = root_steps(&staged)
        .into_iter()
        .map(|s| PrepStep {
            done: old.covers(&s.id),
            needs: s.needs,
            class: s.class,
            argv: fixed_argv(s.argv),
            id: s.id,
        })
        .collect();
    if let Some(first) = steps.first_mut() {
        first.needs.push(RESET.to_owned());
    }
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
