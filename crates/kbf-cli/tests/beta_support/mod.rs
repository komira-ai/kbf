//! What the tests of scenario B0 over a real farm share (tests/beta_b0.rs, the scenarios of the
//! healthy and the broken run, and tests/beta_b0_red.rs, the ones that turn each gate red): a
//! queue for the farms that run at once, a fresh directory, the `B0` arguments over the stand-in
//! `buck2`, the result document's readers, and the helpers that act on a running farm from
//! outside (find a process, stop it, post to the operator API, scan the logs).

#![allow(dead_code)] // Each test binary uses its own part.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use kbf_harness::b0::B0;
use kbf_harness::interrupt::Interrupt;
use kbf_harness::stand_in::{FAKE_BUCK2, sibling_bin};

/// How many farms these tests run at once. Each is seven processes, and a test runs as many
/// at a time as the machine has CPUs: on a big machine, under coverage, a farm that is starved
/// of CPU outlasts its waits. The tests queue for a slot instead.
const FARMS_AT_ONCE: usize = 3;

static SLOTS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

/// A place for one farm; given back when dropped.
pub(crate) struct Slot;

pub(crate) fn take_slot() -> Slot {
    let (farms, free) = &SLOTS;
    let mut running = farms.lock().unwrap_or_else(PoisonError::into_inner);
    while *running >= FARMS_AT_ONCE {
        running = free.wait(running).unwrap_or_else(PoisonError::into_inner);
    }
    *running += 1;
    Slot
}

impl Drop for Slot {
    fn drop(&mut self) {
        let (farms, free) = &SLOTS;
        *farms.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        free.notify_one();
    }
}

thread_local! {
    /// The slot of the test running on this thread, held until the test ends.
    static HELD: RefCell<Option<Slot>> = const { RefCell::new(None) };
}

/// A fresh directory for one test.
pub(crate) fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-beta")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// B0 over the stand-in `buck2` that does `mode` in each build, for about `secs` seconds.
pub(crate) fn b0(name: &str, mode: &str, secs: u64) -> B0 {
    // A slot for this test's farm, kept until its thread ends; one a test held before (for a
    // second run) is given back first, so no test waits for a slot while holding one.
    HELD.with(|held| held.borrow_mut().take());
    let slot = take_slot();
    HELD.with(|held| *held.borrow_mut() = Some(slot));
    let dir = scratch(name);
    let project = dir.join("project");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::write(project.join(".fake-buck2"), mode).expect("mode");
    let fake = sibling_bin(FAKE_BUCK2);
    B0 {
        kbf: PathBuf::from(env!("CARGO_BIN_EXE_kbf")),
        // Any file: the stand-in build runs no action.
        act: fake.clone(),
        buck2: fake,
        project,
        work: dir.join("work"),
        duration: Duration::from_secs(secs),
        settle: Duration::from_secs(1),
        concurrency: 2,
        build_timeout: Duration::from_secs(60),
        kill_timeout: Duration::from_secs(30),
        wall_bound: Duration::from_secs(secs + 120),
        pause_secs: 1,
        fill_kib: 4,
        scrape_every: Duration::from_secs(1),
        summary: Some(dir.join("summary.md")),
        fault: None,
        extra_flags: Vec::new(),
        farm_hang: Duration::from_secs(60),
        interrupt: Interrupt::new(),
        reaper: None,
    }
}

/// Waits until the load of the run in `work` has started and its scraper has read a page: a
/// bounded wait, which guards against a run that never starts and asserts nothing about time.
pub(crate) fn wait_for_the_load(work: &Path) {
    let until = std::time::Instant::now() + Duration::from_secs(120);
    let series = work.join("logs/metrics.jsonl");
    while std::fs::read_to_string(&series).map_or(true, |text| text.lines().count() < 2) {
        assert!(std::time::Instant::now() < until, "the load never started");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The process of the run in `work` that runs in the directory `role` of its farm, found by
/// its working directory.
pub(crate) fn pid_of(work: &Path, role: &str) -> rustix::process::Pid {
    let dir = work.join("farm").join(role);
    let found = std::fs::read_dir("/proc")
        .expect("proc")
        .flatten()
        .find_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            (std::fs::read_link(entry.path().join("cwd")).ok()? == dir).then_some(pid)
        });
    rustix::process::Pid::from_raw(found.unwrap_or_else(|| panic!("{role} runs"))).expect("a pid")
}

