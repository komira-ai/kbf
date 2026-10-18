//! The gates over planted evidence. Every test starts from a passing run (`pass()`),
//! changes one thing, and names the defect that a gate which let it through would hide.

use std::collections::BTreeMap;

use super::*;
use crate::scrape::Scrapes;

fn sample(at: f64, execute_errors: f64, lost: f64, losses: f64) -> Sample {
    let series = BTreeMap::from([
        (EXECUTE_ERRORS.to_owned(), execute_errors),
        (LOST_BLOBS.to_owned(), lost),
        (BLOB_LOSSES.to_owned(), losses),
    ]);
    Sample { at, series }
}

fn scrapes(samples: Vec<Sample>) -> Scrapes {
    Scrapes {
        samples,
        failed: 0,
        last_error: None,
    }
}

fn build(ok: bool, failed: Result<u64, String>) -> BuildResult {
    BuildResult {
        kind: "compile".to_owned(),
        ok,
        failed_actions: failed,
    }
}

/// What a healthy ten-second run measured: two builds, eleven scrapes, nothing lost.
struct Run {
    builds: Vec<BuildResult>,
    scrapes: Scrapes,
    exited: Vec<(String, String)>,
    stopped: Vec<(String, String)>,
    hung: bool,
}

fn pass() -> Run {
    Run {
        builds: vec![build(true, Ok(0)), build(true, Ok(0))],
        scrapes: scrapes(
            (0..11)
                .map(|i| sample(f64::from(i), 0.0, 0.0, 0.0))
                .collect(),
        ),
        exited: Vec::new(),
        stopped: vec![("frontend".to_owned(), "exit 0".to_owned())],
        hung: false,
    }
}

fn gates(run: &Run) -> Vec<Gate> {
    Evidence {
        builds: &run.builds,
        scrapes: &run.scrapes,
        exited_during_load: &run.exited,
        stopped: &run.stopped,
        hung: run.hung,
        load: (
            "2026-10-18T00:00:00Z".to_owned(),
            "2026-10-18T00:00:10Z".to_owned(),
        ),
        load_secs: 10.0,
        stop: (
            "2026-10-18T00:00:10Z".to_owned(),
            "2026-10-18T00:00:12Z".to_owned(),
        ),
        scrape_every_secs: 1.0,
    }
    .gates()
}

/// The verdict of the gate `name`.
fn verdict(gates: &[Gate], name: &str) -> Verdict {
    gates
        .iter()
        .find(|g| g.name == name)
        .unwrap_or_else(|| panic!("no gate {name}"))
        .verdict
}

/// The names of the failed gates.
fn failed(run: &Run) -> Vec<String> {
    gates(run)
        .into_iter()
        .filter(|g| g.verdict == Verdict::Fail)
        .map(|g| g.name)
        .collect()
}

#[test]
fn a_healthy_run_passes_every_gate() {
    // Catches: a control that is red already, which would make every planted defect
    // below go red for the wrong reason.
    let run = pass();
    assert_eq!(failed(&run), Vec::<String>::new());
    assert!(all_pass(&gates(&run)));
    assert_eq!(gates(&run).len(), 10);
}

#[test]
fn the_gates_carry_every_field_of_the_document() {
    // Catches: a renamed or missing field (the offer to gamma reads `name`, `measured`,
    // `threshold`, `verdict`, `started` and `finished`).
    let first = serde_json::to_value(&gates(&pass())[0]).expect("json");
    let mut keys: Vec<&str> = first
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "finished",
            "measured",
            "name",
            "started",
            "threshold",
            "verdict"
        ]
    );
    assert_eq!(first["verdict"], "PASS");
}

#[test]
fn zero_builds_is_red() {
    // Catches: a load loop that never ran a build passing every gate on its empty list.
    let mut run = pass();
    run.builds.clear();
    assert!(failed(&run).contains(&"builds_run".to_owned()));
}

#[test]
fn a_failed_build_is_red() {
    // Catches: builds_failed ignoring a nonzero exit or a timeout.
    let mut run = pass();
    run.builds.push(build(false, Ok(0)));
    assert_eq!(failed(&run), ["builds_failed"]);
}

#[test]
fn a_failed_action_is_red() {
    // Catches: a build that exits 0 with a failed action in its event log (an action
    // Buck2 retried or tolerated) not being counted.
    let mut run = pass();
    run.builds.push(build(true, Ok(2)));
    assert_eq!(failed(&run), ["actions_failed"]);
}

#[test]
fn an_unreadable_event_log_is_a_failure() {
    // Catches: a log that cannot be parsed counting as zero failed actions (fail open).
    let mut run = pass();
    run.builds.push(build(true, Err("not gzip".to_owned())));
    assert_eq!(failed(&run), ["actions_failed"]);
}

#[test]
fn a_lost_blob_in_any_scrape_is_red_even_if_the_last_is_zero() {
    // Catches: the gauge read in the last scrape alone, so a loss that fell back to zero
    // (the blob uploaded again) hid the loss; and the counter, which never falls, being
    // the only evidence.
    let mut run = pass();
    run.scrapes.samples[5] = sample(5.0, 0.0, 1.0, 0.0);
    assert_eq!(failed(&run), ["lost_blobs_gauge"]);
}

