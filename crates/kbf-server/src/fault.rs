//! Farm faults as the server sees them (failure classes, 6.5; issue #22): the
//! scheduler's outcome for a daemon `Result` that says the farm lost the run, and what
//! callers are told once the scheduler finishes an operation whose farm reruns are
//! used up.
//!
//! The scheduler decides whether a run the farm lost or a busy node killed runs again
//! (`kbf_types::FarmRun`); the server only translates. An operation finished that way
//! is answered INTERNAL, naming the last node, what ended its run, the operator's fix,
//! and every farm run, so the person reading the build log knows it is the farm's.

use kbf_front::Finished;
use kbf_proto::google::rpc;
use kbf_proto::worker::{self, Lost};
use kbf_sched::NOT_STARTED_RERUNS;
use kbf_types::{FarmFault, FarmRun, LostWhy};
use tonic::Code;

/// How the farm lost the run `result` reports, if it says so: its status is not OK and
/// it names a reason. `None` for every other `Result`.
pub(crate) fn lost(result: &worker::Result) -> Option<LostWhy> {
    let ok = result
        .status
        .as_ref()
        .is_none_or(|s| s.code == Code::Ok as i32);
    if ok {
        return None;
    }
    match Lost::try_from(result.lost) {
        Ok(Lost::DaemonRestarted) => Some(LostWhy::DaemonRestarted),
        Ok(Lost::Fenced) => Some(LostWhy::Fenced),
        Ok(Lost::Unspecified) | Err(_) => None,
    }
}

/// What the callers of an operation the scheduler finished as a farm fault, past its
/// farm reruns, are told; `runs` are its farm runs, oldest first.
pub(crate) fn finished(runs: &[FarmRun]) -> Finished {
    if runs.is_empty() {
        // No run ended as a farm fault: the scheduler gave up leases the nodes never
        // started (`kbf_sched::NOT_STARTED_RERUNS`), so there is no node or run to name.
        return Finished::Failed(rpc::Status {
            code: Code::Internal as i32,
            message: format!(
                "kbf farm fault: the farm never started the action; {} leases were not \
                 listed by their nodes within the start grace. Operator fix: read the node \
                 daemons' logs for why they dropped the Start.",
                NOT_STARTED_RERUNS + 1
            ),
            details: Vec::new(),
        });
    }
    let last = runs.last();
    let node = last.map_or("", |r| r.worker.as_str());
    let what = last.map_or_else(String::new, |r| r.fault.to_string());
    let fix = last.map_or("", |r| operator_fix(r.fault));
    let list: Vec<String> = runs
        .iter()
        .map(|r| format!("{}, {}", r.worker, short(r.fault)))
        .collect();
    let message = format!(
        "kbf farm fault on {node}: {what}, and its reruns are used up. Operator fix: {fix}. \
         Runs: {} ({}).",
        runs.len(),
        list.join("; ")
    );
    Finished::Failed(rpc::Status {
        code: Code::Internal as i32,
        message,
        details: Vec::new(),
    })
}

/// What an operator does about `fault`.
const fn operator_fix(fault: FarmFault) -> &'static str {
    match fault {
        FarmFault::NodeMemoryPressure => "find what else holds memory on the node",
        FarmFault::Lost(LostWhy::DaemonRestarted) => {
            "find why the node's daemon restarted (its log says how it ended)"
        }
        FarmFault::Lost(LostWhy::Fenced) => "check the node's network path to the server",
        FarmFault::Infra => "read the node daemon's log for why its driver failed",
    }
}

/// `fault` as one run of the list reads it.
const fn short(fault: FarmFault) -> &'static str {
    match fault {
        FarmFault::NodeMemoryPressure => "node memory pressure",
        FarmFault::Lost(LostWhy::DaemonRestarted) => "daemon restarted",
        FarmFault::Lost(LostWhy::Fenced) => "fenced",
        FarmFault::Infra => "infra failure",
    }
}
