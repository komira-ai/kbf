//! The reader and the report on event logs buck2 wrote in CI (`tests/data/README.md` says
//! which job and which buck2): the M1 cell's first build (every action run on the farm),
//! its rebuild after a clean (every action a remote cache hit) and its failing build
//! (one action failing on the farm, one cache hit), and the M2 cell's first build
//! (container driver; `//:hog` queued again after its memory kill).
//!
//! The expected numbers were read from the logs independently of this crate (each
//! action's `execution_kind`, span duration and command metadata), and every count is
//! also checked against the summary buck2 printed on its console for the same build
//! (`Commands: N (cached: C, remote: R, local: L)` and `Cache hits: P%`).

use std::path::{Path, PathBuf};

use kbf_baseline::read::{self, Build, Ran};
use kbf_baseline::report::{Report, Timing};

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name)
}

fn build(name: &str) -> Build {
    read::read_path(&data(&format!("{name}.json-lines.gz"))).expect("read the capture")
}

fn report(names: &[&str]) -> Report {
    let builds: Vec<_> = names.iter().map(|n| ((*n).to_owned(), build(n))).collect();
    Report::of(&builds)
}

/// buck2's own console summary of a build: (commands, cached, remote, local, hit %).
fn console(name: &str) -> (usize, usize, usize, usize, usize) {
    let text = std::fs::read_to_string(data(&format!("{name}.console.log"))).expect("read");
    let after = |key: &str| -> &str {
        let at = text
            .find(key)
            .unwrap_or_else(|| panic!("{name}: no {key:?}"));
        &text[at + key.len()..]
    };
    let num = |s: &str| -> usize {
        let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().expect("a number")
    };
    (
        num(after("Commands: ")),
        num(after("cached: ")),
        num(after("remote: ")),
        num(after("local: ")),
        num(after("Cache hits: ")),
    )
}

const BUCK2: &str = "2026-09-30-2484d70a6f8a5b1f26cbdfdbd7758c9c5b746f49";

#[test]
fn every_count_matches_the_console_summary_buck2_printed() {
    // Catches: an action counted under the wrong kind (a cache hit as remote, or the
    // reverse), an action dropped or counted twice, or a share that is not hits over
    // commands, on any of the four logs.
    for name in ["m1-first", "m1-second", "m1-failing", "m2-first"] {
        let r = report(&[name]);
        let s = &r.builds[0].summary;
        let (commands, cached, remote, local, hits) = console(name);
        assert_eq!(s.ran.commands(), commands, "{name}: commands");
        assert_eq!(
            s.ran.cache_hit + s.ran.local_cache,
            cached,
            "{name}: cached"
        );
        assert_eq!(s.ran.remote, remote, "{name}: remote");
        assert_eq!(s.ran.local, local, "{name}: local");
        let share = s.cache_hit_share.expect("commands ran");
        assert_eq!(
            (share * 100.0).round() as usize,
            hits,
            "{name}: cache hits %"
        );
    }
}

#[test]
fn the_first_build_ran_every_action_on_the_farm() {
    // Catches: remote actions taken for cache hits, the farm's queue or execution time
    // read from the wrong field or dropped, the build's id, revision, result or wall
    // time misread.
    let b = build("m1-first");
    assert_eq!(b.build_id, "f94cb4eb-84b2-48ab-9081-9cb9d75987bc");
    assert_eq!(b.buck2_revision.as_deref(), Some(BUCK2));
    assert_eq!(b.success, Some(true));
    assert_eq!(b.wall_us, Some(1_012_122));
    let mut rows: Vec<_> = b
        .actions
        .iter()
        .map(|a| {
            (
                a.target.as_str(),
                a.ran,
                a.action_us,
                a.queue_us,
                a.execution_us,
            )
        })
        .collect();
    rows.sort_unstable();
    assert_eq!(
        rows,
        [
            (
                "root//:digest",
                Ran::Remote,
                356_326,
                Some(1487),
                Some(3453)
            ),
            ("root//:pause", Ran::Remote, 422_953, Some(1711), Some(3457)),
            ("root//:shout", Ran::Remote, 163_699, Some(1509), Some(2817)),
            (
                "root//:verdict",
                Ran::Remote,
                169_540,
                Some(1493),
                Some(3529)
            ),
            ("root//:words", Ran::Remote, 427_988, Some(1345), Some(3450)),
        ]
    );
    assert!(
        b.actions
            .iter()
            .all(|a| !a.failed && a.status.as_deref() == Some("Success"))
    );
}

