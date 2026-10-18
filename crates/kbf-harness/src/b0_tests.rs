//! The parts of B0 that need no farm: its choices, its mutants, how it reads the processes
//! and reports a farm that did not come up. The whole scenario is run by kbf-cli's
//! tests/beta_b0.rs, over real `kbf` processes.

use super::*;
use crate::farm::{Farm, Role};
use crate::procs::Spec;

fn args() -> B0 {
    B0 {
        kbf: PathBuf::from("kbf"),
        act: PathBuf::from("act"),
        buck2: PathBuf::from("buck2"),
        envoy: None,
        envoy_var: "KBF_TEST_NO_SUCH_ENVOY".to_owned(),
        project: PathBuf::from("project"),
        work: PathBuf::from("work"),
        duration: Duration::from_secs(1),
        settle: Duration::from_secs(1),
        concurrency: 1,
        build_timeout: Duration::from_secs(1),
        kill_timeout: Duration::from_secs(1),
        wall_bound: Duration::from_secs(5),
        pause_secs: 1,
        fill_kib: 1,
        scrape_every: Duration::from_secs(1),
        summary: Some(PathBuf::from("summary")),
        mutant: None,
        extra_flags: Vec::new(),
        farm_hang: Duration::from_secs(1),
        require_envoy: false,
        interrupt: Interrupt::new(),
        reaper: None,
    }
}

#[test]
fn every_path_is_made_absolute() {
    // Catches: a relative path reaching a unit (the renderer refuses it) or a Buck2 that
    // runs in the project's copy and cannot find a path given relative to ours.
    let mut a = args();
    a.envoy = Some(PathBuf::from("envoy"));
    let abs = a.absolute().expect("absolute");
    for path in [
        &abs.kbf,
        &abs.act,
        &abs.buck2,
        &abs.project,
        &abs.work,
        abs.envoy.as_ref().expect("envoy"),
        abs.summary.as_ref().expect("summary"),
    ] {
        assert!(path.is_absolute(), "{}", path.display());
    }
    assert_eq!(abs.duration, a.duration);
    assert!(args().absolute().expect("absolute").envoy.is_none());
}

#[test]
fn an_envoy_that_is_absent_is_a_skip_unless_required() {
    // Catches: a missing Envoy that is required passing, one that is not required failing,
    // and a document that does not say why there is none.
    let a = args();
    let chosen = choose_envoy(&a).expect("a skip");
    assert_eq!(chosen.binary, None);
    assert_eq!(
        chosen.describe(),
        "absent: KBF_TEST_NO_SUCH_ENVOY is not set"
    );
    let mut required = args();
    required.require_envoy = true;
    let err = choose_envoy(&required).err().expect("an error");
    assert_eq!(
        err,
        "Envoy is required (KBF_BETA_REQUIRE_ENVOY=1) and absent: KBF_TEST_NO_SUCH_ENVOY is not set"
    );
}

#[test]
fn an_envoy_given_is_used_as_given() {
    // Catches: the flag lost to the environment's variable, or required-ness refusing an
    // Envoy that is there.
    let mut a = args();
    a.envoy = Some(PathBuf::from("/opt/envoy"));
    a.require_envoy = true;
    let chosen = choose_envoy(&a).expect("the binary");
    assert_eq!(chosen.binary, Some(PathBuf::from("/opt/envoy")));
    assert_eq!(chosen.describe(), "/opt/envoy");
}

#[test]
fn the_cap_mutants_change_flags_on_the_nodes_they_name() {
    // Catches: a mutant that changes nothing (the gate it exists to prove is never seen red),
    // one that changes the wrong node, keeps the evict grace (an object just written is
    // never evicted) or the floor, or one that touches a farm without being asked.
    assert!(overrides(None).is_empty());
    assert!(overrides(Some(Mutant::KillNodeMidway)).is_empty());
    let flags = [
        "--cold-store-size-bytes=1048576",
        "--cold-store-floor-bytes=0",
        "--cold-store-evict-grace-ms=0",
        "--hot-store-max-bytes=1048576",
    ];
    let one = overrides(Some(Mutant::ShrinkNodeCap));
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].0, Role::Node(1));
    assert_eq!(one[0].1, flags);
    let every = overrides(Some(Mutant::ShrinkEveryCap));
    let nodes: Vec<Role> = every.iter().map(|(r, _)| *r).collect();
    assert_eq!(nodes, [Role::Node(1), Role::Node(2), Role::Node(3)]);
    assert!(every.iter().all(|(_, f)| *f == flags));
}