/// SIGKILL to node-3 of the run in `work`, once the load has started.
pub(crate) fn kill_node_3_under_load(work: &Path) {
    wait_for_the_load(work);
    rustix::process::kill_process(pid_of(work, "node-3"), rustix::process::Signal::KILL)
        .expect("SIGKILL");
}

/// The result document of a run.
pub(crate) fn document(args: &B0) -> serde_json::Value {
    let text = std::fs::read_to_string(args.work.join("beta-result.json")).expect("the result");
    serde_json::from_str(&text).expect("JSON")
}

/// The marker files the stand-in `buck2` left for the kills of loop `lane`'s daemon in the run
/// in `work`: the daemon is `beta-<run id>-<lane>`, and the run id carries the harness's pid.
pub(crate) fn kill_markers(work: &Path, lane: usize) -> Vec<PathBuf> {
    let suffix = format!("-{lane}");
    std::fs::read_dir(work.join("project"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".fake-buck2-killed-beta-") && n.ends_with(&suffix))
        })
        .collect()
}

/// The verdict of gate `name` in `doc`.
pub(crate) fn verdict<'a>(doc: &'a serde_json::Value, name: &str) -> &'a str {
    let gate = doc["gates"]
        .as_array()
        .expect("gates")
        .iter()
        .find(|g| g["name"] == name);
    gate.unwrap_or_else(|| panic!("no gate {name} in {doc}"))["verdict"]
        .as_str()
        .expect("verdict")
}

/// The names of the gates that failed.
pub(crate) fn failed(doc: &serde_json::Value) -> Vec<String> {
    doc["gates"]
        .as_array()
        .expect("gates")
        .iter()
        .filter(|g| g["verdict"] == "FAIL")
        .map(|g| g["name"].as_str().expect("name").to_owned())
        .collect()
}

/// The REAPI and operator API ports the frontend of the run in `work` printed.
pub(crate) fn frontend_ports(work: &Path) -> (u16, u16) {
    let text = std::fs::read_to_string(work.join("logs/frontend.out")).expect("the start line");
    let line = text
        .lines()
        .find(|l| l.starts_with("kbf-frontend "))
        .expect("start line");
    let port = |key: &str| {
        let address = kbf_harness::procs::field(line, key);
        address
            .rsplit(':')
            .next()
            .expect("a port")
            .parse()
            .expect("a number")
    };
    (port("reapi="), port("api="))
}

/// `POST path` with the JSON `body` to the operator API at `api`, as an operator does (the
/// harness's token, and no `Origin`, which a browser's request would have); the status code.
pub(crate) fn post(api: u16, path: &str, body: &str) -> u16 {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", api)).expect("the API");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: kbf\r\nAuthorization: Bearer rehearsal-api-token-of-32-or-more-visible-chars\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write");
    let mut text = String::new();
    stream.read_to_string(&mut text).expect("read");
    text.split_whitespace()
        .nth(1)
        .expect("a status")
        .parse()
        .expect("a code")
}

/// Every file under `dir` that holds `needle`.
pub(crate) fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_containing(&path, needle));
        } else if std::fs::read(&path)
            .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
        {
            found.push(path);
        }
    }
    found
}

/// Catches: private keys of the farm in what the workflow uploads (`logs/` is kept for 90 days
/// by whoever can read the run), or readable by the other users of a runner: no file under the
/// logs holds a private key, every key file under the farm is mode 0600 and the directory of
/// each key 0700.
pub(crate) fn assert_keys_are_private(args: &B0) {
    use std::os::unix::fs::PermissionsExt as _;
    let logs = args.work.join("logs");
    assert_eq!(
        files_containing(&logs, "PRIVATE KEY"),
        Vec::<PathBuf>::new()
    );
    let mut keys = 0;
    for role in std::fs::read_dir(args.work.join("farm"))
        .expect("farm")
        .flatten()
    {
        let etc = role.path().join("etc");
        if !etc.is_dir() {
            continue;
        }
        let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        for entry in std::fs::read_dir(&etc).expect("etc").flatten() {
            if entry.path().extension().is_some_and(|e| e == "key") {
                keys += 1;
                assert_eq!(mode(&entry.path()), 0o600, "{}", entry.path().display());
                assert_eq!(mode(&etc), 0o700, "{}", etc.display());
            }
        }
    }
    assert!(keys >= 5, "{keys} keys: the scan looked at nothing");
}

