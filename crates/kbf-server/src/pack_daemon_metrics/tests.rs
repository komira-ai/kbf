//! The `kbf_pack_daemon_*` families as they are written for a scrape.

use kbf_proto::worker::PackReadCounts;
use kbf_types::WorkerId;

use super::{label_value, render};

/// The families in the order they are written, each with the count of
/// [`counts`] it samples.
const NAMES: [&str; 6] = [
    "kbf_pack_daemon_served_total",
    "kbf_pack_daemon_served_bytes_total",
    "kbf_pack_daemon_elsewhere_total",
    "kbf_pack_daemon_unasked_total",
    "kbf_pack_daemon_lost_total",
    "kbf_pack_daemon_damaged_total",
];

/// Counts that are `base + 1` to `base + 6`, in the order of [`NAMES`].
fn counts(base: u64) -> PackReadCounts {
    PackReadCounts {
        served: base + 1,
        served_bytes: base + 2,
        elsewhere: base + 3,
        unasked: base + 4,
        lost: base + 5,
        damaged: base + 6,
    }
}

fn node(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// Asserts that each family is a header (`# HELP`, `# TYPE ... counter`) followed by one sample
/// of the node, and that each is fed by its own count: the six counts of a node are told apart
/// by their values.
#[test]
fn each_family_is_a_counter_sampling_its_own_count() {
    let mut out = String::new();
    render(&mut out, &[(node("node-1"), counts(0))]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 18, "{out}");
    for (i, name) in NAMES.iter().enumerate() {
        let help = format!("# HELP {name} ");
        assert!(lines[3 * i].starts_with(&help), "{out}");
        assert_eq!(lines[3 * i + 1], format!("# TYPE {name} counter"));
        let sample = format!("{name}{{node=\"node-1\"}} {}", i + 1);
        assert_eq!(lines[3 * i + 2], sample);
    }
}

/// Asserts that every node has a sample in every family, in the order the nodes are given, and
/// that a node's values are its own.
#[test]
fn every_node_has_a_sample_in_every_family() {
    let mut out = String::new();
    let nodes = [(node("a"), counts(0)), (node("b"), counts(10))];
    render(&mut out, &nodes);
    let samples: Vec<&str> = out.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(samples.len(), 12, "{out}");
    assert_eq!(samples[0], "kbf_pack_daemon_served_total{node=\"a\"} 1");
    assert_eq!(samples[1], "kbf_pack_daemon_served_total{node=\"b\"} 11");
    assert_eq!(samples[10], "kbf_pack_daemon_damaged_total{node=\"a\"} 6");
    assert_eq!(samples[11], "kbf_pack_daemon_damaged_total{node=\"b\"} 16");
}

/// Asserts that with no node having sent counts nothing is written, not even a header, and what
/// the text already held is kept.
#[test]
fn no_node_no_family() {
    let mut out = "kept\n".to_owned();
    render(&mut out, &[]);
    assert_eq!(out, "kept\n");
}

/// Asserts that a node id is written as a label value of the text format: a backslash, a double
/// quote and a line feed are escaped, and nothing else is changed.
#[test]
fn a_node_id_is_escaped_as_a_label_value() {
    assert_eq!(label_value("node-1.example"), "node-1.example");
    assert_eq!(label_value("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
}
