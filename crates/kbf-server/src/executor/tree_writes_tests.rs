use std::time::Duration;

use kbf_front::action_tree::{WriteStage, WriteTimed, WriteWaits};

use crate::executor::config::InputTree;
use crate::executor::metrics::Metrics;

fn rendered(metrics: &Metrics) -> String {
    let mut out = String::new();
    metrics.render(&mut out);
    out
}

/// The series of `family` whose `part` (`count`, `sum` or a bucket) is not `zero`, with their
/// values, in the order the page lists them.
fn nonzero(text: &str, family: &str, zero: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.strip_prefix(family))
        .filter_map(|line| line.rsplit_once(' '))
        .filter(|(_, value)| *value != zero)
        .map(|(series, value)| (series.to_owned(), value.to_owned()))
        .collect()
}

const fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Catches a family that lists a series twice or not at all, gives a tree list a manifest slot
/// it never takes, or loses its type line: the write family has each part of a manifest's write
/// and of the root's, and a tree list's parts but `manifest_slot`, once each and at zero; the
/// manifests family has one bucket per bound from 0 to 5,000 and `+Inf`, at zero.
#[test]
fn both_families_render_every_series_once_at_zero() {
    let text = rendered(&Metrics::default());
    for family in [
        "kbf_executor_tree_write_seconds",
        "kbf_executor_tree_manifests_written",
    ] {
        let declared = format!("# TYPE {family} histogram\n");
        assert_eq!(text.matches(&declared).count(), 1, "{family}");
    }
    let counted: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("kbf_executor_tree_write_seconds_count{"))
        .filter_map(|line| line.strip_suffix("} 0"))
        .collect();
    let mut want = Vec::new();
    for stage in ["tree_manifests", "tree_root"] {
        for part in ["manifest_slot", "store_slot", "store_503", "request"] {
            want.push(format!(r#"stage="{stage}",part="{part}""#));
        }
    }
    for part in ["store_slot", "store_503", "request"] {
        want.push(format!(r#"stage="tree_list",part="{part}""#));
    }
    assert_eq!(counted, want, "each series once, in order, at zero");
    let buckets: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("kbf_executor_tree_manifests_written_bucket{le=\""))
        .filter_map(|line| line.strip_suffix("\"} 0"))
        .collect();
    let bounds = [
        "0", "1", "2", "5", "10", "20", "50", "100", "200", "500", "1000", "2000", "5000", "+Inf",
    ];
    assert_eq!(buckets, bounds);
    assert!(text.contains("\nkbf_executor_tree_manifests_written_sum 0\n"));
    assert!(text.contains("\nkbf_executor_tree_manifests_written_count 0\n"));
}

/// Catches a part counted in another part's or another stage's series, a tree list's zero
/// manifest slot counted, or a stage's time lost from the step family: one root write of 1, 2, 3
/// and 4 ms lands in its four series with those sums; one list write lands in its three; a
/// blob's or a sidecar's write is not counted; and the stage itself is the step's.
#[test]
fn each_part_of_a_write_lands_in_its_own_series() {
    let metrics = Metrics::default();
    let waits = WriteWaits {
        manifest_slot: ms(1),
        store_slot: ms(2),
        store_503: ms(3),
        request: ms(4),
    };
    let form = InputTree::Manifests;
    metrics.tree_write(form, WriteTimed::Write(WriteStage::Root, waits));
    metrics.tree_write(form, WriteTimed::Write(WriteStage::List, waits));
    metrics.tree_write(form, WriteTimed::Write(WriteStage::Blobs, waits));
    metrics.tree_write(form, WriteTimed::Write(WriteStage::Sidecar, waits));
    metrics.tree_write(form, WriteTimed::Stage(WriteStage::Root, ms(10)));
    let text = rendered(&metrics);
    let sums = nonzero(&text, "kbf_executor_tree_write_seconds_sum", "0.000000");
    let sum = |stage: &str, part: &str, value: &str| {
        (
            format!(r#"{{stage="{stage}",part="{part}"}}"#),
            value.to_owned(),
        )
    };
    assert_eq!(
        sums,
        [
            sum("tree_root", "manifest_slot", "0.001000"),
            sum("tree_root", "store_slot", "0.002000"),
            sum("tree_root", "store_503", "0.003000"),
            sum("tree_root", "request", "0.004000"),
            sum("tree_list", "store_slot", "0.002000"),
            sum("tree_list", "store_503", "0.003000"),
            sum("tree_list", "request", "0.004000"),
        ]
    );
    let counts = nonzero(&text, "kbf_executor_tree_write_seconds_count", "0");
    assert_eq!(counts.len(), 7, "one write each: {counts:?}");
    assert!(counts.iter().all(|(_, count)| count == "1"), "{counts:?}");
    let step = r#"kbf_executor_step_seconds_sum{step="tree_root",form="manifests"} 0.010000"#;
    assert!(text.lines().any(|line| line == step), "{text}");
}

/// Catches a count in the wrong bucket, at a bound or past the last, or a sum that loses a
/// tree: trees of 0, 3, 5 and 6,000 manifests are one in `le="0"`, two more by `le="5"` and none
/// more until `+Inf`, with a sum of 6,008 and a count of 4.
#[test]
fn the_manifests_a_tree_wrote_land_in_their_buckets() {
    let metrics = Metrics::default();
    for written in [0, 3, 5, 6_000] {
        metrics.tree_write(InputTree::Manifests, WriteTimed::Manifests(written));
    }
    let text = rendered(&metrics);
    let bucket = |le: &str| {
        let series = format!("kbf_executor_tree_manifests_written_bucket{{le=\"{le}\"}} ");
        text.lines()
            .find_map(|line| line.strip_prefix(&series))
            .unwrap_or_else(|| panic!("no bucket {le} in {text}"))
            .to_owned()
    };
    let got: Vec<String> = ["0", "1", "2", "5", "5000", "+Inf"]
        .into_iter()
        .map(bucket)
        .collect();
    assert_eq!(got, ["1", "1", "1", "3", "3", "4"]);
    assert!(text.contains("\nkbf_executor_tree_manifests_written_sum 6008\n"));
    assert!(text.contains("\nkbf_executor_tree_manifests_written_count 4\n"));
}
