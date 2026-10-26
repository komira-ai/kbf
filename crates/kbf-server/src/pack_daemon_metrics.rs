//! `kbf_pack_daemon_*` on `GET /metrics`: what each daemon that reads packs (`--pack-read`)
//! says its reads did, from the counts on its heartbeats ([`crate::farm::Farm::pack_reads`]).
//!
//! One sample per node and family, labelled `node`, as the counters of the daemon's process
//! (they start again from 0 when it restarts, as a counter may). A node that sent no counts
//! has no sample, and with no node sending any, no family is written: a daemon that does not
//! read packs is not a daemon that read none.
//!
//! The gate of a farm's move to packs is the sum of `kbf_pack_daemon_elsewhere_total` and the
//! frontend's own `kbf_pack_read_fallbacks_total` no longer rising (reads that still go to the
//! storage nodes, apart from blobs over 64 MiB and digests that are not SHA-256, which always
//! count in `elsewhere`), with `kbf_pack_daemon_unasked_total` flat and
//! `kbf_pack_daemon_lost_total` and `kbf_pack_daemon_damaged_total` staying 0 (`docs/api.md`,
//! "Packs").

use std::fmt::Write as _;

use kbf_proto::worker::PackReadCounts;
use kbf_types::WorkerId;

/// One family: its name, its `# HELP` text, and the count of a node it samples.
type Family = (&'static str, &'static str, fn(&PackReadCounts) -> u64);

/// The families, in the order they are written.
const FAMILIES: [Family; 6] = [
    (
        "kbf_pack_daemon_served_total",
        "Blobs a daemon read from a pack, per node, since its process started",
        |counts| counts.served,
    ),
    (
        "kbf_pack_daemon_served_bytes_total",
        "Bytes of the blobs a daemon read from a pack, per node, since its process started",
        |counts| counts.served_bytes,
    ),
    (
        "kbf_pack_daemon_elsewhere_total",
        "Reads a daemon sent to the storage nodes because no pack holds the blob, per node, \
         since its process started",
        |counts| counts.elsewhere,
    ),
    (
        "kbf_pack_daemon_unasked_total",
        "Reads a daemon sent to the storage nodes because the packs could not be asked, per \
         node, since its process started",
        |counts| counts.unasked,
    ),
    (
        "kbf_pack_daemon_lost_total",
        "Reads that failed because the pack the index names is gone or too short, per node, \
         since the daemon's process started (alert on any)",
        |counts| counts.lost,
    ),
    (
        "kbf_pack_daemon_damaged_total",
        "Reads that failed because the bytes in the pack are not the digest, per node, since \
         the daemon's process started (alert on any)",
        |counts| counts.damaged,
    ),
];

/// `value` as a label value of the text format: backslash, double quote and line feed escaped.
fn label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Appends the families for `nodes`, each node's newest counts, to `out`: nothing when `nodes`
/// is empty.
pub(crate) fn render(out: &mut String, nodes: &[(WorkerId, PackReadCounts)]) {
    if nodes.is_empty() {
        return;
    }
    for (name, help, count) in FAMILIES {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
        for (node, counts) in nodes {
            let node = label_value(node.as_str());
            let _ = writeln!(out, "{name}{{node=\"{node}\"}} {}", count(counts));
        }
    }
}

#[cfg(test)]
mod tests;
