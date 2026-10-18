//! Scenario B0 over a real farm: `kbf` processes (a frontend, three storage nodes, two
//! native daemons, started from the units `kbf-deploy` renders), the harness's supervisor,
//! scraper, gates and result document, with a stand-in `buck2` for the load (the beta
//! workflow runs the real one) and, in the first test, a stand-in Envoy.
//!
//! What it catches is the harness failing on a healthy farm (a flag a unit's role refuses, a
//! certificate, a port, a wait that never ends), and each way a run must go red: a failed
//! build, a failed action in an event log, a build that never ends, a process that exits, a
//! farm that never becomes ready, and an Envoy that is required and absent. The real
//! Buck2's farm behaviour, and the mutant that shrinks a node's cap, are the workflow's.

#![cfg(target_os = "linux")]

#[path = "beta_support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::time::Duration;

use kbf_harness::b0::{B0, Ended, Mutant, run};
use kbf_harness::farm::Role;
use kbf_harness::stand_in::{FAKE_ENVOY, sibling_bin};

use support::*;

/// Catches: a harness that fails on a healthy farm, and one that does not use the parts it
/// reports using. A frontend, three storage nodes and two daemons start from their rendered
/// units behind the stand-in Envoy (the nodes register with the frontend, the daemons dial
/// it through Envoy), the farm is ready, builds run in two loops, the scraper reads the
/// frontend's and each storage node's metrics, every process stops with exit 0, and the
/// document says what ran: every gate passes, the native
/// driver and no container, the Envoy named, one process-table row per role, the
/// unattested note, and the log files of every role and of every build.
#[test]
fn a_healthy_farm_passes_every_gate_behind_envoy() {
    let mut args = b0("healthy", "ok", 6);
    args.envoy = Some(sibling_bin(FAKE_ENVOY));
    assert_eq!(run(&args).expect("a run"), Ended::Passed);
    let doc = document(&args);
    assert_eq!(failed(&doc), Vec::<String>::new(), "{doc}");
    assert_eq!(doc["status"], "PASSED");
    assert_eq!(doc["gates"].as_array().expect("gates").len(), 11);
    assert_eq!(doc["attested"], false);
    assert_eq!(doc["runtime"]["driver"], "native");
    assert_eq!(doc["runtime"]["container"], "none (native driver)");
    assert_eq!(
        doc["runtime"]["envoy"],
        sibling_bin(FAKE_ENVOY).display().to_string()
    );
    assert!(doc["builds"]["run"].as_u64().expect("builds") >= 2, "{doc}");
    assert_eq!(doc["builds"]["failed"], 0);
    assert_every_gate_has_a_scenario(&doc);
    assert_keys_are_private(&args);
    // One start of each of frontend, envoy, three nodes and two daemons, none restarted.
    let table = doc["processes"].as_array().expect("processes");
    let mut roles: Vec<&str> = table
        .iter()
        .map(|p| p["role"].as_str().expect("role"))
        .collect();
    roles.sort_unstable();
    assert_eq!(
        roles,
        [
            "daemon-1", "daemon-2", "envoy", "frontend", "node-1", "node-2", "node-3"
        ]
    );
    assert!(
        table
            .iter()
            .all(|p| p["generation"] == 0 && p["ended"] == "exit 0"),
        "{table:?}"
    );
    let logs = args.work.join("logs");
    for file in [
        "frontend.out",
        "envoy.err",
        "node-3.err",
        "daemon-2.out",
        "metrics.jsonl",
        "buck2-0-0.log",
    ] {
        assert!(logs.join(file).exists(), "{file}");
    }
    let series = std::fs::read_to_string(logs.join("metrics.jsonl")).expect("series");
    assert!(series.lines().count() >= 4, "{series}");
    let summary = std::fs::read_to_string(args.work.parent().expect("dir").join("summary.md"))
        .expect("summary");
    assert!(summary.contains("| builds_run |"), "{summary}");
}

