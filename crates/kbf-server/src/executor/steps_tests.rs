use std::time::Duration;

use super::*;
use crate::executor::metrics::Metrics;

fn rendered(metrics: &Metrics) -> String {
    let mut out = String::new();
    metrics.render(&mut out);
    out
}

fn value<'a>(text: &'a str, series: &str) -> &'a str {
    let prefix = format!("{series} ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no {series} in {text}"))
}

/// Catches a family that lists a step twice or not at all, or loses its type line: every step
/// has one count, one sum and one `+Inf` bucket, and the family is declared a histogram once.
#[test]
fn the_step_family_renders_every_step_label_once() {
    let text = rendered(&Metrics::default());
    let family = "kbf_executor_step_seconds";
    assert_eq!(
        text.matches(&format!("# TYPE {family} histogram\n"))
            .count(),
        1
    );
    let labels: Vec<&str> = Step::ALL.iter().map(|step| step.label()).collect();
    assert_eq!(
        labels,
        [
            "facts",
            "put_dir",
            "action",
            "walk",
            "held",
            "tree",
            "tree_wait",
            "row",
            "rpc",
            "backoff"
        ]
    );
    for label in labels {
        assert_eq!(
            value(&text, &format!("{family}_count{{step=\"{label}\"}}")),
            "0"
        );
        assert_eq!(
            value(
                &text,
                &format!("{family}_bucket{{step=\"{label}\",le=\"+Inf\"}}")
            ),
            "0"
        );
    }
}

/// Catches a step in another step's series, or a millisecond step lost in a coarse bucket: a
/// 1 ms `rpc` is in the `0.001` bucket, a 40 ms one in `0.05` and not `0.025`, and no other
/// step is counted.
#[test]
fn a_step_lands_in_its_own_series_down_to_a_millisecond() {
    let metrics = Metrics::default();
    metrics.step_took(Step::Rpc, Duration::from_millis(1));
    metrics.step_took(Step::Rpc, Duration::from_millis(40));
    let text = rendered(&metrics);
    let bucket = |le: &str| {
        value(
            &text,
            &format!("kbf_executor_step_seconds_bucket{{step=\"rpc\",le=\"{le}\"}}"),
        )
        .to_owned()
    };
    assert_eq!(bucket("0.001"), "1");
    assert_eq!(bucket("0.025"), "1");
    assert_eq!(bucket("0.05"), "2");
    assert_eq!(
        value(&text, "kbf_executor_step_seconds_sum{step=\"rpc\"}"),
        "0.041000"
    );
    for step in Step::ALL.iter().filter(|step| **step != Step::Rpc) {
        let series = format!(
            "kbf_executor_step_seconds_count{{step=\"{}\"}}",
            step.label()
        );
        assert_eq!(value(&text, &series), "0", "{series}");
    }
}
