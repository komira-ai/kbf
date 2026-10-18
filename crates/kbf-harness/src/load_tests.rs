//! The load's command lines, its judgement of a build, and its handling of a build that
//! does not end, without Buck2.

use std::collections::BTreeMap;

use super::*;

fn cfg() -> LoadConfig {
    LoadConfig {
        buck2: PathBuf::from("/nonexistent/buck2"),
        project: PathBuf::from("/tmp"),
        logs: PathBuf::from("/tmp"),
        ca: PathBuf::from("/farm/ca.pem"),
        port: 4321,
        concurrency: 2,
        duration: Duration::from_secs(1),
        build_timeout: Duration::from_secs(5),
        kill_timeout: Duration::from_secs(5),
        pause_secs: 30,
        fill_kib: 256,
        run_id: "r1".to_owned(),
    }
}

fn args(lane: usize, n: usize, kind: Kind) -> Vec<String> {
    build_args(&cfg(), lane, n, kind, Path::new("/logs/e.json-lines.gz"))
}

/// The value of the `-c key=value` option named `key`.
fn config(args: &[String], key: &str) -> Option<String> {
    args.windows(2)
        .filter(|w| w[0] == "-c")
        .find_map(|w| w[1].strip_prefix(&format!("{key}=")).map(str::to_owned))
}

#[test]
fn every_build_has_a_salt_of_its_own() {
    // Catches: two builds with one salt, the second answered from the action cache, so the
    // load would run nothing on the farm and every gate would pass on cache hits.
    let mut salts = std::collections::BTreeSet::new();
    for lane in 0..3 {
        for n in 0..4 {
            let a = args(lane, n, Kind::Compile);
            assert!(salts.insert(config(&a, "kbf_beta.salt").expect("a salt")));
        }
    }
    assert_eq!(salts.len(), 12);
}

#[test]
fn the_farm_is_dialled_over_tls_under_its_ca_from_the_projects_buckconfig() {
    // Catches: a build that reaches the farm in plain text or without the CA (it would fail
    // on a farm that serves TLS, or pass against a stand-in), one that dials another port
    // than the one given (Envoy's, not the frontend's), and settings given on a command
    // line, where Buck2 ignores them ("No engine address").
    let block = buckconfig_block(&cfg());
    for key in ["engine_address", "action_cache_address", "cas_address"] {
        assert!(
            block.contains(&format!("  {key} = grpc://localhost:4321\n")),
            "{key} in {block}"
        );
    }
    assert!(block.contains("  tls_ca_certs = /farm/ca.pem\n"), "{block}");
    assert!(block.starts_with("\n[buck2_re_client]\n"), "{block}");
    let a = args(0, 0, Kind::Compile);
    assert!(!a.iter().any(|w| w.contains("buck2_re_client")), "{a:?}");
}

#[test]
fn the_block_is_appended_to_the_projects_buckconfig() {
    // Catches: a block that replaces the project's own settings (TLS on, the instance name)
    // instead of following them, or written to a file that is not the project's.
    let dir = crate::scratch("load-buckconfig");
    std::fs::write(dir.join(".buckconfig"), "[buck2_re_client]\n  tls = true\n").expect("file");
    let mut c = cfg();
    c.project = dir.clone();
    write_buckconfig(&c).expect("written");
    let text = std::fs::read_to_string(dir.join(".buckconfig")).expect("file");
    assert!(
        text.starts_with("[buck2_re_client]\n  tls = true\n\n[buck2_re_client]\n  engine_address"),
        "{text}"
    );
    // A file that is not there is made.
    c.project = crate::scratch("load-buckconfig-new");
    write_buckconfig(&c).expect("made");
    assert!(
        std::fs::read_to_string(c.project.join(".buckconfig"))
            .expect("file")
            .contains("engine_address")
    );
    // A `.buckconfig` that is a directory cannot be appended to.
    std::fs::create_dir(dir.join("sub")).expect("dir");
    std::fs::create_dir(dir.join("sub/.buckconfig")).expect("dir");
    c.project = dir.join("sub");
    assert!(write_buckconfig(&c).is_err());
}

#[test]
fn each_loop_has_a_daemon_of_its_own() {
    // Catches: loops sharing one Buck2 daemon, which serializes their builds (the load
    // would be one build at a time whatever the concurrency says).
    let (a, b) = (args(0, 0, Kind::Compile), args(1, 0, Kind::Compile));
    assert_eq!(a[..2], ["--isolation-dir", "beta-0"]);
    assert_eq!(b[..2], ["--isolation-dir", "beta-1"]);
}

