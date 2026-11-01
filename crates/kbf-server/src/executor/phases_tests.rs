use super::*;

/// The line of `text` for `series`, without its name and space.
fn value<'a>(text: &'a str, series: &str) -> &'a str {
    let prefix = format!("{series} ");
    let mut lines = text.lines().filter(|line| line.starts_with(&prefix));
    let line = lines
        .next()
        .unwrap_or_else(|| panic!("no {series} in\n{text}"));
    assert!(lines.next().is_none(), "{series} is listed twice");
    &line[prefix.len()..]
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn rendered(metrics: &Metrics) -> String {
    let mut text = String::new();
    metrics.render(&mut text);
    text
}

/// Catches a phase measured between the wrong two stamps: with every stamp read from values off
/// one base (no clock read, so nothing here depends on how long the test took), each stamp
/// returns exactly the phases it ends, with the difference of the two readings.
#[test]
fn phases_are_recorded_in_order_from_the_injected_clock() {
    let base = Instant::now();
    let at = |n: u64| base + secs(n);
    let mut line = Timeline::default();

    assert!(line.mark(Mark::Seen, at(0)).is_empty());
    assert_eq!(
        line.mark(Mark::Prepared, at(2)),
        [(Phase::Prepare, secs(2))]
    );
    assert_eq!(line.mark(Mark::Posted, at(3)), [(Phase::Start, secs(1))]);
    assert_eq!(line.mark(Mark::Running, at(8)), [(Phase::Queue, secs(5))]);
    assert_eq!(
        line.mark(Mark::Ended, at(20)),
        [(Phase::Task, secs(12)), (Phase::Run, secs(17))]
    );
    assert_eq!(line.mark(Mark::Done, at(21)), [(Phase::Ingest, secs(1))]);
}

/// Catches a phase made up for a run that was not seen: a run posted and never read back running
/// has no `queue`, `task` or `run`; one first seen ended has a `run` and no `queue` or `task`;
/// and an ingest with no ended stamp is no `ingest`.
#[test]
fn a_run_that_never_starts_records_no_task_phase() {
    let base = Instant::now();
    let at = |n: u64| base + secs(n);

    let mut never = Timeline::default();
    never.mark(Mark::Seen, at(0));
    never.mark(Mark::Prepared, at(1));
    never.mark(Mark::Posted, at(2));
    assert!(never.mark(Mark::Done, at(30)).is_empty());

    let mut fast = Timeline::default();
    fast.mark(Mark::Seen, at(0));
    fast.mark(Mark::Prepared, at(1));
    fast.mark(Mark::Posted, at(2));
    assert_eq!(fast.mark(Mark::Ended, at(9)), [(Phase::Run, secs(7))]);
}

/// Catches a phase observed again, or moved, by a stamp set twice (a run read back as running on
/// every pass): the first stamp is kept and the second ends nothing.
#[test]
fn a_stamp_is_kept_once() {
    let base = Instant::now();
    let at = |n: u64| base + secs(n);
    let mut line = Timeline::default();
    line.mark(Mark::Posted, at(0));
    assert_eq!(line.mark(Mark::Running, at(4)), [(Phase::Queue, secs(4))]);
    assert!(line.mark(Mark::Running, at(9)).is_empty());
    assert_eq!(
        line.mark(Mark::Ended, at(10)),
        [(Phase::Task, secs(6)), (Phase::Run, secs(10))]
    );
}

/// Catches `stamp` not reaching the metric: the phases a stamp ends are counted, with their
/// seconds, under their own label and no other.
#[test]
fn a_stamp_is_observed_into_the_metric() {
    let metrics = Metrics::default();
    let base = Instant::now();
    let mut line = Timeline::default();
    line.stamp(Mark::Seen, base, &metrics);
    line.stamp(
        Mark::Prepared,
        base + Duration::from_millis(2_250),
        &metrics,
    );

    let text = rendered(&metrics);
    assert_eq!(
        value(&text, "kbf_executor_phase_seconds_count{phase=\"prepare\"}"),
        "1"
    );
    assert_eq!(
        value(&text, "kbf_executor_phase_seconds_sum{phase=\"prepare\"}"),
        "2.250000"
    );
    for other in [
        Phase::Start,
        Phase::Queue,
        Phase::Task,
        Phase::Run,
        Phase::Ingest,
    ] {
        let series = format!(
            "kbf_executor_phase_seconds_count{{phase=\"{}\"}}",
            other.label()
        );
        assert_eq!(value(&text, &series), "0");
    }
}

/// Catches a family that lists a phase twice or not at all, or loses its type line: every label
/// has one count, one sum and one `+Inf` bucket, and the family is declared a histogram once.
#[test]
fn the_metric_family_renders_every_phase_label_once() {
    let text = rendered(&Metrics::default());
    assert_eq!(
        text.matches("# TYPE kbf_executor_phase_seconds histogram\n")
            .count(),
        1
    );
    assert_eq!(Phase::ALL.len(), 6);
    for phase in Phase::ALL {
        let label = phase.label();
        let family = "kbf_executor_phase_seconds";
        assert_eq!(
            value(&text, &format!("{family}_count{{phase=\"{label}\"}}")),
            "0"
        );
        assert_eq!(
            value(&text, &format!("{family}_sum{{phase=\"{label}\"}}")),
            "0.000000"
        );
        assert_eq!(
            value(
                &text,
                &format!("{family}_bucket{{phase=\"{label}\",le=\"+Inf\"}}")
            ),
            "0"
        );
    }
}

/// Catches a duration in the wrong bucket: a bound is inclusive, a duration over every bound is
/// only in `+Inf`, the buckets are cumulative, and the sum and the count are the observations'.
#[test]
fn an_observation_lands_in_every_bucket_at_or_over_it() {
    let metrics = Metrics::default();
    metrics.phase_took(Phase::Queue, Duration::from_millis(5));
    metrics.phase_took(Phase::Queue, Duration::from_millis(6));
    metrics.phase_took(Phase::Queue, secs(3_601));

    let text = rendered(&metrics);
    let bucket = |le: &str| {
        value(
            &text,
            &format!("kbf_executor_phase_seconds_bucket{{phase=\"queue\",le=\"{le}\"}}"),
        )
        .to_owned()
    };
    assert_eq!(bucket("0.005"), "1");
    assert_eq!(bucket("0.025"), "2");
    assert_eq!(bucket("3600"), "2");
    assert_eq!(bucket("+Inf"), "3");
    assert_eq!(
        value(&text, "kbf_executor_phase_seconds_count{phase=\"queue\"}"),
        "3"
    );
    assert_eq!(
        value(&text, "kbf_executor_phase_seconds_sum{phase=\"queue\"}"),
        "3601.011000"
    );
    let other = "kbf_executor_phase_seconds_count{phase=\"task\"}";
    assert_eq!(value(&text, other), "0");
}
