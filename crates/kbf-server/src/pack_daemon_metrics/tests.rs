//! The `kbf_pack_daemon_*` families as they are written for a scrape.

use kbf_proto::worker::{PackReadCounts, PackWriteCounts};
use kbf_types::WorkerId;

use super::{label_value, render, render_writes};

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

/// The write families without a reason, in the order they are written.
const WRITE_NAMES: [&str; 5] = [
    "kbf_pack_daemon_packs_written_total",
    "kbf_pack_daemon_blobs_packed_total",
    "kbf_pack_daemon_packed_bytes_total",
    "kbf_pack_daemon_missing_indexed_total",
    "kbf_pack_daemon_missing_unasked_total",
];

/// The reasons of the fallback family, in the order they are written.
const REASON_NAMES: [&str; 7] = [
    "off",
    "unfit",
    "unsupported",
    "refused",
    "store",
    "record",
    "index",
];

/// Write counts that are `base + 1` to `base + 12` in the order of the proto's fields: packs,
/// packed, packed_bytes, the seven fallbacks, missing_indexed, missing_unasked.
fn writes(base: u64) -> PackWriteCounts {
    PackWriteCounts {
        packs: base + 1,
        packed: base + 2,
        packed_bytes: base + 3,
        fallback_off: base + 4,
        fallback_unfit: base + 5,
        fallback_unsupported: base + 6,
        fallback_refused: base + 7,
        fallback_store: base + 8,
        fallback_record: base + 9,
        fallback_index: base + 10,
        missing_indexed: base + 11,
        missing_unasked: base + 12,
    }
}

/// Asserts that each write family is a header followed by one sample of the node fed by its own
/// count, and that the fallback family has one sample per reason, in the fixed order of reasons,
/// each fed by its own count: the twelve counts of a node are told apart by their values.
#[test]
fn each_write_family_samples_its_own_count() {
    let mut out = String::new();
    render_writes(&mut out, &[(node("node-1"), writes(0))]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3 * 5 + 2 + 7, "{out}");
    let values = [1, 2, 3, 11, 12];
    for (i, name) in WRITE_NAMES.iter().enumerate() {
        let help = format!("# HELP {name} ");
        assert!(lines[3 * i].starts_with(&help), "{out}");
        assert_eq!(lines[3 * i + 1], format!("# TYPE {name} counter"));
        let sample = format!("{name}{{node=\"node-1\"}} {}", values[i]);
        assert_eq!(lines[3 * i + 2], sample);
    }
    let name = "kbf_pack_daemon_write_fallbacks_total";
    assert!(lines[15].starts_with(&format!("# HELP {name} ")), "{out}");
    assert_eq!(lines[16], format!("# TYPE {name} counter"));
    for (i, reason) in REASON_NAMES.iter().enumerate() {
        let sample = format!("{name}{{node=\"node-1\",reason=\"{reason}\"}} {}", i + 4);
        assert_eq!(lines[17 + i], sample);
    }
}

/// Asserts that every node has a sample in every write family, in the order the nodes are given,
/// and that a node's values are its own.
#[test]
fn every_node_has_a_sample_in_every_write_family() {
    let mut out = String::new();
    let nodes = [(node("a"), writes(0)), (node("b"), writes(100))];
    render_writes(&mut out, &nodes);
    let samples: Vec<&str> = out.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(samples.len(), 2 * 5 + 2 * 7, "{out}");
    assert_eq!(
        samples[0],
        "kbf_pack_daemon_packs_written_total{node=\"a\"} 1"
    );
    assert_eq!(
        samples[1],
        "kbf_pack_daemon_packs_written_total{node=\"b\"} 101"
    );
    let fallback = "kbf_pack_daemon_write_fallbacks_total";
    assert_eq!(
        samples[10],
        format!("{fallback}{{node=\"a\",reason=\"off\"}} 4")
    );
    assert_eq!(
        samples[23],
        format!("{fallback}{{node=\"b\",reason=\"index\"}} 110")
    );
}

/// Asserts that with no node having sent write counts nothing is written, not even a header, and
/// what the text already held is kept.
#[test]
fn no_node_no_write_family() {
    let mut out = "kept\n".to_owned();
    render_writes(&mut out, &[]);
    assert_eq!(out, "kept\n");
}