#[test]
fn each_kind_asks_for_its_own_load() {
    // Catches: a pause build without its sleep (a lease never held), an upload without
    // its size (nothing new stored), a compile with either.
    let pause = args(0, 0, Kind::Pause);
    assert_eq!(config(&pause, "kbf_beta.sleep_secs").as_deref(), Some("30"));
    assert_eq!(pause.last().map(String::as_str), Some("//:pause"));
    let upload = args(0, 0, Kind::Upload);
    assert_eq!(config(&upload, "kbf_beta.fill_kib").as_deref(), Some("256"));
    assert_eq!(
        &upload[upload.len() - 4..],
        ["//:fill_0", "//:fill_1", "//:fill_2", "//:fill_3"]
    );
    let compile = args(0, 0, Kind::Compile);
    assert_eq!(config(&compile, "kbf_beta.sleep_secs"), None);
    assert_eq!(config(&compile, "kbf_beta.fill_kib"), None);
    assert!(compile.contains(&"//:joined".to_owned()));
    // The event log is where the loop reads failed actions from.
    assert!(
        compile
            .windows(2)
            .any(|w| w == ["--event-log", "/logs/e.json-lines.gz"])
    );
}

#[test]
fn the_loops_start_on_different_kinds_and_each_covers_all() {
    // Catches: every loop doing the same kind at once (all pausing, none uploading).
    let first: std::collections::BTreeSet<Kind> = (0..3).map(|lane| kind_for(lane, 0)).collect();
    assert_eq!(first.len(), 3);
    for lane in 0..3 {
        let seen: std::collections::BTreeSet<Kind> = (0..3).map(|n| kind_for(lane, n)).collect();
        assert_eq!(seen.len(), 3, "lane {lane}");
    }
}

fn spawns(remote: u64, cached: u64, other: &[(&str, u64)]) -> Result<Spawns, String> {
    Ok(Spawns {
        remote,
        cached,
        other: other
            .iter()
            .map(|(k, n)| ((*k).to_owned(), *n))
            .collect::<BTreeMap<_, _>>(),
    })
}

fn outcome(exit: Option<i32>, timed_out: bool, spawns: Result<Spawns, String>) -> Outcome {
    Outcome {
        kind: Kind::Compile,
        lane: 0,
        n: 0,
        exit,
        timed_out,
        secs: 1.0,
        spawns,
        failed_actions: Ok(0),
    }
}

#[test]
fn a_build_is_ok_only_if_it_ran_actions_on_the_farm_and_nowhere_else() {
    // Catches: a pass for a build that exited 0 but ran its actions on the client
    // (the farm untested), ran none (all cache hits: the load did nothing), printed no
    // summary (we cannot say what it did), or timed out.
    assert!(outcome(Some(0), false, spawns(4, 0, &[])).ok());
    assert!(!outcome(Some(1), false, spawns(4, 0, &[])).ok());
    assert!(!outcome(None, true, spawns(4, 0, &[])).ok());
    assert!(!outcome(Some(0), true, spawns(4, 0, &[])).ok());
    assert!(!outcome(Some(0), false, spawns(3, 0, &[("local", 1)])).ok());
    assert!(!outcome(Some(0), false, spawns(0, 4, &[])).ok());
    assert!(!outcome(Some(0), false, Err("no summary line".to_owned())).ok());
}

#[test]
fn a_build_that_cannot_be_started_is_a_failure_with_a_reason() {
    // Catches: a missing Buck2 binary becoming a panic of a loop's thread, or a build
    // that counts as run and passed.
    let dir = crate::scratch("load-spawn");
    let mut c = cfg();
    c.logs = dir.clone();
    c.project = dir;
    let o = build(&c, 0, 0, Kind::Compile);
    assert!(!o.ok());
    assert_eq!(o.exit, None);
    assert!(
        o.spawns.as_ref().expect_err("no summary").contains("spawn"),
        "{:?}",
        o.spawns
    );
    assert!(o.failed_actions.is_err());
}

#[test]
fn a_build_that_does_not_end_is_killed_at_its_bound() {
    // Catches: a hung build holding the scenario until the job's timeout, with nothing
    // said of which build it was; and a code read from a process that was killed.
    let mut hung = Command::new("sleep").arg("300").spawn().expect("sleep");
    assert_eq!(
        wait_bounded(&mut hung, Duration::from_millis(200)),
        (None, true)
    );
    let mut ok = Command::new("true").spawn().expect("true");
    assert_eq!(
        wait_bounded(&mut ok, Duration::from_secs(60)),
        (Some(0), false)
    );
    let mut bad = Command::new("false").spawn().expect("false");
    assert_eq!(
        wait_bounded(&mut bad, Duration::from_secs(60)),
        (Some(1), false)
    );
}

