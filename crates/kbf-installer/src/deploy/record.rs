//! The deploy's result document (`kbf-gate`): written `RUNNING` before the first request and
//! `FINISHED` before [`super::deploy`] returns.

use kbf_gate::{CommitId, Header, Run as GateRun, SetHash, Stage, Status, Window, system_clock};

use super::summary::Exit;
use super::{Fail, Run, fail};

/// Start the document, for an applied deploy that names a path. A document that cannot be
/// written ends the deploy before it changes anything: a deploy nobody can read the verdict
/// of is not one to start.
pub(super) fn open(run: &Run) -> Result<Option<GateRun>, Fail> {
    let (true, Some(path)) = (run.opts.apply, run.opts.result.as_ref()) else {
        return Ok(None);
    };
    let began = run.env.clock.now();
    let commit = CommitId::parse(run.opts.commit.as_str())
        .map_err(|e| fail(Exit::Usage, "bad_commit", e.to_string()))?;
    // Unknown until preflight lists the release (or the caller gave it): the zero hash is
    // there to be replaced, and a document that dies first is red anyway.
    let set_hash = run
        .opts
        .set_hash
        .clone()
        .unwrap_or_else(|| SetHash::parse(&"0".repeat(64)).expect("64 hex digits"));
    let header = Header {
        stage: Stage::Gamma,
        commit,
        set_hash,
        run_id: format!("deploy-{}-{began}", &run.opts.commit.as_str()[..12]),
        attempt: 1,
    };
    GateRun::start(path, header, system_clock)
        .map(Some)
        .map_err(|e| fail(Exit::Failed, "result_unwritable", e.to_string()))
}

/// Finish the document by the way the deploy ended: exit 0 by its gates, exit 5 (the smoke did
/// not run) as no decision, anything else red with the reason. A document that cannot be
/// written turns an `ok` into a failure.
pub(super) fn seal(run: &mut Run, mut record: GateRun, began: u64) {
    let now = run.env.clock.now();
    let gates = run.gates.results(now);
    let hash = run
        .opts
        .set_hash
        .clone()
        .or_else(|| run.gates.derived_set_hash.clone());
    let generation = run.sum.generation;
    let wrote = record.update(|doc| {
        doc.gates = gates;
        doc.desired_generation = generation;
        doc.deploy = Some(Window {
            start: began,
            end: Some(now),
        });
        if let Some(hash) = hash {
            doc.set_hash = hash;
        }
    });
    let note = match run.sum.detail.as_str() {
        "" => run.sum.reason.clone(),
        detail => format!("{}: {detail}", run.sum.reason),
    };
    let sealed = match (wrote, run.sum.exit) {
        (Err(e), _) => Err(e),
        (Ok(()), 0) => record.finish().map(|status| {
            // The gates decide an ok deploy; one that is not green is a failure.
            if status != Status::Green {
                run.sum
                    .end(Exit::Failed, "gate_red", "the result document is not green");
            }
        }),
        (Ok(()), 5) => record.extend(&note),
        (Ok(()), _) => record.fail(&note),
    };
    if let Err(e) = sealed {
        run.sum
            .end(Exit::Failed, "result_unwritable", &e.to_string());
    }
}