/// `buck2 --version` of `program` in a scratch directory, waiting at most `limit`.
fn version_of(name: &str, program: &Path, limit: Duration) -> String {
    let d = crate::scratch(name);
    buck2_version(program, &d, &d.join("version.txt"), limit)
}

#[test]
fn the_version_is_the_first_line_or_the_reason_there_is_none() {
    // Catches: a version that is lost when the program fails (the document would say
    // nothing of which Buck2 ran), or a failure taken for a version.
    let limit = Duration::from_secs(60);
    // GNU `sleep --version` prints a version line and succeeds.
    assert!(version_of("version-ok", Path::new("sleep"), limit).starts_with("sleep "));
    let missing = version_of("version-missing", Path::new("/nonexistent/buck2"), limit);
    assert!(missing.starts_with("failed: "), "{missing}");
    assert_eq!(
        version_of("version-false", Path::new("false"), limit),
        "failed: exit Some(1)"
    );
    // The output file cannot be made: a directory is in its place.
    let d = crate::scratch("version-nofile");
    let in_the_way = d.join("version.txt");
    std::fs::create_dir_all(&in_the_way).expect("dir");
    let refused = buck2_version(Path::new("sleep"), &d, &in_the_way, limit);
    assert!(refused.starts_with("failed: "), "{refused}");
}

#[test]
fn a_version_that_is_never_answered_is_given_up_on() {
    // Catches: `buck2 --version` without a bound: a wedged Buck2 client would hold the run
    // before it started anything, with no gate and no document to say why (the stand-in
    // sleeps thirty seconds when told to). Mutant: the wait has no deadline.
    let d = crate::scratch("version-hangs");
    std::fs::write(d.join(".fake-buck2"), "version-hangs").expect("mode");
    let fake = crate::stand_in::sibling_bin(crate::stand_in::FAKE_BUCK2);
    let said = buck2_version(&fake, &d, &d.join("version.txt"), Duration::from_secs(1));
    assert_eq!(said, "failed: no answer in 1 s");
}

#[test]
fn an_environment_variable_that_is_unset_is_none() {
    assert!(env("PATH").is_some());
    assert_eq!(env("KBF_TEST_NO_SUCH_VARIABLE"), None);
}

fn sleeper_farm(name: &str, program: &str) -> Farm {
    let d = crate::scratch(name);
    let spec = Spec {
        name: "node-1".to_owned(),
        program: PathBuf::from(program),
        argv: if program == "sleep" {
            vec!["300".to_owned()]
        } else {
            Vec::new()
        },
        env: Vec::new(),
        cwd: d.clone(),
    };
    Farm::supervise(&d, vec![(Role::Node(1), spec)])
}

