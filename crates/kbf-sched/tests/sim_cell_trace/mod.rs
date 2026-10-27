//! The trace of the simulated cell in `sim_cell.rs`: its messages as trace lines spell
//! them (every field written by hand, in the shapes of `kbf_sim::text`, never through
//! `Debug`), and the golden hashes of its runs.

use kbf_sim::text;
use kbf_sim::{TraceHash, golden::GoldenFile};

use crate::{BASE, Fault, Msg, REBOOT_AT, RESTART_AT, Scenario, run_scenario};

/// The message as one trace line.
pub fn msg(msg: &Msg) -> String {
    match msg {
        Msg::Append(record) => format!("append({})", text::record(record)),
        Msg::Committed { index, record } => {
            format!("committed(index={index} {})", text::record(record))
        }
        Msg::Hello { capacity, session } => {
            format!("hello(session={session} {})", text::resources(capacity))
        }
        Msg::Heartbeat {
            sent_at,
            session,
            running,
        } => format!(
            "heartbeat(sent_at={} session={session} running={})",
            text::time(*sent_at),
            text::list(running, |l| text::lease(*l))
        ),
        Msg::Ack { sent_at, session } => {
            format!("ack(sent_at={} session={session})", text::time(*sent_at))
        }
        Msg::Start { start, session } => format!("start(session={session} {})", text::start(start)),
        Msg::Started { operation, lease } => format!(
            "started(op={} lease={})",
            text::operation(*operation),
            text::lease(*lease)
        ),
        Msg::Report {
            operation,
            lease,
            outcome,
        } => format!(
            "report(op={} lease={} outcome={})",
            text::operation(*operation),
            text::lease(*lease),
            text::outcome(*outcome)
        ),
        Msg::ReportAck { lease } => format!("report_ack(lease={})", text::lease(*lease)),
    }
}

fn golden() -> GoldenFile {
    GoldenFile::new(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/trace-hashes.txt"),
        "cell",
        "cargo test -p kbf-sched -- --ignored regenerate_golden_hashes",
    )
}

/// The trace hash of the cell under each fault scenario and three seeds.
fn golden_runs() -> Vec<(String, TraceHash)> {
    let scenarios = [
        ("base", BASE),
        (
            "reboot-and-restart",
            Scenario {
                cut: false,
                worker_1: Fault::Reboot(REBOOT_AT),
                worker_2: Fault::Restart(RESTART_AT),
            },
        ),
        (
            "drop-first-start",
            Scenario {
                cut: false,
                worker_1: Fault::DropFirstStart,
                worker_2: Fault::None,
            },
        ),
        (
            "hide-first-hermetic",
            Scenario {
                cut: false,
                worker_1: Fault::HideFirstHermetic,
                worker_2: Fault::None,
            },
        ),
    ];
    let mut runs = Vec::new();
    for (name, scenario) in scenarios {
        for seed in 0..3 {
            let hash = run_scenario(seed, scenario).trace_hash();
            runs.push((format!("{name}/seed-{seed}"), hash));
        }
    }
    runs
}

/// Catches: a change to the scheduler, the cell or the trace text that alters what a
/// scenario decides, or how its trace reads, without the author meaning it: the cell's
/// trace under four fault scenarios and three seeds is compared with the checked-in
/// hash.
#[test]
fn traces_match_their_golden_hashes() {
    golden().assert_matches(&golden_runs());
}

/// Rewrites the cell lines of `tests/golden/trace-hashes.txt`.
#[test]
#[ignore = "regenerates the golden trace hashes; run it only on purpose"]
fn regenerate_golden_hashes() {
    golden().regenerate(&golden_runs()).unwrap();
}
