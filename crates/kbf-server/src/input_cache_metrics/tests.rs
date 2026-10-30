//! The `kbf_node_input_cache_*` families as they are written for a scrape.

use kbf_proto::worker::{InputCacheCounts, TreeCounts};
use kbf_types::WorkerId;

use super::render;

fn node(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// Counts that are `base + 1` to `base + 6`, in the order the families are written, with
/// tree counts `base + 7` to `base + 9` when `trees`.
fn counts(base: u64, trees: bool) -> InputCacheCounts {
    InputCacheCounts {
        fills: base + 1,
        fill_bytes: base + 2,
        hits: base + 3,
        hit_bytes: base + 4,
        evictions: base + 5,
        evicted_bytes: base + 6,
        trees: trees.then_some(TreeCounts {
            builds: base + 7,
            hits: base + 8,
            evictions: base + 9,
        }),
    }
}

const NAMES: [&str; 9] = [
    "kbf_node_input_cache_fills_total",
    "kbf_node_input_cache_fill_bytes_total",
    "kbf_node_input_cache_hits_total",
    "kbf_node_input_cache_hit_bytes_total",
    "kbf_node_input_cache_evictions_total",
    "kbf_node_input_cache_evicted_bytes_total",
    "kbf_node_input_cache_tree_builds_total",
    "kbf_node_input_cache_tree_hits_total",
    "kbf_node_input_cache_tree_evictions_total",
];

/// Asserts that each family is a header (`# HELP`, `# TYPE ... counter`) followed by one
/// sample of the node, fed by its own count (the nine counts of a node are told apart by their
/// values), and that every node has a sample in every family, in the order the nodes are given.
#[test]
fn each_family_is_a_counter_sampling_its_own_count_for_every_node() {
    let mut out = String::new();
    let nodes = [(node("a"), counts(0, true)), (node("b"), counts(10, true))];
    render(&mut out, &nodes);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4 * NAMES.len(), "{out}");
    for (i, name) in NAMES.iter().enumerate() {
        let at = 4 * i;
        assert!(lines[at].starts_with(&format!("# HELP {name} ")), "{out}");
        assert_eq!(lines[at + 1], format!("# TYPE {name} counter"));
        let want = |node: &str, value: usize| format!("{name}{{node=\"{node}\"}} {value}");
        assert_eq!(lines[at + 2], want("a", i + 1));
        assert_eq!(lines[at + 3], want("b", i + 11));
    }
}

/// Asserts that a node with no tree store has samples in no tree family, and that the other
/// node's are written: a family is written for the nodes that have a value.
#[test]
fn a_node_without_a_tree_store_has_no_tree_samples() {
    let mut out = String::new();
    let nodes = [(node("a"), counts(0, false)), (node("b"), counts(10, true))];
    render(&mut out, &nodes);
    assert!(
        out.contains("kbf_node_input_cache_fills_total{node=\"a\"} 1\n"),
        "{out}"
    );
    assert!(!out.contains("tree_builds_total{node=\"a\"}"), "{out}");
    assert!(out.contains("tree_builds_total{node=\"b\"} 17\n"), "{out}");
}

/// Asserts that no node, or no tree store on any, writes nothing for the families that have no
/// sample, not even their headers: a cache that is off is not a cache that did nothing.
#[test]
fn a_family_without_a_sample_is_not_written() {
    let mut out = String::new();
    render(&mut out, &[]);
    assert_eq!(out, "");
    render(&mut out, &[(node("a"), counts(0, false))]);
    assert!(
        out.contains("kbf_node_input_cache_evicted_bytes_total{node=\"a\"} 6\n"),
        "{out}"
    );
    assert!(!out.contains("tree"), "{out}");
}

/// Asserts that a node id with a quote, a backslash or a line feed cannot end its label early.
#[test]
fn a_node_id_is_escaped_in_its_label() {
    let mut out = String::new();
    render(&mut out, &[(node("a\"b\\c\nd"), counts(0, false))]);
    assert!(
        out.contains("kbf_node_input_cache_fills_total{node=\"a\\\"b\\\\c\\nd\"} 1\n"),
        "{out}"
    );
}