#[test]
fn an_exit_is_noted_once_and_says_whether_it_is_new() {
    // Catches: a role that exited being recorded again at every poll (the process list of
    // the document would be a log of the polls), or an exit going unseen.
    let mut alive = sleeper_farm("b0-alive", "sleep");
    let mut exited = Vec::new();
    assert!(!note_exits(&mut alive, &mut exited));
    assert!(exited.is_empty());
    let mut dead = sleeper_farm("b0-dead", "false");
    let until = Instant::now() + Duration::from_secs(60);
    while !note_exits(&mut dead, &mut exited) {
        assert!(Instant::now() < until, "false never exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(exited, [("node-1".to_owned(), "exit 1".to_owned())]);
    assert!(!note_exits(&mut dead, &mut exited));
    assert_eq!(exited.len(), 1);
}

#[test]
fn a_role_that_left_before_the_stop_is_a_liveness_failure_and_an_unclean_stop() {
    // Catches: a node that exits (0 or not) in the window between the last poll and the
    // stop, never seen by either gate: `processes_alive` reads `exited`, `clean_stop` reads
    // what the stop returns, and a role skipped by both passes both.
    let mut gone = sleeper_farm("b0-left", "true");
    let until = Instant::now() + Duration::from_secs(60);
    while gone.process_table()[0].ended.is_none() {
        assert!(Instant::now() < until, "true never exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut exited = Vec::new();
    let stopped = stop_farm(&mut gone, &mut exited, Duration::from_secs(60));
    assert_eq!(exited, [("node-1".to_owned(), "exit 0".to_owned())]);
    // Not "exit 0": the harness did not stop it, so the stop is not clean.
    assert_eq!(
        stopped,
        [(
            "node-1".to_owned(),
            "exited before the stop: exit 0".to_owned()
        )]
    );
    let mut crashed = sleeper_farm("b0-crashed", "false");
    while crashed.process_table()[0].ended.is_none() {
        assert!(Instant::now() < until, "false never exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut exited = Vec::new();
    let stopped = stop_farm(&mut crashed, &mut exited, Duration::from_secs(60));
    assert_eq!(
        stopped,
        [(
            "node-1".to_owned(),
            "exited before the stop: exit 1".to_owned()
        )]
    );
    assert_eq!(exited, [("node-1".to_owned(), "exit 1".to_owned())]);
}

fn writer(name: &str) -> (Writer, PathBuf) {
    let dir = crate::scratch(name);
    let path = dir.join("beta-result.json");
    (
        Writer::start(&path, Document::running(SCENARIO, |_| None)),
        dir,
    )
}

#[test]
fn a_farm_that_timed_out_is_timed_out_and_any_other_is_failed() {
    // Catches: a hang reported as a plain failure (the aggregator tells them apart), a
    // failure reported as a hang, and a report that does not reach the logs directory.
    let (mut w, dir) = writer("b0-hang-or-fail");
    let ended = hang_or_fail(
        &mut w,
        &dir,
        "timed out waiting for the frontend's /readyz",
        "--- processes ---\n",
    );
    assert_eq!(ended, Ended::Failed);
    assert_eq!(w.doc.status, Status::TimedOut);
    assert_eq!(w.doc.gates.len(), 1);
    assert_eq!(w.doc.gates[0].name, "farm_ready");
    assert!(
        std::fs::read_to_string(dir.join("hang-report.txt"))
            .expect("report")
            .contains("--- processes ---")
    );
    let (mut w, dir) = writer("b0-hang-or-fail-2");
    hang_or_fail(
        &mut w,
        &dir,
        "frontend exited (exit status: 2) while waiting for x",
        "",
    );
    assert_eq!(w.doc.status, Status::Failed);
    assert_eq!(
        w.doc.error.as_deref(),
        Some("frontend exited (exit status: 2) while waiting for x")
    );
}

fn gate(name: &str, verdict: crate::gates::Verdict) -> Gate {
    Gate {
        name: name.to_owned(),
        measured: serde_json::json!(0),
        threshold: "0".to_owned(),
        verdict,
        started: String::new(),
        finished: String::new(),
        detail: None,
    }
}

#[test]
fn a_run_ends_passed_only_if_every_gate_passed_and_it_did_not_hang() {
    // Catches: a run that is `PASSED` with a gate red, with no gate at all, or after a hang
    // (the load never ended: what the gates measured is not the run's), and the statuses
    // that an aggregator reads: `PASSED`, `FAILED` and `TIMED_OUT` for exactly these.
    use crate::gates::Verdict::{Fail, Pass};
    let green = [gate("a", Pass), gate("b", Pass)];
    let red = [gate("a", Pass), gate("b", Fail)];
    assert_eq!(ending(&green, false), (Status::Passed, Ended::Passed));
    assert_eq!(ending(&red, false), (Status::Failed, Ended::Failed));
    assert_eq!(ending(&[], false), (Status::Failed, Ended::Failed));
    assert_eq!(ending(&green, true), (Status::TimedOut, Ended::Failed));
    assert_eq!(ending(&red, true), (Status::TimedOut, Ended::Failed));
}

#[test]
fn the_sentinel_is_started_only_where_there_is_a_program_for_it() {
    // Catches: a run with no program for a sentinel starting one anyway (a test binary that
    // cannot answer `reap`), a run that has one starting none (the promise that a killed
    // run leaves no daemon, broken silently), and a sentinel that fails to start being
    // swallowed. The reaper's path is made absolute with the others.
    let dir = crate::scratch("b0-sentinel");
    let (project, logs) = (dir.join("project"), dir.join("logs"));
    std::fs::create_dir_all(&project).expect("project");
    let mut a = args();
    assert!(
        start_sentinel(&a, &project, &dir.join("farm"), &logs, "r1")
            .expect("none")
            .is_none()
    );
    a.reaper = Some(PathBuf::from("/nonexistent/kbf-beta"));
    std::fs::create_dir_all(&logs).expect("logs");
    assert!(start_sentinel(&a, &project, &dir.join("farm"), &logs, "r1").is_err());
    a.reaper = Some(PathBuf::from("kbf-beta"));
    assert!(
        a.absolute()
            .expect("absolute")
            .reaper
            .expect("reaper")
            .is_absolute()
    );
    a.reaper = Some(crate::stand_in::sibling_bin("kbf-beta"));
    let sentinel = start_sentinel(&a, &project, &dir.join("farm"), &logs, "r1").expect("started");
    assert!(sentinel.is_some());
}