/// Catches: the lost-blob gates reading a series no storage node serves (the frontend holds
/// no cache), or a node scraped but not read: on a healthy farm both pass on the three
/// storage nodes' own series, summed over all three, and each node has a series file with
/// both families in every line, while the frontend's file has neither.
#[test]
fn the_loss_gates_read_every_storage_nodes_series() {
    let args = b0("loss-series", "ok", 3);
    assert_eq!(run(&args).expect("a run"), Ended::Passed);
    let doc = document(&args);
    for name in ["lost_blobs_gauge", "blob_losses"] {
        assert_eq!(verdict(&doc, name), "PASS", "{doc}");
        let gate = doc["gates"]
            .as_array()
            .expect("gates")
            .iter()
            .find(|g| g["name"] == name)
            .expect("gate");
        assert_eq!(gate["measured"], 0.0, "{gate}");
        assert!(
            gate["detail"]
                .as_str()
                .expect("detail")
                .contains("summed over 3 storage nodes"),
            "{gate}"
        );
    }
    let logs = args.work.join("logs");
    for node in ["node-1", "node-2", "node-3"] {
        let series = std::fs::read_to_string(logs.join(format!("metrics-{node}.jsonl")))
            .unwrap_or_else(|e| panic!("{node}: {e}"));
        let lines: Vec<&str> = series.lines().collect();
        assert!(lines.len() >= 2, "{node}: {series}");
        for line in lines {
            assert!(
                line.contains("\"kbf_cache_lost_blobs\":0.0"),
                "{node}: {line}"
            );
            assert!(
                line.contains("\"kbf_cache_blob_losses_total\":0.0"),
                "{node}: {line}"
            );
        }
    }
    let series = std::fs::read_to_string(logs.join("metrics.jsonl")).expect("series");
    assert!(
        series.contains("kbf_execute_errors_total"),
        "the frontend's own series are read"
    );
    assert!(
        !series.contains("kbf_cache_lost_blobs"),
        "the frontend holds no cache"
    );
}

/// Catches: a failed build, a failed action in an event log of a build that exited 0, a
/// build that ran its actions locally, and a build that printed no summary, each passing:
/// every one must fail its own gate (and only the gates it should).
#[test]
fn each_kind_of_bad_build_fails_its_gate() {
    for (mode, gates) in [
        ("exit-1", vec!["builds_failed"]),
        ("failed-action", vec!["actions_failed"]),
        ("local", vec!["builds_failed"]),
        ("no-summary", vec!["builds_failed"]),
        ("bad-log", vec!["actions_failed"]),
    ] {
        let args = b0(&format!("bad-{mode}"), mode, 3);
        assert_eq!(run(&args).expect("a run"), Ended::Failed, "{mode}");
        let doc = document(&args);
        assert_eq!(failed(&doc), gates, "{mode}: {doc}");
        assert_eq!(doc["status"], "FAILED", "{mode}");
    }
}

/// Catches: a build that never ends holding the run until the job's timeout: it is killed at
/// its bound and the run is red (`builds_failed`, and `actions_failed` for the event log it
/// never wrote), with every process still stopped cleanly.
#[test]
fn a_build_that_never_ends_is_killed_and_the_run_is_red() {
    let mut args = b0("hang", "hang", 2);
    args.build_timeout = Duration::from_secs(3);
    args.concurrency = 1;
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    let doc = document(&args);
    assert_eq!(failed(&doc), ["builds_failed", "actions_failed"], "{doc}");
    assert_eq!(verdict(&doc, "clean_stop"), "PASS");
}

/// Catches: a load that outlasts its wall-clock bound (a build that cannot be killed) holding
/// the run until the job's timeout: the run ends `TIMED_OUT` at the bound, red on `hang`, with
/// the process list in `processes.txt` (the logs are beside it), and every process still
/// stopped.
#[test]
fn a_load_past_its_wall_bound_is_timed_out_with_a_report() {
    let mut args = b0("bound", "hang", 2);
    args.concurrency = 1;
    args.wall_bound = Duration::from_secs(3);
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    let doc = document(&args);
    assert_eq!(doc["status"], "TIMED_OUT");
    assert!(failed(&doc).contains(&"hang".to_owned()), "{doc}");
    assert!(
        doc["error"]
            .as_str()
            .expect("error")
            .contains("processes.txt")
    );
    let list = std::fs::read_to_string(args.work.join("logs/processes.txt")).expect("the list");
    assert!(list.starts_with("--- processes ---"), "{list}");
    assert!(list.contains("frontend pid"), "{list}");
}

/// Whether the stand-in `buck2 kill` of the run in `args` ran to its end (it leaves a marker
/// then; the harness gives up on it and kills the client before that).
fn kill_survived(args: &B0) -> bool {
    args.work.join("project/.fake-buck2-kill-survived").exists()
}

/// Catches: an unbounded `buck2 kill` at the end of a loop (a wedged daemon holds the call
/// for ever, so the loop never ends and the run lasts until the job's timeout): the call is
/// given up on at its bound, which the stand-in shows by never reaching its end. The
/// assertion is on what the stand-in did, not on how long anything took.
#[test]
fn a_kill_that_never_returns_is_given_up_on_at_the_end_of_a_loop() {
    let mut args = b0("kill-hangs", "kill-hangs", 2);
    args.concurrency = 1;
    args.kill_timeout = Duration::from_secs(1);
    assert_eq!(run(&args).expect("a run"), Ended::Passed);
    let doc = document(&args);
    assert_eq!(failed(&doc), Vec::<String>::new(), "{doc}");
    assert!(!kill_survived(&args), "the kill ran to its end");
}