#[test]
fn the_loss_counter_is_a_delta() {
    // Catches: the counter compared with zero instead of with its first reading (a farm
    // that restarted over a store that already lost blobs is not this run's loss), and
    // a loss the gauge missed.
    let mut run = pass();
    for s in &mut run.scrapes.samples {
        s.series.insert(BLOB_LOSSES.to_owned(), 7.0);
    }
    assert_eq!(failed(&run), Vec::<String>::new());
    run.scrapes.samples[10]
        .series
        .insert(BLOB_LOSSES.to_owned(), 8.0);
    assert_eq!(failed(&run), ["blob_losses"]);
}

#[test]
fn the_execute_error_counter_is_a_delta() {
    // Catches: the absolute value used (a start value of 3 read as three failures), or an
    // increase ignored.
    let mut run = pass();
    for s in &mut run.scrapes.samples {
        s.series.insert(EXECUTE_ERRORS.to_owned(), 3.0);
    }
    assert_eq!(failed(&run), Vec::<String>::new());
    run.scrapes.samples[10]
        .series
        .insert(EXECUTE_ERRORS.to_owned(), 4.0);
    assert_eq!(failed(&run), ["execute_errors"]);
}

#[test]
fn an_absent_metric_fails_its_gate_in_the_first_scrape_and_in_a_later_one() {
    // Catches: an absent series read as zero, so a metric the frontend stops exporting
    // turns its gate green.
    for scrape in [0, 7] {
        for name in [EXECUTE_ERRORS, LOST_BLOBS, BLOB_LOSSES] {
            let mut run = pass();
            run.scrapes.samples[scrape].series.remove(name);
            let failed = failed(&run);
            assert_eq!(
                failed.len(),
                1,
                "{name} absent from scrape {scrape}: {failed:?}"
            );
        }
    }
}

#[test]
fn fewer_than_two_scrapes_pass_no_series_gate() {
    // Catches: a run whose scraper never got a page passing on a difference of nothing.
    let mut run = pass();
    run.scrapes.samples.truncate(1);
    let failed = failed(&run);
    for gate in [
        "execute_errors",
        "lost_blobs_gauge",
        "blob_losses",
        "scrape_coverage",
    ] {
        assert!(failed.contains(&gate.to_owned()), "{gate} in {failed:?}");
    }
}

#[test]
fn a_family_with_labels_is_summed() {
    // Catches: a labelled series (`name{class="x"}`, which a by-class counter will be)
    // read as absent, or only its first label counted.
    let mut s = sample(0.0, 0.0, 0.0, 0.0);
    s.series.remove(EXECUTE_ERRORS);
    s.series
        .insert(format!("{EXECUTE_ERRORS}{{class=\"a\"}}"), 2.0);
    s.series
        .insert(format!("{EXECUTE_ERRORS}{{class=\"b\"}}"), 3.0);
    assert_eq!(s.value(EXECUTE_ERRORS), Some(5.0));
    assert_eq!(s.value("kbf_no_such"), None);
}

#[test]
fn a_flaky_scrape_is_a_harness_defect() {
    // Catches: scrape_coverage not noticing a scraper that read a page in only half of
    // the seconds, which would leave a loss between two scrapes unseen.
    let mut run = pass();
    run.scrapes.samples.truncate(5);
    assert_eq!(failed(&run), ["scrape_coverage"]);
}

#[test]
fn a_process_that_exited_during_the_load_is_red() {
    // Catches: a crash of a role the load survived (a retry hid it) passing.
    let mut run = pass();
    run.exited
        .push(("node-2".to_owned(), "signal 9".to_owned()));
    assert_eq!(failed(&run), ["processes_alive"]);
    let detail = gates(&run)
        .into_iter()
        .find(|g| g.name == "processes_alive")
        .and_then(|g| g.detail);
    assert_eq!(detail.as_deref(), Some("node-2: signal 9"));
}

#[test]
fn a_process_that_did_not_exit_zero_on_term_is_red() {
    // Catches: a farm that has to be killed to stop passing.
    let mut run = pass();
    run.stopped
        .push(("daemon-1".to_owned(), "exit 1".to_owned()));
    assert_eq!(failed(&run), ["clean_stop"]);
    let gates = gates(&run);
    let stop = gates.iter().find(|g| g.name == "clean_stop").expect("gate");
    // The stop gate's window is the stop's, not the load's.
    assert_eq!(stop.started, "2026-10-18T00:00:10Z");
    assert_eq!(stop.finished, "2026-10-18T00:00:12Z");
}

#[test]
fn a_hang_is_red() {
    // Catches: a run that hit its wall-clock bound passing on what it had measured.
    let mut run = pass();
    run.hung = true;
    assert_eq!(failed(&run), ["hang"]);
    assert_eq!(verdict(&gates(&run), "hang"), Verdict::Fail);
}

#[test]
fn no_gates_is_not_a_pass() {
    // Catches: `all_pass` on an empty table, for a run that failed before it judged.
    assert!(!all_pass(&[]));
}

#[test]
fn a_load_of_no_time_has_no_scrape_coverage() {
    // Catches: a division by a zero load time (NaN, which compares false either way) giving a
    // verdict by accident.
    let run = pass();
    let g = Evidence {
        builds: &run.builds,
        scrapes: &run.scrapes,
        exited_during_load: &run.exited,
        stopped: &run.stopped,
        hung: false,
        load: (String::new(), String::new()),
        load_secs: 0.0,
        stop: (String::new(), String::new()),
        scrape_every_secs: 1.0,
    }
    .gates();
    assert_eq!(verdict(&g, "scrape_coverage"), Verdict::Fail);
}
