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

use crate::frame::{Frame, Kind, SendError};

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

/// How many times, and how far apart, [`ask_host`] tries to reach the host before it
/// concludes that there is no answer.
pub const ASK_ATTEMPTS: u32 = 20;
pub const ASK_PAUSE: std::time::Duration = std::time::Duration::from_secs(3);
/// The name of the frame that asks the host whether this boot is the priming one.
pub const QUESTION: &str = "prime-question";
/// The body of the host's `ack` that says it is. Anything else says it is not.
pub const ANSWER_YES: &str = "yes";

/// What the question to the host got, as the summary reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// Not asked: the setup staged no Xcode, so there was nothing to ask for.
    NotChecked,
    /// The host answered yes: this is the base's priming boot.
    Yes,
    /// The host answered, and not yes (any other host, a clone's boot, an older host).
    No,
    /// The host read the question and refused it, with this reason.
    Refused(String),
    /// No connection or reply in [`ASK_ATTEMPTS`] tries; the text is the last error.
    NoAnswer(String),
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
    /// Steps were left to do and not run, because the host did not say this is the priming
    /// boot. A priming boot that ends so did not do what the host asked.
    Skipped,
}

/// Asks the host, over the results channel (vsock `port`), whether this is the base's priming
/// boot. The disk is the same in the base and in every clone, so nothing on it can tell them
/// apart; only `image prime` answers yes, and a host that does not know the question (a probe
/// run, an older `kbf-vmm`) acknowledges it without saying yes. The channel is the one thing
/// measured to work in every guest boot, where reading the share is not. The host may not be
/// listening yet, so a failed try is repeated, at most [`ASK_ATTEMPTS`] times, [`ASK_PAUSE`]
/// apart; a refusal is final.
pub fn ask_host(host: &dyn MkUserHost, port: u32) -> Signal {
    let question = Frame::new(Kind::Result, QUESTION, "");
    let mut last = String::new();
    for attempt in 1..=ASK_ATTEMPTS {
        match host.ask(port, &question) {
            Ok(a) if a == ANSWER_YES => return Signal::Yes,
            Ok(_) => return Signal::No,
            Err(SendError::Refused(why)) => return Signal::Refused(why),
            Err(e) => last = e.to_string(),
        }
        if attempt < ASK_ATTEMPTS {
            host.pause(ASK_PAUSE);
        }
    }
    Signal::NoAnswer(last)
}

/// The root setup as this boot is to do it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gated {
    /// The steps to run: `prep` when this is the priming boot (or there is nothing to do),
    /// else none. A clone's boot runs `mkuser` too, and the root steps are then `sim`'s (it
    /// runs the ones the record does not cover), never `mkuser`'s: it would race `sim` and
    /// power the clone off mid-probe.
    pub prep: Prep,
    pub signal: Signal,
    pub root_setup: RootSetup,
}

/// Decides, from the host's answer, whether `prep` runs in this boot.
pub fn gate(host: &dyn MkUserHost, port: u32, prep: Prep) -> Gated {
    if prep.steps.is_empty() {
        return Gated {
            prep,
            signal: Signal::NotChecked,
            root_setup: RootSetup::NotNeeded,
        };
    }
    let signal = ask_host(host, port);
    if signal == Signal::Yes {
        return Gated {
            prep,
            signal,
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
        signal,
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
