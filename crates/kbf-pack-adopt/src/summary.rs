//! The text a run prints: the report, as lines an operator reads.

use kbf_pack::{AdoptMode, AdoptReport};

/// The report as text, one fact to a line, ending with the problems it lists.
#[must_use]
pub fn render(report: &AdoptReport) -> String {
    let AdoptReport {
        mode,
        prefix,
        source,
        resumed_after,
        last_key,
        finished,
        objects,
        segments,
        gone,
        short,
        not_a_segment,
        read_failed,
        foreign,
        blobs,
        bytes,
        packs_new,
        packs_known,
        rows_new,
        rows_existing,
        bytes_new,
        conflicts,
        sampled,
        sample_bad,
        problems,
    } = report;
    let how = match mode {
        AdoptMode::DryRun => "dry run, nothing was recorded",
        AdoptMode::Apply => "apply",
    };
    let after = resumed_after.as_deref().unwrap_or("-");
    let last = last_key.as_deref().unwrap_or("-");
    let count = problems.len();
    let listed: String = problems
        .iter()
        .map(|problem| format!("  {}: {:?}\n", problem.key, problem.kind))
        .collect();
    format!(
        "kbf-pack-adopt: {how}\n\
         source {source}\n\
         prefix {prefix}, resumed after {after}, last key handled {last}, \
         listing finished {finished}\n\
         objects {objects}, segments {segments}\n\
         not adopted: gone {gone}, shorter than a segment {short}, not a segment \
         {not_a_segment}, unreadable {read_failed}, key in no known layout {foreign}\n\
         blobs {blobs}, bytes {bytes}\n\
         packs: new {packs_new}, already recorded {packs_known}\n\
         rows: new {rows_new} ({bytes_new} bytes, dry run only), already indexed {rows_existing}\n\
         conflicts {conflicts}\n\
         sample: {sampled} blobs read back, {sample_bad} did not match\n\
         problems listed {count}\n\
         {listed}"
    )
}
