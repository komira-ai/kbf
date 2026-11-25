use std::time::Duration;

use super::*;
use crate::executor::config::InputTree;
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

/// The `form` label values a step other than `facts` is counted under.
const FORMS: [(InputTree, &str); 2] = [
    (InputTree::Manifests, "manifests"),
    (InputTree::Provider, "provider"),
];

/// Catches a family that lists a step twice or not at all, or loses its type line: every step
/// but `facts` has one count, one sum and one `+Inf` bucket in each form, `facts` has them once
/// with the form `none`, and the family is declared a histogram once.
#[test]
fn the_step_family_renders_every_step_label_once_per_form() {
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
            "tree_blobs",
            "tree_manifests",
            "tree_root",
            "tree_list",
            "tree_sidecar",
            "row",
            "rpc",
            "backoff"
        ]
    );
    let mut series = vec![r#"step="facts",form="none""#.to_owned()];
    for label in &labels[1..] {
        for (_, form) in FORMS {
            series.push(format!(r#"step="{label}",form="{form}""#));
        }
    }
    let counted: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix(&format!("{family}_count{{")))
        .filter_map(|line| line.strip_suffix("} 0"))
        .collect();
    assert_eq!(counted, series, "each series once, in order, at zero");
    for labels in &series {
        assert_eq!(
            value(&text, &format!("{family}_bucket{{{labels},le=\"+Inf\"}}")),
            "0"
        );
    }
}

/// Catches a step in another step's or another form's series, or a millisecond step lost in a
/// coarse bucket: a 1 ms `rpc` of a tree list is in the `0.001` bucket, a 40 ms one in `0.05`
/// and not `0.025`, and no other series is counted, the same step of manifests included.
#[test]
fn a_step_lands_in_its_own_series_and_form_down_to_a_millisecond() {
    let metrics = Metrics::default();
    metrics.step_took(Step::Rpc, InputTree::Provider, Duration::from_millis(1));
    metrics.step_took(Step::Rpc, InputTree::Provider, Duration::from_millis(40));
    let text = rendered(&metrics);
    let rpc = r#"step="rpc",form="provider""#;
    let bucket = |le: &str| {
        value(
            &text,
            &format!("kbf_executor_step_seconds_bucket{{{rpc},le=\"{le}\"}}"),
        )
        .to_owned()
    };
    assert_eq!(bucket("0.001"), "1");
    assert_eq!(bucket("0.025"), "1");
    assert_eq!(bucket("0.05"), "2");
    assert_eq!(
        value(&text, &format!("kbf_executor_step_seconds_sum{{{rpc}}}")),
        "0.041000"
    );
    let counts: Vec<(String, String)> = text
        .lines()
        .filter_map(|line| line.strip_prefix("kbf_executor_step_seconds_count{"))
        .filter_map(|line| line.split_once("} "))
        .filter(|(_, count)| *count != "0")
        .map(|(labels, count)| (labels.to_owned(), count.to_owned()))
        .collect();
    assert_eq!(counts, [(rpc.to_owned(), "2".to_owned())]);
}

/// Catches `facts` counted under a form it has none of, or apart from its one series: it is
/// taken before the form is chosen, and whatever form a caller names it lands in `form="none"`.
#[test]
fn facts_has_no_form() {
    let metrics = Metrics::default();
    metrics.facts_took(Duration::from_millis(2));
    metrics.step_took(Step::Facts, InputTree::Provider, Duration::from_millis(3));
    let text = rendered(&metrics);
    assert_eq!(
        value(
            &text,
            r#"kbf_executor_step_seconds_count{step="facts",form="none"}"#
        ),
        "2"
    );
    assert!(!text.contains(r#"step="facts",form="provider""#), "{text}");
}

/// Catches a tree list's stages timed as another step: the list's write is `tree_list`, its
/// sidecar `tree_sidecar`, and its blobs `tree_blobs` as for manifests.
#[test]
fn the_stages_of_a_tree_list_are_steps_of_their_own() {
    assert_eq!(Step::of_write(WriteStage::List), Step::TreeList);
    assert_eq!(Step::of_write(WriteStage::Sidecar), Step::TreeSidecar);
    assert_eq!(Step::of_write(WriteStage::Blobs), Step::TreeBlobs);
}