#[test]
fn a_kill_of_a_daemon_that_cannot_be_run_is_skipped() {
    // Catches: a `buck2` that cannot be started (it was never there, or the run is being
    // torn down after it failed to) panicking the teardown, which would hide the failure
    // the run is reporting.
    let c = cfg();
    Daemons::of(&c).kill_lane(0);
    kill_daemons(&c);
}

#[test]
fn the_project_is_copied_with_its_tool_and_without_build_output() {
    // Catches: a tool that is not executable (every action would fail to start), a build
    // output of an earlier run copied into the new one, and a second run into one directory.
    let dir = crate::scratch("load-prepare");
    let (source, work) = (dir.join("source"), dir.join("work"));
    std::fs::create_dir_all(source.join("platforms")).expect("dirs");
    std::fs::create_dir_all(source.join("buck-out/old")).expect("dirs");
    std::fs::create_dir_all(&work).expect("dirs");
    std::fs::write(source.join("BUCK"), "x").expect("file");
    std::fs::write(source.join("platforms/BUCK"), "y").expect("file");
    std::fs::write(source.join("buck-out/old/out"), "z").expect("file");
    let act = dir.join("act");
    std::fs::write(&act, "tool").expect("file");
    let project = prepare(&source, &work, &act).expect("copied");
    assert_eq!(
        std::fs::read_to_string(project.join("platforms/BUCK")).expect("file"),
        "y"
    );
    assert!(!project.join("buck-out").exists());
    let tool = project.join("tools/kbf-m2-act");
    assert_eq!(std::fs::read_to_string(&tool).expect("tool"), "tool");
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(
        std::fs::metadata(&tool).expect("meta").permissions().mode() & 0o111,
        0o111
    );
    let again = prepare(&source, &work, &act).expect_err("a project is there");
    assert!(again.to_string().contains("exists"), "{again}");
}

/// A load over the stand-in `buck2` (`mode` in its `.fake-buck2`), `concurrency` loops for
/// `secs` seconds, in a fresh project directory.
fn stand_in_load(name: &str, mode: &str, concurrency: usize, secs: u64) -> LoadConfig {
    let dir = crate::scratch(name);
    let (project, logs) = (dir.join("project"), dir.join("logs"));
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&logs).expect("logs");
    std::fs::write(project.join(".fake-buck2"), mode).expect("mode");
    LoadConfig {
        buck2: crate::stand_in::sibling_bin(crate::stand_in::FAKE_BUCK2),
        project,
        logs,
        concurrency,
        duration: Duration::from_secs(secs),
        ..cfg()
    }
}

/// How many times the stand-in `buck2` was told to `kill` the daemon of loop `lane`.
fn kills(c: &LoadConfig, lane: usize) -> usize {
    std::fs::read_to_string(c.project.join(format!(".fake-buck2-killed-beta-{lane}")))
        .map_or(0, |text| text.lines().count())
}

#[test]
fn a_load_that_is_dropped_kills_the_daemon_of_every_loop() {
    // Catches: a run that panics (or returns early) after the load started leaving a Buck2
    // daemon per loop behind: the daemon leads a session of its own and outlives its client,
    // holding the farm's connection and a port, and a runner's next job meets it. The load
    // here is ten minutes long and the stand-in's builds take a moment, so no loop has ended
    // by itself when it is dropped in a panic. Mutant: no `Drop` on the load's guard.
    let c = stand_in_load("load-drop", "ok", 2, 600);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _load = Load::start(&c);
        panic!("a failed assertion after the load started");
    }));
    assert!(unwound.is_err());
    // At least one `buck2 kill` per loop: a loop that was in a build then ends and kills its
    // own daemon too, so the count may be two; with no drop it is zero.
    assert!(
        kills(&c, 0) >= 1 && kills(&c, 1) >= 1,
        "{} {}",
        kills(&c, 0),
        kills(&c, 1)
    );
}

#[test]
fn a_load_that_was_joined_is_not_killed_again() {
    // Catches: the drop's kill running after a normal end as well, where each loop has
    // already killed its own daemon (a second `buck2 kill` per loop, and with a wedged daemon
    // a second wait on it). Mutant: the guard is armed after `join`.
    let c = stand_in_load("load-joined", "ok", 1, 1);
    let outcomes = Load::start(&c).join();
    assert!(!outcomes.is_empty());
    assert_eq!(kills(&c, 0), 1);
}

#[test]
fn a_load_that_is_stopped_starts_no_more_builds() {
    // Catches: `stop` that does not stop (the run that found a dead process would go on
    // loading a farm it already failed).
    let c = stand_in_load("load-stop", "ok", 1, 600);
    let load = Load::start(&c);
    load.stop();
    let until = Instant::now() + Duration::from_secs(60);
    while !load.finished() {
        assert!(Instant::now() < until, "the loop never ended");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(load.join().len() <= 1);
}
