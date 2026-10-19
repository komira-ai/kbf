//! The scrape loop: fetch, decide, save the state, write the verdict, and stop when the
//! bake is over. The verdict file is sealed before the state file says the bake ended, so a
//! crash between the two leaves a bake that decides again, never one that is over with no
//! verdict.

use kbf_gate::{GateError, Metrics, Run, Status, Window};

use crate::eval::{Decision, Evaluator, Observation};
use crate::state::StateError;
use crate::updater::UpdaterLog;

/// A scrape that fails this many times in a row ends the run as errored: the bake extends.
pub const MAX_FAILURES: u32 = 3;

/// One fetch of the farm.
pub struct Fetched {
    /// The `/metrics` text.
    pub text: String,
    /// Builds counted since the bake began, when the invocation records could be read.
    pub builds: Option<u64>,
}

/// Why the loop stopped without a verdict of its own.
#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    /// The verdict could not be written.
    #[error(transparent)]
    Gate(#[from] GateError),
    /// The state could not be saved.
    #[error(transparent)]
    State(#[from] StateError),
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Green, red, or extended.
    pub status: Status,
    /// Why, in a sentence.
    pub note: String,
}

/// What the loop gives the sealing step.
enum Stop {
    Decided(Status, String),
    Errored(String),
}

/// Run the bake until it is decided. `fetch` reads the farm, `updater` the phase log, `sleep`
/// waits for `interval` seconds. The verdict in `run` is sealed on every way out: green,
/// red, extended (the farm could not be read, or the load was too thin) and, for an error of
/// the evaluator's own, extended with the error.
pub fn drive(
    eval: &mut Evaluator,
    mut run: Run,
    state_path: &std::path::Path,
    interval: u64,
    fetch: &mut dyn FnMut() -> Result<Fetched, String>,
    updater: &mut dyn FnMut() -> Option<UpdaterLog>,
    sleep: &mut dyn FnMut(u64),
) -> Result<Outcome, DriveError> {
    let stop = scrape_loop(eval, &mut run, state_path, interval, fetch, updater, sleep);
    let (status, note) = match stop {
        Ok(Stop::Decided(status, note)) => (status, note),
        Ok(Stop::Errored(why)) => (Status::Extended, why),
        Err(e) => {
            let _ = run.extend(&format!("the evaluator failed: {e}"));
            return Err(e);
        }
    };
    let sealed = match status {
        Status::Green => run.finish()?,
        Status::Extended => {
            run.extend(&note)?;
            Status::Extended
        }
        Status::Red | Status::Running => {
            run.fail(&note)?;
            Status::Red
        }
    };
    if matches!(sealed, Status::Green | Status::Red) {
        eval.mark_ended(sealed);
    }
    eval.state().save(state_path)?;
    Ok(Outcome {
        status: sealed,
        note,
    })
}

fn scrape_loop(
    eval: &mut Evaluator,
    run: &mut Run,
    state_path: &std::path::Path,
    interval: u64,
    fetch: &mut dyn FnMut() -> Result<Fetched, String>,
    updater: &mut dyn FnMut() -> Option<UpdaterLog>,
    sleep: &mut dyn FnMut(u64),
) -> Result<Stop, DriveError> {
    let mut failures = 0;
    loop {
        let at = run.now();
        let read = fetch().and_then(|f| {
            let m = Metrics::parse(&f.text).map_err(|e| e.to_string())?;
            Ok((m, f.builds))
        });
        match read {
            Err(why) => {
                failures += 1;
                if failures >= MAX_FAILURES {
                    return Ok(Stop::Errored(format!(
                        "the farm could not be read {failures} times in a row: {why}"
                    )));
                }
            }
            Ok((metrics, builds)) => {
                failures = 0;
                let log = updater();
                let d = eval.observe(&Observation {
                    at,
                    metrics: &metrics,
                    builds,
                    updater: log.as_ref(),
                });
                record(run, eval, &d, at)?;
                eval.state().save(state_path)?;
                if let Some(end) = d.end {
                    return Ok(Stop::Decided(end.status, end.note));
                }
            }
        }
        sleep(interval);
    }
}

/// Put what this scrape decided into the verdict document.
fn record(run: &mut Run, eval: &Evaluator, d: &Decision, at: u64) -> Result<(), GateError> {
    let st = eval.state();
    let over = d.end.is_some();
    run.update(|doc| {
        doc.started = st.bake_start;
        doc.desired_generation = st.desired_generation;
        doc.deploy = st.deploy;
        doc.bake = Some(Window {
            start: st.bake_start,
            end: over.then_some(at),
        });
        doc.load = Some(d.load);
        doc.waivers = st.waivers.clone();
        doc.gates = d.gates.clone();
    })
}
