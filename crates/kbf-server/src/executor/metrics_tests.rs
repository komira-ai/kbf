use kbf_proto::reapi::{ActionResult, Digest};
use kbf_proto::worker::Lost;
use kbf_types::LeaseId;

use super::*;
use crate::executor::outcome::{Settled, ran, result_of};

fn end_of(settled: &Settled) -> RunEnd {
    let action = Digest::default();
    RunEnd::of(&result_of(LeaseId::new(1, 1), &action, settled))
}

/// Catches a run counted under an ending the farm was not told: each way a result is formed
/// has its own label.
#[test]
fn each_result_is_counted_under_the_ending_the_farm_is_told() {
    let action = Digest::default();
    let lease = LeaseId::new(1, 1);
    assert_eq!(
        RunEnd::of(&ran(lease, &action, ActionResult::default())),
        RunEnd::Ran
    );
    assert_eq!(end_of(&Settled::Infra("x".to_owned())), RunEnd::Infra);
    assert_eq!(end_of(&Settled::Timeout), RunEnd::Timeout);
    assert_eq!(end_of(&Settled::OutOfMemory), RunEnd::OutOfMemory);
    assert_eq!(end_of(&Settled::Lost(Lost::Fenced)), RunEnd::Lost);
    assert_eq!(end_of(&Settled::Invalid("x".to_owned())), RunEnd::Invalid);
}

/// Catches a counter that lands in another's row: every ending and every ingest result has a
/// row of its own, in the order of the labels.
#[test]
fn every_ending_and_every_ingest_result_has_a_row_of_its_own() {
    let metrics = Metrics::default();
    for (n, end) in RunEnd::ALL.iter().enumerate() {
        for _ in 0..=n {
            metrics.run_ended(*end);
        }
    }
    let ingests: Vec<IngestEnd> = [IngestEnd::Ok, IngestEnd::Unavailable, IngestEnd::Stopped]
        .into_iter()
        .chain(
            REASONS
                .iter()
                .map(|(reason, _)| IngestEnd::Rejected(*reason)),
        )
        .collect();
    assert_eq!(ingests.len(), IngestEnd::labels().count());
    for (n, end) in ingests.iter().enumerate() {
        for _ in 0..=n {
            metrics.ingested(*end);
        }
    }
    let mut text = String::new();
    metrics.render(&mut text);
    for (n, end) in RunEnd::ALL.iter().enumerate() {
        let row = format!(
            "kbf_executor_runs_total{{outcome=\"{}\"}} {}\n",
            end.label(),
            n + 1
        );
        assert!(text.contains(&row), "{row}in {text}");
    }
    for (n, label) in IngestEnd::labels().enumerate() {
        let row = format!(
            "kbf_executor_ingests_total{{result=\"{label}\"}} {}\n",
            n + 1
        );
        assert!(text.contains(&row), "{row}in {text}");
    }
}

/// Catches a plain counter that is not rendered, is rendered under another name, or counts
/// with another's calls: each of them is counted a different number of times, and its row
/// carries that number and its own name, as a counter.
#[test]
fn each_plain_counter_renders_under_its_name_and_counts() {
    let metrics = Metrics::default();
    let calls: [(&str, &dyn Fn(&Metrics), u32); 6] = [
        ("kbf_executor_late_starts_total", &Metrics::late_start, 1),
        (
            "kbf_executor_jobs_submitted_total",
            &Metrics::job_submitted,
            2,
        ),
        (
            "kbf_executor_jobs_resubmitted_after_lost_answer_total",
            &Metrics::job_resubmitted,
            3,
        ),
        (
            "kbf_executor_jobs_cancelled_total",
            &Metrics::job_cancelled,
            4,
        ),
        (
            "kbf_executor_jobs_adopted_cancelled_total",
            &Metrics::job_adopted_cancelled,
            5,
        ),
        (
            "kbf_executor_host_silence_seen_total",
            &Metrics::host_silence,
            6,
        ),
    ];
    for (_, call, times) in &calls {
        for _ in 0..*times {
            call(&metrics);
        }
    }
    let mut text = String::new();
    metrics.render(&mut text);
    for (name, _, times) in &calls {
        let row = format!("{name} {times}\n");
        assert!(text.contains(&row), "{row}in {text}");
        let family = format!("# TYPE {name} counter\n");
        assert!(text.contains(&family), "{family}in {text}");
    }
    for family in [
        "# TYPE kbf_executor_runs_total counter",
        "# TYPE kbf_executor_ingests_total counter",
    ] {
        assert!(text.contains(family), "{family}");
    }
    assert!(
        !text.contains("leases_held"),
        "no gauge of held leases: {text}"
    );
    assert!(
        !text.contains("stray_leases"),
        "no stray lease counter: {text}"
    );
}
