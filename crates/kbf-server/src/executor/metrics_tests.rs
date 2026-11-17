use super::*;
use crate::executor::outcome::Settled;

/// Catches a run counted under an ending it did not have: each way a run is settled has its
/// own label, and the labels are the six of `RunEnd::ALL`, each once.
#[test]
fn each_settled_run_is_counted_under_its_own_ending() {
    let ends = [
        RunEnd::of(&Settled::Infra("x".to_owned())),
        RunEnd::of(&Settled::Timeout),
        RunEnd::of(&Settled::OutOfMemory),
        RunEnd::of(&Settled::Lost("gone")),
        RunEnd::of(&Settled::Invalid("x".to_owned())),
    ];
    assert_eq!(
        ends,
        [
            RunEnd::Infra,
            RunEnd::Timeout,
            RunEnd::OutOfMemory,
            RunEnd::Lost,
            RunEnd::Invalid
        ]
    );
    let labels: std::collections::BTreeSet<&str> =
        RunEnd::ALL.iter().map(|end| end.label()).collect();
    assert_eq!(labels.len(), RunEnd::ALL.len());
    assert!(labels.contains("ran"));
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
    let ingests: Vec<IngestEnd> = [IngestEnd::Ok, IngestEnd::Unavailable]
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

/// A counter's name, the call that counts it once, and how many times it is called.
type Counted<'a> = (&'a str, &'a dyn Fn(&Metrics), u32);

/// Catches a plain counter that is not rendered, is rendered under another name, or counts
/// with another's calls: each of them is counted a different number of times, and its row
/// carries that number and its own name, as a counter.
#[test]
fn each_plain_counter_renders_under_its_name_and_counts() {
    let metrics = Metrics::default();
    let calls: [Counted<'_>; 6] = [
        (
            "kbf_executor_jobs_submitted_total",
            &Metrics::job_submitted,
            1,
        ),
        (
            "kbf_executor_jobs_resubmitted_after_lost_answer_total",
            &Metrics::job_resubmitted,
            2,
        ),
        (
            "kbf_executor_attempts_resubmitted_total",
            &Metrics::attempt_resubmitted,
            3,
        ),
        (
            "kbf_executor_jobs_cancelled_total",
            &Metrics::job_cancelled,
            4,
        ),
        (
            "kbf_executor_operations_reattached_total",
            &Metrics::reattached,
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
    for gone in [
        "leases_held",
        "stray_leases",
        "late_starts",
        "adopted_cancelled",
    ] {
        assert!(!text.contains(gone), "no {gone} family: {text}");
    }
}
