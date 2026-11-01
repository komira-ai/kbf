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
//!
//! The daemons that write packs (`--pack-write`) say what their writes did in the same way
//! ([`crate::farm::Farm::pack_writes`]): `kbf_pack_daemon_packs_written_total`,
//! `kbf_pack_daemon_blobs_packed_total`, `kbf_pack_daemon_packed_bytes_total`,
//! `kbf_pack_daemon_missing_indexed_total`, `kbf_pack_daemon_missing_unasked_total` and
//! `kbf_pack_daemon_write_fallbacks_total`, which has a `reason` label as well as `node`: one of
//! [`REASONS`], a fixed set. A node that sent no write counts has no sample of these, whether or
//! not it sent read counts.

use std::fmt::Write as _;

use kbf_proto::worker::{PackReadCounts, PackWriteCounts};
use kbf_types::WorkerId;

use crate::farm::Farm;

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
pub(crate) fn label_value(value: &str) -> String {
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

/// One write family: its name, its `# HELP` text, and the count of a node it samples.
type WriteFamily = (&'static str, &'static str, fn(&PackWriteCounts) -> u64);

/// The write families without a label but `node`, in the order they are written.
const WRITE_FAMILIES: [WriteFamily; 5] = [
    (
        "kbf_pack_daemon_packs_written_total",
        "Packs a daemon stored and had the frontend commit, per node, since its process started",
        |counts| counts.packs,
    ),
    (
        "kbf_pack_daemon_blobs_packed_total",
        "Blobs in the packs a daemon committed, per node, since its process started",
        |counts| counts.packed,
    ),
    (
        "kbf_pack_daemon_packed_bytes_total",
        "Bytes of the packs a daemon committed, per node, since its process started",
        |counts| counts.packed_bytes,
    ),
    (
        "kbf_pack_daemon_missing_indexed_total",
        "Digests of a find-missing question a daemon had the pack index answer, per node, since \
         its process started",
        |counts| counts.missing_indexed,
    ),
    (
        "kbf_pack_daemon_missing_unasked_total",
        "Digests of a find-missing question a daemon sent to the storage nodes whole because the \
         pack index could not be asked, per node, since its process started",
        |counts| counts.missing_unasked,
    ),
];

/// The name and help of the family of blobs a daemon wrote to the storage nodes instead of into
/// a pack, which has one sample per node and reason.
const FALLBACKS: (&str, &str) = (
    "kbf_pack_daemon_write_fallbacks_total",
    "Blobs a daemon wrote to the storage nodes instead of into a pack, per node and reason, \
     since its process started",
);

/// One fallback reason: its value as the `reason` label, and the count of a node it samples.
type Reason = (&'static str, fn(&PackWriteCounts) -> u64);

/// The values of the `reason` label of [`FALLBACKS`], in the order they are written. A fixed
/// set: a reason is never made from a runtime value.
const REASONS: [Reason; 7] = [
    ("off", |counts| counts.fallback_off),
    ("unfit", |counts| counts.fallback_unfit),
    ("unsupported", |counts| counts.fallback_unsupported),
    ("refused", |counts| counts.fallback_refused),
    ("store", |counts| counts.fallback_store),
    ("record", |counts| counts.fallback_record),
    ("index", |counts| counts.fallback_index),
];

/// Appends the write families for `nodes`, each node's newest counts, to `out`: nothing when
/// `nodes` is empty.
pub(crate) fn render_writes(out: &mut String, nodes: &[(WorkerId, PackWriteCounts)]) {
    if nodes.is_empty() {
        return;
    }
    for (name, help, count) in WRITE_FAMILIES {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
        for (node, counts) in nodes {
            let node = label_value(node.as_str());
            let _ = writeln!(out, "{name}{{node=\"{node}\"}} {}", count(counts));
        }
    }
    let (name, help) = FALLBACKS;
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
    for (node, counts) in nodes {
        let node = label_value(node.as_str());
        for (reason, count) in REASONS {
            let value = count(counts);
            let _ = writeln!(out, "{name}{{node=\"{node}\",reason=\"{reason}\"}} {value}");
        }
    }
}

/// Appends what the daemons of `farm` report of their pack reads and writes to `out`.
pub(crate) fn render_farm(out: &mut String, farm: &Farm) {
    render(out, &farm.pack_reads());
    render_writes(out, &farm.pack_writes());
    render_store_writes(out, farm);
}

/// Appends `kbf_farm_lost_races_total` and `kbf_farm_dropped_writes_total` to `out`.
fn render_store_writes(out: &mut String, farm: &Farm) {
    let _ = writeln!(
        out,
        "# HELP kbf_farm_lost_races_total Writes to the state store refused because another \
         frontend took the same operation first (the designed end of a race; the operation \
         runs once), since the process started\n# TYPE kbf_farm_lost_races_total counter\n\
         kbf_farm_lost_races_total {}",
        farm.lost_races()
    );
    let _ = writeln!(
        out,
        "# HELP kbf_farm_dropped_writes_total Writes to the state store given up on for \
         good (fenced by a newer writer, for a node another frontend owns, not encodable, or \
         over the held cap), since the process started\n# TYPE kbf_farm_dropped_writes_total \
         counter\nkbf_farm_dropped_writes_total {}",
        farm.dropped_writes()
    );
}

#[cfg(test)]
mod tests;
