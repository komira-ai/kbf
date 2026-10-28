//! The gates over planted evidence. Every test starts from a passing run (`pass()`),
//! changes one thing, and names the defect that a gate which let it through would hide.

use std::collections::BTreeMap;

use super::*;
use crate::scrape::Scrapes;

/// The series key of a member's boot counter.
fn boot_key(member: u32) -> String {
    format!("{MEMBER_BOOT}{{member=\"{member}\"}}")
}

/// A scrape of the frontend: it holds no cache, so it carries no lost-blob series.
fn sample(at: f64, execute_errors: f64) -> Sample {
    let mut series = BTreeMap::from([
        (EXECUTE_ERRORS.to_owned(), execute_errors),
        (MEMBERS_NOT_ACTIVE.to_owned(), 0.0),
    ]);
    for member in 1..=3 {
        series.insert(boot_key(member), 1.0);
    }
    for class in FAILURE_CLASSES {
        series.insert(class_key(class), 0.0);
    }
    Sample { at, series }
}

/// The key of the failed-action series of `class`, as the exposition writes it.
fn class_key(class: &str) -> String {
    format!("{FAILED_ACTIONS}{{class=\"{class}\"}}")
}

/// A scrape of a storage node.
fn node_sample(at: f64, lost: f64, losses: f64) -> Sample {
    let series = BTreeMap::from([
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

/// What a healthy ten-second run measured: two builds, eleven scrapes of the frontend and
/// of each of three storage nodes, nothing lost.
struct Run {
    builds: Vec<BuildResult>,
    scrapes: Scrapes,
    nodes: Vec<(String, Scrapes)>,
    exited: Vec<(String, String)>,
    whole: Result<(), String>,
    stopped: Vec<(String, String)>,
    hung: bool,
    restarts_allowed: u32,
}

fn pass() -> Run {
    Run {
        builds: vec![build(true, Ok(0)), build(true, Ok(0))],
        scrapes: scrapes((0..11).map(|i| sample(f64::from(i), 0.0)).collect()),
        nodes: (1..=3)
            .map(|n| {
                let samples = (0..11)
                    .map(|i| node_sample(f64::from(i), 0.0, 0.0))
                    .collect();
                (format!("node-{n}"), scrapes(samples))
            })
            .collect(),
        exited: Vec::new(),
        whole: Ok(()),
        stopped: vec![("frontend".to_owned(), "exit 0".to_owned())],
        hung: false,
        restarts_allowed: 0,
    }
}

fn gates(run: &Run) -> Vec<Gate> {
    Evidence {
        builds: &run.builds,
        scrapes: &run.scrapes,
        nodes: &run.nodes,
        exited_during_load: &run.exited,
        farm_whole: &run.whole,
        stopped: &run.stopped,
        hung: run.hung,
        restarts_allowed: run.restarts_allowed,
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
    // Catches: a control that is red already, which would make every broken run
    // below go red for the wrong reason.
    let run = pass();
    assert_eq!(failed(&run), Vec::<String>::new());
    assert!(all_pass(&gates(&run)));
    assert_eq!(gates(&run).len(), 14);
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
    run.nodes[1].1.samples[5] = node_sample(5.0, 1.0, 0.0);
    assert_eq!(failed(&run), ["lost_blobs_gauge"]);
}

#[test]
fn the_lost_blob_gates_sum_the_storage_nodes() {
    // Catches: one node's reading taken for the farm's (the first, the last or the
    // largest), so that a loss on the other nodes is not counted: two nodes that each
    // lost one blob read 2.
    let mut run = pass();
    for node in [0, 2] {
        run.nodes[node].1.samples[3] = node_sample(3.0, 1.0, 0.0);
        run.nodes[node].1.samples[10] = node_sample(10.0, 0.0, 1.0);
    }
    let gates = gates(&run);
    for name in ["lost_blobs_gauge", "blob_losses"] {
        let gate = gates.iter().find(|g| g.name == name).expect("gate");
        assert_eq!(gate.verdict, Verdict::Fail, "{name}");
        assert_eq!(gate.measured, json!(2.0), "{name}");
    }
}

#[test]
fn the_frontends_series_are_not_the_lost_blob_evidence() {
    // Catches: the gates reading the frontend's scrapes (which a frontend that grows the
    // series would satisfy) instead of the storage nodes', and a farm with no storage
    // node scraped passing on nothing.
    let mut run = pass();
    run.nodes.clear();
    for s in &mut run.scrapes.samples {
        s.series.insert(LOST_BLOBS.to_owned(), 0.0);
        s.series.insert(BLOB_LOSSES.to_owned(), 0.0);
    }
    assert_eq!(failed(&run), ["lost_blobs_gauge", "blob_losses"]);
    let gate = gates(&run)
        .into_iter()
        .find(|g| g.name == "blob_losses")
        .expect("gate");
    assert!(gate.detail.expect("detail").contains("no storage node"));
}

#[test]
fn the_loss_counter_is_a_delta() {
    // Catches: the counter compared with zero instead of with its first reading (a farm
    // that restarted over a store that already lost blobs is not this run's loss), and
    // a loss the gauge missed.
    let mut run = pass();
    for s in &mut run.nodes[2].1.samples {
        s.series.insert(BLOB_LOSSES.to_owned(), 7.0);
    }
    assert_eq!(failed(&run), Vec::<String>::new());
    run.nodes[2].1.samples[10]
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

/// Asserts that the execute-errors gate reads the sum of the frontend's `cause` series, and
/// goes red when any one of them moves, a client abort included (no cause is a pass).
///
/// Catches: a gate that reads one cause only, or that excuses `client_abort` (reading
/// only the `status` series, say).
#[test]
fn any_one_execute_error_cause_moving_fails_the_gate() {
    let series = |cause: &str| format!("{EXECUTE_ERRORS}{{cause=\"{cause}\"}}");
    let causes = ["status", "client_abort", "dropped"];
    let quiet = || {
        let mut run = pass();
        for s in &mut run.scrapes.samples {
            s.series.remove(EXECUTE_ERRORS);
            for cause in causes {
                s.series.insert(series(cause), 0.0);
            }
        }
        run
    };
    assert_eq!(failed(&quiet()), Vec::<String>::new());
    for moved in causes {
        let mut run = quiet();
        run.scrapes.samples[10].series.insert(series(moved), 1.0);
        assert_eq!(failed(&run), ["execute_errors"], "{moved}");
    }
}

/// The detail of the execute-errors gate of `run`.
fn execute_errors_detail(run: &Run) -> String {
    gates(run)
        .into_iter()
        .find(|g| g.name == "execute_errors")
        .and_then(|g| g.detail)
        .expect("detail")
}

/// Asserts that a red execute-errors gate names how each cause series moved, so the
/// report says whether platform errors or client aborts turned it red, and that the
/// verdict is still the sum's.
///
/// Catches: a detail that gives only the summed delta, with the per-series part left out
/// of `execute_errors_gate`.
#[test]
fn a_red_execute_errors_gate_names_the_causes_that_moved() {
    let series = |cause: &str| format!("{EXECUTE_ERRORS}{{cause=\"{cause}\"}}");
    let mut run = pass();
    for s in &mut run.scrapes.samples {
        s.series.remove(EXECUTE_ERRORS);
        for cause in ["status", "client_abort", "dropped"] {
            s.series.insert(series(cause), 0.0);
        }
    }
    run.scrapes.samples[10]
        .series
        .insert(series("client_abort"), 2.0);
    assert_eq!(failed(&run), ["execute_errors"]);
    let detail = execute_errors_detail(&run);
    assert!(
        detail.contains(&format!("{} +2", series("client_abort"))),
        "{detail}"
    );
    assert!(
        detail.contains(&format!("{} +0", series("status"))),
        "{detail}"
    );
    assert!(
        detail.contains(&format!("{} +0", series("dropped"))),
        "{detail}"
    );
}

/// Asserts that a gate that could not be measured (a series absent from a scrape, too few
/// scrapes) keeps its own reason as the detail, with no per-series part; and that a cause
/// series missing from the last scrape only is left out of the part of the others.
///
/// Catches: a per-series part made of series that cannot be read, as when an absent last
/// value is treated as 0.
#[test]
fn an_unmeasured_execute_errors_gate_has_no_per_series_part() {
    let mut run = pass();
    run.scrapes.samples[10].series.remove(EXECUTE_ERRORS);
    let detail = execute_errors_detail(&run);
    assert!(detail.contains("absent"), "{detail}");
    assert!(!detail.contains("by series"), "{detail}");
    run.scrapes.samples.truncate(1);
    let detail = execute_errors_detail(&run);
    assert!(!detail.contains("by series"), "{detail}");

    let mut run = pass();
    let (a, b) = (
        format!("{EXECUTE_ERRORS}{{cause=\"a\"}}"),
        format!("{EXECUTE_ERRORS}{{cause=\"b\"}}"),
    );
    for s in &mut run.scrapes.samples {
        s.series.remove(EXECUTE_ERRORS);
        s.series.insert(a.clone(), 0.0);
    }
    run.scrapes.samples[0].series.insert(b.clone(), 0.0);
    run.scrapes.samples[5..]
        .iter_mut()
        .for_each(|s| _ = s.series.insert(a.clone(), 1.0));
    let detail = execute_errors_detail(&run);
    assert!(detail.contains(&format!("{a} +1")), "{detail}");
    assert!(!detail.contains(&b), "{detail}");
}

#[test]
fn an_absent_metric_fails_its_gate_in_the_first_scrape_and_in_a_later_one() {
    // Catches: an absent series read as zero, so a metric the frontend stops exporting
    // turns its gate green.
    for scrape in [0, 7] {
        let mut run = pass();
        run.scrapes.samples[scrape].series.remove(EXECUTE_ERRORS);
        assert_eq!(failed(&run), ["execute_errors"], "scrape {scrape}");
    }
}

#[test]
fn one_storage_nodes_missing_series_fails_the_gate_whatever_the_others_read() {
    // Catches: the lost-blob gates summing what is there (a node that stopped exporting,
    // or whose node never answered, adding nothing to a sum of zeros), so that the one
    // node that lost a blob is the one the gate cannot see; and a detail that does not say
    // which node.
    for node in 0..3 {
        for scrape in [0, 7] {
            for (name, gate) in [
                (LOST_BLOBS, "lost_blobs_gauge"),
                (BLOB_LOSSES, "blob_losses"),
            ] {
                let mut run = pass();
                run.nodes[node].1.samples[scrape].series.remove(name);
                assert_eq!(
                    failed(&run),
                    [gate],
                    "{name} from node {node}, scrape {scrape}"
                );
                let detail = gates(&run)
                    .into_iter()
                    .find(|g| g.name == gate)
                    .and_then(|g| g.detail)
                    .expect("detail");
                let who = format!("node-{}: {name} is absent from scrape {scrape}", node + 1);
                assert!(detail.contains(&who), "{detail}");
            }
        }
    }
}

#[test]
fn fewer_than_two_scrapes_pass_no_series_gate() {
    // Catches: a run whose scraper never got a page passing on a difference of nothing.
    let mut run = pass();
    run.scrapes.samples.truncate(1);
    run.nodes[0].1.samples.truncate(1);
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
    let mut s = sample(0.0, 0.0);
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
fn scrapes_that_failed_count_against_coverage() {
    // Catches: a coverage measured against the length of the load alone. The scrapes of
    // the settling window and the last one are extra, so a scraper that failed one in four
    // of its tries still reached the load's scrape count and passed.
    let mut run = pass();
    run.scrapes.failed = 4;
    assert_eq!(failed(&run), ["scrape_coverage"]);
    // Three failures of fourteen tries: 11 of 14 is 0.79, under 0.9.
    run.scrapes.failed = 3;
    assert_eq!(failed(&run), ["scrape_coverage"]);
    // One of twelve tries is 11 of 12, which is 0.92.
    run.scrapes.failed = 1;
    assert!(failed(&run).is_empty(), "{:?}", failed(&run));
}

#[test]
fn a_farm_that_is_not_whole_is_red() {
    // Catches: a node or member that is still running but is no longer part of the farm
    // (deregistered, or wedged) passing because the builds went to the others.
    let mut run = pass();
    run.whole = Err("nodes connected: [], expected [\"node-1\"]".to_owned());
    assert_eq!(failed(&run), ["farm_whole"]);
    let gate = gates(&run)
        .into_iter()
        .find(|g| g.name == "farm_whole")
        .expect("gate");
    assert!(gate.detail.expect("detail").contains("expected"));
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
        nodes: &run.nodes,
        exited_during_load: &run.exited,
        farm_whole: &run.whole,
        stopped: &run.stopped,
        hung: false,
        restarts_allowed: 0,
        load: (String::new(), String::new()),
        load_secs: 0.0,
        stop: (String::new(), String::new()),
        scrape_every_secs: 1.0,
    }
    .gates();
    assert_eq!(verdict(&g, "scrape_coverage"), Verdict::Fail);
}

/// The frontend's execute-error counter, one reading per scrape.
fn execute_errors(run: &mut Run, readings: &[f64]) {
    run.scrapes.samples = readings
        .iter()
        .enumerate()
        .map(|(i, v)| sample(i as f64, *v))
        .collect();
}

/// The detail of the gate `name`.
fn detail(run: &Run, name: &str) -> String {
    gates(run)
        .into_iter()
        .find(|g| g.name == name)
        .and_then(|g| g.detail)
        .unwrap_or_default()
}

#[test]
fn a_counter_that_falls_is_red_whatever_the_ends_say() {
    // Catches: a counter that went down (a restarted process: its counts began again from
    // zero) read as "nothing happened". Catches: the verdict `measured == 0.0` becomes
    // `<= 0.0` (a delta of -5 passes), or the fall is not looked for (the ends of
    // [5, ..., 5, 0, 5] agree, and so do a spike's: 0, 5, 0).
    let mut run = pass();
    execute_errors(
        &mut run,
        &[5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 0.0],
    );
    assert_eq!(failed(&run), ["execute_errors"], "a fall to zero");
    assert!(detail(&run, "execute_errors").contains("reset"));
    execute_errors(
        &mut run,
        &[5.0, 5.0, 5.0, 5.0, 5.0, 0.0, 5.0, 5.0, 5.0, 5.0, 5.0],
    );
    assert_eq!(failed(&run), ["execute_errors"], "a fall and a rise back");
    execute_errors(
        &mut run,
        &[0.0, 0.0, 0.0, 0.0, 0.0, 5.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    );
    assert_eq!(failed(&run), ["execute_errors"], "a spike in the middle");
    // The same for a storage node's counter, whichever node it is.
    for node in 0..3 {
        let mut run = pass();
        for (i, s) in run.nodes[node].1.samples.iter_mut().enumerate() {
            s.series
                .insert(BLOB_LOSSES.to_owned(), if i == 5 { 2.0 } else { 4.0 });
        }
        assert_eq!(failed(&run), ["blob_losses"], "node {node}");
    }
}

#[test]
fn a_gauge_below_zero_is_not_zero() {
    // Catches: the verdict read as "not above zero": a lost-blob gauge of -1 is not a
    // healthy reading but a defect of what exports it. Catches: `measured == 0.0` becomes
    // `<= 0.0` in `nodes_gate`.
    let mut run = pass();
    for s in &mut run.nodes[1].1.samples {
        s.series.insert(LOST_BLOBS.to_owned(), -1.0);
    }
    assert_eq!(failed(&run), ["lost_blobs_gauge"]);
}

#[test]
fn a_counter_is_judged_by_its_last_reading_less_its_first() {
    // Catches: the first reading taken from the second scrape (a rise between the first two
    // scrapes, 0 then 7, hides) and the last taken from before the end; the existing delta
    // tests change the last reading alone. Catches: `v[0]` -> `v[1]`, `v[len - 1]` ->
    // `v[len - 2]` in `counter_delta`.
    let mut run = pass();
    execute_errors(
        &mut run,
        &[0.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0],
    );
    assert_eq!(
        failed(&run),
        ["execute_errors"],
        "a rise in the first interval"
    );
    execute_errors(
        &mut run,
        &[7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 9.0],
    );
    assert_eq!(
        failed(&run),
        ["execute_errors"],
        "a rise in the last interval"
    );
    execute_errors(
        &mut run,
        &[7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0, 7.0],
    );
    assert_eq!(
        failed(&run),
        Vec::<String>::new(),
        "a counter that stood still"
    );
}

#[test]
fn a_reading_that_is_not_a_finite_number_is_red() {
    // Catches: NaN and infinities passing by accident of comparison (`NaN == 0.0` is false
    // today, but `f64::max` skips a NaN, so the gauge's maximum would not see one, and a
    // difference of two infinities is NaN). Each is red, in the middle of a series as at its
    // ends, with the reason. Catches: `series` accepts every `f64`.
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for at in [0, 5, 10] {
            let mut run = pass();
            run.scrapes.samples[at]
                .series
                .insert(EXECUTE_ERRORS.to_owned(), bad);
            assert_eq!(failed(&run), ["execute_errors"], "{bad} at {at}");
            assert!(
                detail(&run, "execute_errors").contains("not a finite number"),
                "{}",
                detail(&run, "execute_errors")
            );
            let mut run = pass();
            run.nodes[2].1.samples[at]
                .series
                .insert(LOST_BLOBS.to_owned(), bad);
            assert_eq!(failed(&run), ["lost_blobs_gauge"], "{bad} at {at}");
        }
    }
}

#[test]
fn the_scrape_coverage_threshold_is_inclusive_at_the_boundary() {
    // Catches: a threshold off by one in either direction. Nine scrapes of ten tries in a
    // ten-second load are exactly 0.9: a pass; nine of eleven are 0.82: a fail. Catches:
    // `coverage >= MIN_SCRAPE_COVERAGE` becomes `>`.
    assert!((MIN_SCRAPE_COVERAGE - 0.9).abs() < f64::EPSILON);
    let mut run = pass();
    run.scrapes.samples.truncate(9);
    run.scrapes.failed = 1;
    assert_eq!(failed(&run), Vec::<String>::new());
    let gate = gates(&run)
        .into_iter()
        .find(|g| g.name == "scrape_coverage")
        .expect("gate");
    assert_eq!(gate.measured, json!(0.9));
    run.scrapes.failed = 2;
    assert_eq!(failed(&run), ["scrape_coverage"]);
}

/// A run in which the gate `name`, and only it, fails: the broken run for each gate.
/// A gate that is added without a case here fails [`every_gate_has_a_case_that_fails_it_alone`].
fn broken(name: &str) -> Run {
    let mut run = pass();
    match name {
        "builds_run" => run.builds.clear(),
        "builds_failed" => run.builds[0].ok = false,
        "actions_failed" => run.builds[0].failed_actions = Ok(1),
        "execute_errors" => {
            run.scrapes.samples[10]
                .series
                .insert(EXECUTE_ERRORS.to_owned(), 1.0);
        }
        "failed_actions_by_class" => {
            run.scrapes.samples[10]
                .series
                .insert(class_key("farm"), 1.0);
        }
        "lost_blobs_gauge" => run.nodes[0].1.samples[4] = node_sample(4.0, 1.0, 0.0),
        "blob_losses" => run.nodes[0].1.samples[10] = node_sample(10.0, 0.0, 1.0),
        "scrape_coverage" => run.scrapes.failed = 5,
        "processes_alive" => run
            .exited
            .push(("node-2".to_owned(), "signal 9".to_owned())),
        "farm_whole" => run.whole = Err("nodes connected: []".to_owned()),
        "clean_stop" => run
            .stopped
            .push(("daemon-1".to_owned(), "exit 1".to_owned())),
        "members_down_at_most_one" => {
            run.scrapes.samples[5]
                .series
                .insert(MEMBERS_NOT_ACTIVE.to_owned(), 2.0);
        }
        "member_restarts" => boots_from(&mut run, 2, 6, 2.0),
        "hang" => run.hung = true,
        other => panic!("gate {other} has no failing case: add it to `broken`"),
    }
    run
}

#[test]
fn every_gate_has_a_case_that_fails_it_alone() {
    // Catches: a gate that never fails, the kind that cannot fail: every gate the
    // evidence produces must name a broken run (`broken`) that turns it, and nothing
    // else, red; the control (`pass`) is green. A gate added without its case fails here.
    // Catches: any gate's verdict made constant `PASS` (or its row dropped, which the count
    // in `a_healthy_run_passes_every_gate` also sees).
    let names: Vec<String> = gates(&pass()).into_iter().map(|g| g.name).collect();
    let mut seen = std::collections::BTreeSet::new();
    for name in &names {
        assert_eq!(failed(&broken(name)), [name.as_str()], "{name}");
        assert!(seen.insert(name.clone()), "{name} is listed twice");
    }
    assert_eq!(names.len(), 14);
}

/// Sets member `member`'s boot to `boot` from scrape `from` on.
fn boots_from(run: &mut Run, member: u32, from: usize, boot: f64) {
    for sample in &mut run.scrapes.samples[from..] {
        sample.series.insert(boot_key(member), boot);
    }
}

/// Asserts that one member out of `ACTIVE` is allowed and two are not, in any one scrape,
/// the first included. Catches: a threshold of zero (a planned single restart read as a
/// failure), a gate that reads only the last scrape (two down in the middle of the load,
/// none at the end), and one that sums a scrape's count with another's.
#[test]
fn two_members_down_at_once_in_any_scrape_is_red_and_one_is_not() {
    let mut run = pass();
    for sample in &mut run.scrapes.samples {
        sample.series.insert(MEMBERS_NOT_ACTIVE.to_owned(), 1.0);
    }
    assert_eq!(failed(&run), Vec::<String>::new());
    for scrape in [0, 5, 9] {
        let mut run = pass();
        run.scrapes.samples[scrape]
            .series
            .insert(MEMBERS_NOT_ACTIVE.to_owned(), 2.0);
        assert_eq!(
            failed(&run),
            ["members_down_at_most_one"],
            "scrape {scrape}"
        );
    }
}

/// Asserts that the gate fails closed on a missing count, in the first scrape and a later
/// one. Catches: an unreadable membership (the series is then not exported) read as no
/// member down.
#[test]
fn an_absent_member_count_fails_the_down_gate() {
    for scrape in [0, 7] {
        let mut run = pass();
        run.scrapes.samples[scrape]
            .series
            .remove(MEMBERS_NOT_ACTIVE);
        assert_eq!(
            failed(&run),
            ["members_down_at_most_one"],
            "scrape {scrape}"
        );
    }
}

/// Asserts that a restart is a rise of the member's boot, that the number allowed is
/// per member and inclusive, and that two restarts of one member are two. Catches: a
/// gate that counts members that restarted instead of restarts, an off-by-one at the
/// allowance, a rise of two read as one.
#[test]
fn a_restart_is_a_rise_of_the_boot_and_the_allowance_is_per_member() {
    let mut run = pass();
    boots_from(&mut run, 2, 4, 2.0);
    assert_eq!(failed(&run), ["member_restarts"]);
    run.restarts_allowed = 1;
    assert_eq!(failed(&run), Vec::<String>::new());
    boots_from(&mut run, 2, 8, 3.0);
    assert_eq!(failed(&run), ["member_restarts"]);
    let detail = detail(&run, "member_restarts");
    assert!(detail.contains("most of any member"), "{detail}");
    // One restart each of two members is still one per member.
    let mut run = pass();
    run.restarts_allowed = 1;
    boots_from(&mut run, 1, 3, 2.0);
    boots_from(&mut run, 3, 6, 2.0);
    assert_eq!(failed(&run), Vec::<String>::new());
}

/// Asserts that a member missing from a scrape, a boot that falls, a boot that is not a
/// finite number, no boot series at all and too few scrapes each fail the gate. Catches: a
/// member that vanished (and so shows no restart) passing, a reset read as no growth.
#[test]
fn a_boot_series_that_cannot_be_read_fails_the_restart_gate() {
    let mut run = pass();
    run.scrapes.samples[7].series.remove(&boot_key(2));
    assert_eq!(failed(&run), ["member_restarts"]);
    assert!(detail(&run, "member_restarts").contains("absent from scrape 7"));

    let mut run = pass();
    boots_from(&mut run, 1, 0, 3.0);
    boots_from(&mut run, 1, 5, 2.0);
    assert_eq!(failed(&run), ["member_restarts"]);
    assert!(detail(&run, "member_restarts").contains("went down"));

    let mut run = pass();
    boots_from(&mut run, 1, 4, f64::NAN);
    assert_eq!(failed(&run), ["member_restarts"]);
    assert!(detail(&run, "member_restarts").contains("not a finite number"));

    let mut run = pass();
    for sample in &mut run.scrapes.samples {
        sample.series.retain(|key, _| !key.starts_with(MEMBER_BOOT));
    }
    assert_eq!(failed(&run), ["member_restarts"]);
    assert!(detail(&run, "member_restarts").contains("absent from scrape 0"));

    let mut run = pass();
    run.scrapes.samples.truncate(1);
    assert!(failed(&run).contains(&"member_restarts".to_owned()));
}

#[test]
fn a_failed_action_of_any_class_fails_the_by_class_gate_and_names_the_class() {
    // Catches: a class left out of the gate (one that rises unseen), the delta of one class
    // charged to another, the absolute value used (a start value of 4 read as four failures),
    // and a detail that does not say which class rose.
    for class in FAILURE_CLASSES {
        let mut run = pass();
        for s in &mut run.scrapes.samples {
            s.series.insert(class_key(class), 4.0);
        }
        assert_eq!(failed(&run), Vec::<String>::new(), "{class} at a steady 4");
        run.scrapes.samples[10].series.insert(class_key(class), 6.0);
        assert_eq!(failed(&run), ["failed_actions_by_class"], "{class}");
        let gate = gates(&run)
            .into_iter()
            .find(|g| g.name == "failed_actions_by_class")
            .expect("gate");
        assert_eq!(gate.measured[class], json!(2.0), "{class}");
        let detail = gate.detail.expect("detail");
        assert_eq!(detail, format!("{class}: delta 2"));
    }
}

#[test]
fn a_missing_or_reset_class_series_fails_the_by_class_gate() {
    // Catches: a class series that the frontend stops exporting (or exports in only some
    // scrapes) read as zero, and a restart of the frontend between scrapes (the counter
    // falls) read as no failures.
    for class in FAILURE_CLASSES {
        for scrape in [0, 7] {
            let mut run = pass();
            run.scrapes.samples[scrape].series.remove(&class_key(class));
            assert_eq!(
                failed(&run),
                ["failed_actions_by_class"],
                "{class} {scrape}"
            );
            let gate = gates(&run)
                .into_iter()
                .find(|g| g.name == "failed_actions_by_class")
                .expect("gate");
            assert_eq!(gate.measured[class], Value::Null);
            let want = format!(
                "{class}: {} is absent from scrape {scrape}",
                class_key(class)
            );
            assert!(gate.detail.expect("detail").contains(&want));
        }
    }
    let mut run = pass();
    for (i, s) in run.scrapes.samples.iter_mut().enumerate() {
        let value = if i < 5 { 3.0 } else { 0.0 };
        s.series.insert(class_key("farm"), value);
    }
    assert_eq!(failed(&run), ["failed_actions_by_class"]);
    let gate = gates(&run)
        .into_iter()
        .find(|g| g.name == "failed_actions_by_class")
        .expect("gate");
    assert!(
        gate.detail
            .expect("detail")
            .contains("farm: went down from 3 to 0")
    );
}
