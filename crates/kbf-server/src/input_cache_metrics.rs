//! `kbf_node_input_cache_*` on `GET /metrics`: what each daemon says its input cache did, from
//! the counts on its heartbeats ([`crate::farm::Farm::input_caches`]).
//!
//! One sample per node and family, labelled `node`, as the counters of the daemon's process
//! (they start again from 0 when it restarts, as a counter may; nothing here keeps them across
//! a restart). A node that sent no counts has no sample, and with no node sending any, no family
//! is written: a daemon whose cache is off is not a daemon whose cache did nothing.
//!
//! The unit is the cache entry (one blob by digest and executable bit), not the file name a
//! lease places it under. The tree store's counts are families of their own, written only for
//! the nodes that have a store.

use std::fmt::Write as _;

use kbf_proto::worker::InputCacheCounts;
use kbf_types::WorkerId;

use crate::farm::Farm;
use crate::pack_daemon_metrics::label_value;

/// One family: its name, its `# HELP` text, and the count of a node it samples, if the node has
/// one.
type Family = (
    &'static str,
    &'static str,
    fn(&InputCacheCounts) -> Option<u64>,
);

/// The families, in the order they are written.
const FAMILIES: [Family; 9] = [
    (
        "kbf_node_input_cache_fills_total",
        "Entries a daemon wrote into its input cache from the content store, per node, since its \
         process started",
        |counts| Some(counts.fills),
    ),
    (
        "kbf_node_input_cache_fill_bytes_total",
        "Bytes of the entries a daemon wrote into its input cache from the content store, per \
         node, since its process started",
        |counts| Some(counts.fill_bytes),
    ),
    (
        "kbf_node_input_cache_hits_total",
        "Entries a lease found in the daemon's input cache and used, per node, since its process \
         started",
        |counts| Some(counts.hits),
    ),
    (
        "kbf_node_input_cache_hit_bytes_total",
        "Bytes of the entries a lease found in the daemon's input cache and used, per node, \
         since its process started",
        |counts| Some(counts.hit_bytes),
    ),
    (
        "kbf_node_input_cache_evictions_total",
        "Entries a daemon evicted from its input cache for its byte budget or free-space floor, \
         per node, since its process started",
        |counts| Some(counts.evictions),
    ),
    (
        "kbf_node_input_cache_evicted_bytes_total",
        "Bytes of the entries a daemon evicted from its input cache, per node, since its process \
         started",
        |counts| Some(counts.evicted_bytes),
    ),
    (
        "kbf_node_input_cache_tree_builds_total",
        "Trees a daemon built in its shared tree store, per node, since its process started",
        |counts| counts.trees.as_ref().map(|trees| trees.builds),
    ),
    (
        "kbf_node_input_cache_tree_hits_total",
        "Acquires that found the tree built in a daemon's shared tree store, per node, since its \
         process started",
        |counts| counts.trees.as_ref().map(|trees| trees.hits),
    ),
    (
        "kbf_node_input_cache_tree_evictions_total",
        "Trees a daemon evicted from its shared tree store, per node, since its process started",
        |counts| counts.trees.as_ref().map(|trees| trees.evictions),
    ),
];

/// Appends the families for `nodes`, each node's newest counts, to `out`. A family with no
/// sample is not written.
pub(crate) fn render(out: &mut String, nodes: &[(WorkerId, InputCacheCounts)]) {
    for (name, help, count) in FAMILIES {
        let mut samples = nodes
            .iter()
            .filter_map(|(node, counts)| count(counts).map(|value| (node, value)))
            .peekable();
        if samples.peek().is_none() {
            continue;
        }
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
        for (node, value) in samples {
            let node = label_value(node.as_str());
            let _ = writeln!(out, "{name}{{node=\"{node}\"}} {value}");
        }
    }
}

/// Appends what the daemons of `farm` report of their input caches to `out`.
pub(crate) fn render_farm(out: &mut String, farm: &Farm) {
    render(out, &farm.input_caches());
}

#[cfg(test)]
mod tests;