/// Stops a process for as long as it lives, and continues it when dropped (a test that failed
/// must not leave the farm stopped).
pub(crate) struct Stopped(rustix::process::Pid);

impl Stopped {
    pub(crate) fn new(pid: rustix::process::Pid) -> Self {
        rustix::process::kill_process(pid, rustix::process::Signal::STOP).expect("SIGSTOP");
        Self(pid)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process(self.0, rustix::process::Signal::CONT);
    }
}

/// Every gate of B0 and the test (of tests/beta_b0.rs or tests/beta_b0_red.rs) that turns it red
/// in a scenario over a real farm.
/// `None` is a gate that only the real Buck2 can turn red: the stand-in's builds write nothing
/// to the farm, so no blob can be lost and no action runs on a daemon to fail. Those are
/// `kbf-beta b0 --fault shrink-every-cap`'s to show red, with the real Buck2 of the workflow, for
/// the lost-blob gates; `failed_actions_by_class` is turned red by the gate tests of kbf-harness
/// (a series fed to it) and its count by `kbf-server/tests/failed_actions_metric.rs`.
pub(crate) const RED_IN_A_SCENARIO: &[(&str, Option<&str>)] = &[
    (
        "builds_run",
        Some("a_load_that_runs_no_build_fails_the_builds_run_gate"),
    ),
    (
        "builds_failed",
        Some("each_kind_of_bad_build_fails_its_gate"),
    ),
    (
        "actions_failed",
        Some("each_kind_of_bad_build_fails_its_gate"),
    ),
    (
        "execute_errors",
        Some("an_execute_that_fails_fails_the_execute_error_gate"),
    ),
    ("failed_actions_by_class", None),
    ("lost_blobs_gauge", None),
    ("blob_losses", None),
    // No B0 scenario takes two members down or restarts one; the gate tests of kbf-harness
    // turn these red from series fed to them, and kbf-server/tests/storage_health.rs
    // pins the series they read.
    ("members_down_at_most_one", None),
    ("member_restarts", None),
    (
        "scrape_coverage",
        Some("a_frontend_that_stops_answering_fails_the_scrape_coverage_gate"),
    ),
    (
        "processes_alive",
        Some("a_node_killed_midway_fails_the_liveness_gate"),
    ),
    (
        "farm_whole",
        Some("a_member_that_is_drained_fails_the_whole_farm_gate_and_only_it"),
    ),
    (
        "clean_stop",
        Some("a_node_that_dies_on_its_own_fails_the_liveness_gate"),
    ),
    (
        "hang",
        Some("a_load_past_its_wall_bound_is_timed_out_with_a_report"),
    ),
];

/// Catches: a gate added to B0 that no scenario test turns red (a gate that never fails
/// is not a gate), and a name in [`RED_IN_A_SCENARIO`] that is no test of this file. The gates
/// are those a healthy run writes.
pub(crate) fn assert_every_gate_has_a_scenario(doc: &serde_json::Value) {
    let source = [
        include_str!("../beta_b0.rs"),
        include_str!("../beta_b0_red.rs"),
    ]
    .join("\n");
    let gates: Vec<&str> = doc["gates"]
        .as_array()
        .expect("gates")
        .iter()
        .map(|g| g["name"].as_str().expect("name"))
        .collect();
    let listed: Vec<&str> = RED_IN_A_SCENARIO.iter().map(|(gate, _)| *gate).collect();
    assert_eq!(
        gates, listed,
        "a gate without a scenario test, or one that is gone"
    );
    for (gate, test) in RED_IN_A_SCENARIO {
        if let Some(test) = test {
            assert!(
                source.contains(&format!("fn {test}()")),
                "{gate}: no test {test}"
            );
        }
    }
}