#[test]
fn a_cache_hit_counts_as_a_hit_and_carries_no_farm_time() {
    // Catches: a cache hit counted as remote, and a hit's queue or execution time (the
    // metadata of the run that filled the cache, still in the log) counted as work done
    // in this build.
    let r = report(&["m1-second"]);
    let s = &r.builds[0].summary;
    assert_eq!((s.ran.remote, s.ran.cache_hit, s.actions), (0, 5, 5));
    assert_eq!(s.cache_hit_share, Some(1.0));
    assert_eq!(s.queue_us, Timing::default());
    assert_eq!(s.execution_us, Timing::default());
    assert_eq!(r.builds[0].success, Some(true));
}

#[test]
fn a_failed_action_is_counted_and_listed_with_its_exit_code() {
    // Catches: a failed action dropped (from the counts, the timings or the failure
    // list), its exit code or status lost, or the failed build reported as a success.
    let r = report(&["m1-failing"]);
    let b = &r.builds[0];
    assert_eq!(b.success, Some(false));
    assert_eq!(b.wall_us, Some(390_620));
    let s = &b.summary;
    assert_eq!((s.ran.remote, s.ran.cache_hit, s.actions), (1, 1, 2));
    assert_eq!(s.cache_hit_share, Some(0.5));
    assert_eq!((s.queue_us.count, s.queue_us.max), (1, Some(1354)));
    assert_eq!(s.failures.len(), 1);
    let f = &s.failures[0];
    assert_eq!(
        (
            f.target.as_str(),
            f.category.as_str(),
            f.ran,
            f.exit_code,
            f.status.as_deref()
        ),
        (
            "root//:verdict",
            "step",
            Ran::Remote,
            Some(3),
            Some("Failure")
        )
    );
    assert_eq!(f.build_id, "9fb3b55a-9715-46b4-b386-4ad8661d7d63");
    assert_eq!((r.total.builds, r.total.failed_builds), (1, 1));
}

#[test]
fn the_set_pools_every_action_before_taking_percentiles() {
    // Catches: a wrong percentile rank (these six queue times give 1487 at the 50th by
    // nearest rank; an index of p*n/100 gives 1493), percentiles averaged over builds
    // instead of pooled, a failed action's times dropped, and wall times not summed.
    let r = report(&["m1-first", "m1-second", "m1-failing"]);
    let t = &r.total;
    assert_eq!((t.builds, t.failed_builds), (3, 1));
    assert_eq!(t.wall_us, 1_012_122 + 275_270 + 390_620);
    let s = &t.summary;
    assert_eq!((s.actions, s.ran.remote, s.ran.cache_hit), (12, 6, 6));
    assert_eq!(s.cache_hit_share, Some(0.5));
    // Queue: 1345 1354 1487 1493 1509 1711.
    assert_eq!(
        s.queue_us,
        Timing {
            count: 6,
            p50: Some(1487),
            p90: Some(1711),
            p99: Some(1711),
            max: Some(1711)
        }
    );
    // Execution: 2817 3440 3450 3453 3457 3529.
    assert_eq!(
        s.execution_us,
        Timing {
            count: 6,
            p50: Some(3450),
            p90: Some(3529),
            p99: Some(3529),
            max: Some(3529)
        }
    );
    assert_eq!((s.action_us.count, s.action_us.max), (12, Some(427_988)));
    assert_eq!(s.failures.len(), 1);
}

#[test]
fn a_requeued_action_shows_its_whole_wait_as_queue() {
    // Catches: the queue time read from buck2's own stream counter (`queue_time`, tens of
    // microseconds here) rather than the farm's metadata: `//:hog`, killed at its memory
    // cap and run again, waited about 21 s by the farm's account.
    let b = build("m2-first");
    let hog = b
        .actions
        .iter()
        .find(|a| a.target == "root//:hog")
        .expect("hog");
    assert_eq!((hog.ran, hog.queue_us), (Ran::Remote, Some(20_813_073)));
    assert_eq!(b.wall_us, Some(41_641_620));
    let r = report(&["m2-first"]);
    assert_eq!(r.builds[0].summary.queue_us.max, Some(20_813_073));
}

#[test]
fn a_log_cut_before_its_end_has_no_result() {
    // Catches: a build whose log ends early (buck2 killed) reported as a success or a
    // failure, or with a wall time, instead of neither; and its finished actions lost.
    let mut text = String::new();
    let mut gz = flate2::read::GzDecoder::new(
        std::fs::File::open(data("m1-first.json-lines.gz")).expect("open"),
    );
    std::io::Read::read_to_string(&mut gz, &mut text).expect("gunzip");
    let cut = text
        .find(r#""data":{"Command":{"is_success""#)
        .expect("the command's end");
    let cut = text[..cut].rfind('\n').expect("a line before it");
    let b = read::parse(&mut &text.as_bytes()[..=cut]).expect("parse the cut log");
    assert_eq!((b.success, b.wall_us, b.actions.len()), (None, None, 5));
}