/// Catches: an unbounded `buck2 kill` in the run that gives up on its load (the wall bound):
/// the one run that has a wedged daemon would block on it, with no `TIMED_OUT`, no process
/// list and no stop. The run ends `TIMED_OUT` with its report, and the kill was given up on.
#[test]
fn a_kill_that_never_returns_does_not_hold_up_the_wall_bound() {
    let mut args = b0("bound-kill-hangs", "hang kill-hangs", 2);
    args.concurrency = 1;
    args.wall_bound = Duration::from_secs(3);
    args.kill_timeout = Duration::from_secs(1);
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    let doc = document(&args);
    assert_eq!(doc["status"], "TIMED_OUT");
    assert!(args.work.join("logs/processes.txt").exists());
    assert!(!kill_survived(&args), "the kill ran to its end");
}

/// Catches: a node that dies on its own (not killed by the harness) going unseen: the run
/// notices at once, stops loading, fails `processes_alive` (and `clean_stop`, which reports
/// every role) with the role and how it died, and still stops the others cleanly. The load is a hundred seconds long and ends long before
/// (a bound that only a load which did not stop would break, not a measure of time).
#[test]
fn a_node_that_dies_on_its_own_fails_the_liveness_gate() {
    let args = b0("dies", "ok", 100);
    let work = args.work.clone();
    // Kills node-3 once the load is under way, from outside the harness.
    let killer = std::thread::spawn(move || kill_node_3_under_load(&work));
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    killer.join().expect("the killer");
    let doc = document(&args);
    assert!(
        failed(&doc).contains(&"processes_alive".to_owned()),
        "{doc}"
    );
    let gate = doc["gates"]
        .as_array()
        .expect("gates")
        .iter()
        .find(|g| g["name"] == "processes_alive");
    assert_eq!(gate.expect("gate")["detail"], "node-3: signal 9");
    // The stop reports every role, one that had died before its turn with how it died:
    // the dead node is not a clean stop, and it is the only one that is not.
    let stop = doc["gates"]
        .as_array()
        .expect("gates")
        .iter()
        .find(|g| g["name"] == "clean_stop");
    let stop = stop.expect("gate");
    assert_eq!(stop["verdict"], "FAIL", "{stop}");
    assert_eq!(stop["detail"], "node-3: exited before the stop: signal 9");
    let took = doc["duration_secs"].as_f64().expect("a duration");
    assert!(
        took < 90.0,
        "the load went on after a process died: {took} s"
    );
}

/// Catches: the liveness gate that cannot go red: a node killed halfway through the load
/// (the kill-node-midway mutant) must fail `processes_alive`, and the run must not call a
/// farm that lost a member healthy, while the other roles stop cleanly.
#[test]
fn a_node_killed_midway_fails_the_liveness_gate() {
    let mut args = b0("kill-midway", "ok", 8);
    args.mutant = Some(Mutant::KillNodeMidway);
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    let doc = document(&args);
    assert!(
        failed(&doc).contains(&"processes_alive".to_owned()),
        "{doc}"
    );
    assert_eq!(verdict(&doc, "clean_stop"), "PASS");
    let detail = doc["gates"]
        .as_array()
        .expect("gates")
        .iter()
        .find(|g| g["name"] == "processes_alive");
    assert!(
        detail.expect("gate")["detail"]
            .as_str()
            .expect("detail")
            .contains("node-2")
    );
    // The node was killed, not just said to be: its process ended by SIGKILL.
    let table = doc["processes"].as_array().expect("processes");
    let node_2 = table.iter().find(|p| p["role"] == "node-2");
    assert_eq!(node_2.expect("a row")["ended"], "signal 9", "{table:?}");
}

/// Catches: an Envoy that is required and missing becoming a skip (a green smoke that never
/// used Envoy), and a farm started before the check: the run fails at once, with the reason
/// in the document and no process started.
#[test]
fn a_required_envoy_that_is_absent_fails_the_run() {
    let mut args = b0("no-envoy", "ok", 3);
    args.require_envoy = true;
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    let doc = document(&args);
    assert_eq!(doc["status"], "FAILED");
    assert!(
        doc["error"]
            .as_str()
            .expect("error")
            .contains("Envoy is required"),
        "{doc}"
    );
    assert!(
        doc["runtime"]["envoy"]
            .as_str()
            .expect("envoy")
            .starts_with("absent: "),
        "{doc}"
    );
    assert!(!args.work.join("farm").exists());
}

/// Catches: an Envoy that is not required being missed without a word: the run goes on without
/// one and the document says `absent: <why>`.
#[test]
fn an_absent_envoy_is_recorded_when_it_is_not_required() {
    let args = b0("no-envoy-ok", "ok", 3);
    assert_eq!(run(&args).expect("a run"), Ended::Passed);
    let doc = document(&args);
    assert_eq!(
        doc["runtime"]["envoy"],
        "absent: KBF_TEST_NO_SUCH_ENVOY is not set"
    );
    assert_eq!(failed(&doc), Vec::<String>::new(), "{doc}");
}

/// Catches: a farm that cannot start (a `kbf` that exits at once) being reported as a pass,
/// a panic, or a hang: the run is red with `farm_ready` failing and the exit in the report.
#[test]
fn a_farm_that_cannot_start_is_red_with_the_reason() {
    let mut args = b0("no-farm", "ok", 3);
    // A program that exits at once, in place of `kbf`.
    args.kbf = PathBuf::from(env!("CARGO_BIN_EXE_kbf-cli-fake-systemctl"));
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    let doc = document(&args);
    assert_eq!(doc["status"], "FAILED");
    assert_eq!(verdict(&doc, "farm_ready"), "FAIL");
    assert!(args.work.join("logs/hang-report.txt").exists());
}

/// Catches: a farm that starts but never becomes ready (a daemon told to dial a port nobody
/// listens on, so the frontend never lists it) hanging the run: it ends `TIMED_OUT` at the farm's
/// own bound, `farm_ready` red, with the logs of every process in `hang-report.txt`.
#[test]
fn a_farm_that_never_becomes_ready_times_out_with_its_logs() {
    let mut args = b0("not-ready", "ok", 3);
    args.farm_hang = Duration::from_secs(15);
    args.extra_flags = vec![(
        Role::Daemon(1),
        vec!["--server=https://localhost:1".to_owned()],
    )];
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    let doc = document(&args);
    assert_eq!(doc["status"], "TIMED_OUT");
    assert_eq!(verdict(&doc, "farm_ready"), "FAIL");
    let report = std::fs::read_to_string(args.work.join("logs/hang-report.txt")).expect("report");
    assert!(
        report.contains("timed out waiting for every node connected to the frontend"),
        "{report}"
    );
    assert!(report.contains("--- daemon-1 stderr ---"), "{report}");
}

/// Catches: a project that cannot be copied found only after a farm has started and stopped
/// (the run is red at once, with the reason, before any process starts).
#[test]
fn a_project_that_cannot_be_copied_fails_before_a_farm_starts() {
    let mut args = b0("no-project", "ok", 3);
    args.project = args.work.parent().expect("dir").join("no-such-project");
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    let doc = document(&args);
    assert!(
        doc["error"]
            .as_str()
            .expect("error")
            .starts_with("the sample project: "),
        "{doc}"
    );
    assert!(!args.work.join("farm").exists());
}

/// Catches: a project whose `.buckconfig` cannot be written (so Buck2 would dial no farm) being
/// run anyway: red at once with the reason, the farm stopped.
#[test]
fn a_project_whose_buckconfig_cannot_be_written_fails_the_run() {
    let args = b0("buckconfig", "ok", 3);
    std::fs::create_dir(args.project.join(".buckconfig")).expect("a directory in its place");
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    let doc = document(&args);
    assert!(
        doc["error"]
            .as_str()
            .expect("error")
            .starts_with("the project's .buckconfig: "),
        "{doc}"
    );
}

/// Catches: the command line that the workflow runs not reaching the scenario: through
/// `kbf-beta b0 --bin-dir ...` the farm starts, the loops run, the result is written, and the
/// exit status is 1 (a gate failed: the loss gates, today).
#[test]
fn the_command_line_runs_b0_and_exits_1_on_a_failed_gate() {
    let args = b0("cli", "ok", 3);
    let bin = args.work.parent().expect("dir").join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    std::os::unix::fs::symlink(&args.kbf, bin.join("kbf")).expect("link");
    std::os::unix::fs::symlink(&args.act, bin.join("kbf-m2-act")).expect("link");
    let words: Vec<std::ffi::OsString> = [
        "kbf-beta",
        "b0",
        "--duration-secs",
        "3",
        "--settle-secs",
        "1",
        "--concurrency",
        "1",
        "--bin-dir",
        bin.to_str().expect("utf-8"),
        "--buck2",
        args.buck2.to_str().expect("utf-8"),
        "--project",
        args.project.to_str().expect("utf-8"),
        "--work",
        args.work.to_str().expect("utf-8"),
        "--mutant",
        "kill-node-midway",
    ]
    .iter()
    .map(Into::into)
    .collect();
    assert_eq!(kbf_harness::cli::status(words, |_| None), 1);
    let doc = document(&args);
    assert!(
        failed(&doc).contains(&"processes_alive".to_owned()),
        "{doc}"
    );
}
