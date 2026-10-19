//! A process's group, its death with the harness, and the harness's own exits. Some tests
//! run this test binary again as the process they need (`helper`, told what to be by
//! `KBF_HARNESS_HELPER`): a harness that is killed, or asked to stop, cannot be a test
//! thread.

use std::io::Write as _;
use std::process::Command;
use std::time::Duration;

use super::*;
use crate::alive;
use crate::farm::{Farm, Role};
use crate::interrupt::Interrupt;

/// The variable that tells the re-run test binary what to be, and the file it reports in.
const HELPER: &str = "KBF_HARNESS_HELPER";
const REPORT: &str = "KBF_HARNESS_REPORT";

/// Not a test: the process the tests below run again. Without [`HELPER`] it does nothing.
/// `deathsig` asks for the parent-death signal and says what it is set to; `orphan` starts `sleep` and exits, leaving it; `harness` starts a [`Proc`] running `sleep`
/// and waits to be killed; `session` makes itself the leader of a session and says which;
/// `reaped` is a harness over the stand-in `buck2`: a sentinel and a load of two loops, which
/// says where the project is and waits to be killed; `interruptible` supervises two `sleep`s until it is asked to stop,
/// then stops them (SIGTERM, as `kbf-beta` does); any other role is that one.
#[test]
fn helper() {
    let Ok(role) = std::env::var(HELPER) else {
        return;
    };
    let report = std::env::var(REPORT).expect("the report file");
    let say = |line: String| {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&report)
            .expect("the report");
        writeln!(file, "{line}").expect("report");
    };
    // The test's directory, not one of its own: a directory named for this process's id is a
    // dead process's once the harness is killed, and the sweep of any test that starts
    // removes it, with the records the sentinel is about to read.
    let dir = Path::new(&report)
        .parent()
        .expect("the report's directory")
        .to_owned();
    let sleeper = |name: &str| Spec {
        name: name.to_owned(),
        program: PathBuf::from("sleep"),
        argv: vec!["300".to_owned()],
        env: Vec::new(),
        cwd: dir.clone(),
    };
    match role.as_str() {
        "deathsig" => {
            die_with_the_harness().expect("prctl");
            let now = rustix::process::parent_process_death_signal().expect("prctl");
            say(format!("signal {now:?}"));
        }
        "session" => {
            crate::reap::own_session().expect("setsid");
            let sid = rustix::process::getsid(None).expect("getsid");
            say(format!(
                "session {} process {}",
                sid.as_raw_pid(),
                rustix::process::getpid().as_raw_pid()
            ));
        }
        "reaped" => {
            let (project, logs) = (dir.join("project"), dir.join("logs"));
            std::fs::create_dir_all(&project).expect("project");
            std::fs::create_dir_all(&logs).expect("logs");
            std::fs::write(project.join(".fake-buck2"), "ok").expect("mode");
            let cfg = crate::load::LoadConfig {
                buck2: crate::stand_in::sibling_bin(crate::stand_in::FAKE_BUCK2),
                project,
                logs,
                ca: PathBuf::from("ca.pem"),
                port: 1,
                concurrency: 2,
                duration: Duration::from_secs(600),
                build_timeout: Duration::from_secs(60),
                kill_timeout: Duration::from_secs(30),
                pause_secs: 1,
                fill_kib: 1,
                run_id: "reaped".to_owned(),
            };
            let reap = crate::reap::Reap::new(&crate::load::Daemons::of(&cfg), None, None);
            let sentinel = crate::reap::Sentinel::start(
                &crate::stand_in::sibling_bin("kbf-beta"),
                &reap,
                &dir.join("reaper.err"),
            )
            .expect("a sentinel");
            let load = crate::load::Load::start(&cfg);
            say(format!("project {}", cfg.project.display()));
            // Held until the process is killed.
            let _held = (sentinel, load);
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        "grouped" => {
            // A role whose process starts another (`find` starts `sleep`), and a sentinel that
            // knows the role's group from its file, over the stand-in `buck2`.
            let project = dir.join("project");
            std::fs::create_dir_all(&project).expect("project");
            let daemons = crate::load::Daemons {
                buck2: crate::stand_in::sibling_bin(crate::stand_in::FAKE_BUCK2),
                project,
                lanes: 1,
                kill_timeout: Duration::from_secs(30),
                run_id: "grouped".to_owned(),
            };
            let reap = crate::reap::Reap::new(&daemons, None, Some(dir.clone()));
            let sentinel = crate::reap::Sentinel::start(
                &crate::stand_in::sibling_bin("kbf-beta"),
                &reap,
                &dir.join("reaper.err"),
            )
            .expect("a sentinel");
            let proc = Proc::start(
                &spec(
                    "find",
                    "find",
                    &["/dev/null", "-exec", "sleep", "300", ";"],
                    &dir,
                ),
                &dir,
                0,
            );
            say(format!("records {}", dir.display()));
            say(format!("child {}", proc.pid()));
            let _held = (sentinel, proc);
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        "orphan" => {
            let child = Command::new("sleep")
                .arg("300")
                .stdout(Stdio::null())
                .spawn()
                .expect("sleep");
            say(format!("child {}", child.id()));
            // Left running and never waited for: that is the point of this role.
            std::mem::forget(child);
        }
        "harness" => {
            let proc = Proc::start(&sleeper("sleeper"), &dir, 0);
            say(format!("child {}", proc.pid()));
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        _ => {
            crate::interrupt::install();
            let mut farm = Farm::supervise(
                &dir,
                vec![
                    (Role::Node(1), sleeper("node-1")),
                    (Role::Node(2), sleeper("node-2")),
                ],
            );
            for pid in farm.process_table().iter().map(|p| p.pid) {
                say(format!("child {pid}"));
            }
            let stop = Interrupt::process();
            while !stop.is_set() {
                std::thread::sleep(Duration::from_millis(20));
            }
            say(format!("stopped {:?}", farm.stop_all()));
        }
    }
}

/// This test binary run as `role`, reporting in `report`.
fn helper_process(role: &str, report: &Path) -> Command {
    let helper = helper_spec(role, report, Path::new("."));
    let mut command = Command::new(&helper.program);
    command
        .args(&helper.argv)
        .envs(helper.env.iter().map(|(k, v)| (k, v)))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// [`helper_process`] as a [`Spec`], to run under [`Proc`].
fn helper_spec(role: &str, report: &Path, cwd: &Path) -> Spec {
    Spec {
        name: "helper".to_owned(),
        program: std::env::current_exe().expect("the test binary"),
        argv: ["--exact", "procs::tests::helper", "--nocapture"]
            .map(str::to_owned)
            .to_vec(),
        env: vec![
            (HELPER.to_owned(), role.to_owned()),
            (REPORT.to_owned(), report.display().to_string()),
        ],
        cwd: cwd.to_owned(),
    }
}

/// Calls `found` until it gives something; a bound that ends a hang only.
fn until<T>(what: &str, mut found: impl FnMut() -> Option<T>) -> T {
    let deadline = after(Duration::from_secs(60));
    loop {
        if let Some(found) = found() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The process ids the report lists, as `child <pid>` lines, once there are `n`.
fn children(report: &Path, n: usize) -> Vec<u32> {
    until("the helper to report its children", || {
        let text = std::fs::read_to_string(report).unwrap_or_default();
        let pids: Vec<u32> = text
            .lines()
            .filter_map(|l| l.strip_prefix("child ")?.parse().ok())
            .collect();
        (pids.len() >= n).then_some(pids)
    })
}

/// Ends `pids` if they run: a test that failed must not leave `sleep 300` behind.
struct Cleanup(Vec<u32>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for pid in &self.0 {
            if let Some(pid) = Pid::from_raw(i32::try_from(*pid).expect("a pid")) {
                let _ = rustix::process::kill_process(pid, Signal::KILL);
            }
        }
    }
}

fn spec(name: &str, program: &str, argv: &[&str], cwd: &Path) -> Spec {
    Spec {
        name: name.to_owned(),
        program: PathBuf::from(program),
        argv: argv.iter().map(|a| (*a).to_owned()).collect(),
        env: Vec::new(),
        cwd: cwd.to_owned(),
    }
}

#[test]
fn ending_a_process_ends_what_it_started() {
    // Catches: a role killed while the actions of its native driver (its children) run on,
    // alone, in the dark: the frontend, nodes and daemons of a run that ended would
    // leave their own children behind. `find` starts `sleep` and stays; killing `find`
    // must kill `sleep`. Mutant: `kill` signals the process alone, not its group.
    let d = crate::scratch("group");
    let mut proc = Proc::start(
        &spec(
            "find",
            "find",
            &["/dev/null", "-exec", "sleep", "300", ";"],
            &d,
        ),
        &d,
        0,
    );
    let child = until("find to start sleep", || {
        std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
            let after_name = stat.rsplit_once(')')?.1;
            let ppid: u32 = after_name.split_whitespace().nth(1)?.parse().ok()?;
            (ppid == proc.pid() && stat.contains("(sleep)")).then_some(pid)
        })
    });
    let _cleanup = Cleanup(vec![child]);
    proc.kill();
    until("the grandchild to end", || (!alive(child)).then_some(()));
}

#[test]
fn a_process_that_exits_takes_what_it_left_running_with_it() {
    // Catches: a role that exited (or crashed) on its own leaving its children running, to
    // hold a port or a lease directory of the next run. The helper starts `sleep` and
    // exits; the supervisor sweeps the group when it finds the leader ended. Mutant:
    // `exited` does not sweep.
    let d = crate::scratch("sweep");
    let report = d.join("report");
    let mut proc = Proc::start(&helper_spec("orphan", &report, &d), &d, 0);
    let child = children(&report, 1)[0];
    let _cleanup = Cleanup(vec![child]);
    until("the helper to end", || proc.exited());
    until("its child to be swept", || (!alive(child)).then_some(()));
}

#[test]
fn a_harness_that_is_killed_leaves_no_process_running() {
    // Catches: a CI job's cancel or timeout (SIGKILL after the grace period) leaving the
    // frontend, nodes and daemons of the run behind, each holding its ports into the
    // next job on the runner. The helper is a harness holding one `sleep`; it is killed
    // without a chance to run anything. Mutant: no parent-death signal in `Proc::start`.
    let d = crate::scratch("pdeathsig");
    let report = d.join("report");
    let mut harness = helper_process("harness", &report)
        .spawn()
        .expect("the helper");
    let child = children(&report, 1)[0];
    let _cleanup = Cleanup(vec![child]);
    assert!(alive(child));
    harness.kill().expect("kill the harness");
    harness.wait().expect("reaped");
    until("the sleeper to die with its harness", || {
        (!alive(child)).then_some(())
    });
}

#[test]
fn a_term_signal_stops_the_farm_through_the_harnesss_own_exit() {
    // Catches: a harness that dies of SIGTERM at once (the default), skipping the stop, the
    // result document and the kill of the Buck2 daemons: with the handler installed it sets
    // the flag its waits look at, and the run stops its processes (stop_all, which reports
    // them) and exits by itself. Mutant: `install` installs nothing.
    let d = crate::scratch("sigterm");
    let report = d.join("report");
    let mut harness = helper_process("interruptible", &report)
        .spawn()
        .expect("the helper");
    let kids = children(&report, 2);
    let _cleanup = Cleanup(kids.clone());
    let pid = Pid::from_child(&harness);
    rustix::process::kill_process(pid, Signal::TERM).expect("SIGTERM");
    let status = until("the helper to end", || harness.try_wait().expect("wait"));
    assert!(status.success(), "{status}");
    let text = std::fs::read_to_string(&report).expect("the report");
    assert!(
        text.contains("stopped [(Node(2), \"signal 15\"), (Node(1), \"signal 15\")]"),
        "{text}"
    );
    for kid in kids {
        assert!(!alive(kid));
    }
}

#[test]
fn a_process_that_has_ended_is_reported_as_it_ended_and_not_signalled_again() {
    // Catches: SIGTERM sent to a process id whose process has been reaped (it may be
    // someone else's by then), and a stop that panics on a process that is already gone.
    let d = crate::scratch("ended");
    let mut proc = Proc::start(&spec("true", "true", &[], &d), &d, 0);
    let status = until("true to end", || proc.exited());
    assert!(status.success());
    assert_eq!(proc.exited(), Some(status), "the status is kept");
    assert_eq!(proc.term_within(Duration::from_secs(60)), Some(status));
    // A kill of what has ended is nothing to do.
    proc.kill();
    assert_eq!(proc.exited(), Some(status));
}

#[test]
fn an_instant_too_far_away_is_far_away_not_a_panic() {
    // Catches: `Instant::now() + d` overflowing on a bound a command line made enormous.
    let far = after(Duration::MAX);
    assert!(far > after(Duration::from_secs(365 * 24 * 3600)));
    assert!(after(Duration::ZERO) <= Instant::now());
}

#[test]
fn the_child_asks_for_sigkill_when_its_harness_goes() {
    // Catches: a child started without the parent-death signal, or with another signal than
    // the one that cannot be ignored. The function the child runs between fork and exec is
    // run in a process of its own (the child's counters are lost at exec) and the process
    // says what the kernel then holds. Mutant: the function asks for nothing.
    let d = crate::scratch("deathsig");
    let report = d.join("report");
    let status = helper_process("deathsig", &report)
        .status()
        .expect("the helper");
    assert!(status.success());
    let text = std::fs::read_to_string(&report).expect("the report");
    assert_eq!(text.trim(), format!("signal {:?}", Some(Signal::KILL)));
}

#[test]
fn the_sentinel_leads_a_session_of_its_own() {
    // Catches: a sentinel in the harness's session and group, which a signal to the harness's
    // group (a terminal's interrupt, a runner's kill of the job's group) takes with it before
    // it has cleaned up. The function the sentinel runs between fork and exec is run in a
    // process of its own (the child's counters are lost at exec), which says its session.
    // Mutant: the function does nothing.
    let d = crate::scratch("session");
    let report = d.join("report");
    let status = helper_process("session", &report)
        .status()
        .expect("the helper");
    assert!(status.success());
    let text = std::fs::read_to_string(&report).expect("the report");
    let numbers: Vec<&str> = text.split_whitespace().collect();
    assert_eq!(numbers.len(), 4, "{text}");
    assert_eq!(numbers[1], numbers[3], "session leader: {text}");
}

/// A helper that is killed (and reaped) when the test ends, however it ends: a test that
/// failed must not leave a harness running its load.
struct Killed(std::process::Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_harness_killed_by_sigkill_leaves_no_buck2_daemon_running() {
    // Catches: a run killed with no chance to run a `Drop` (a runner cancelling the job)
    // leaving the Buck2 daemon of every loop behind: each leads a session of its own, so the
    // parent-death signal does not reach it. The helper is a harness holding a sentinel and a
    // load of two loops over the stand-in `buck2`, which notes every `kill` it is given;
    // nothing is killed while the harness lives, and after its SIGKILL the sentinel kills both
    // daemons. Mutants: no sentinel is started; the sentinel's input is not a pipe the
    // harness holds (the sentinel then cleans up at once, and the first check fails).
    let d = crate::scratch("reaped");
    let report = d.join("report");
    let mut harness = Killed(
        helper_process("reaped", &report)
            .spawn()
            .expect("the helper"),
    );
    let project = until("the helper to start its load", || {
        let text = std::fs::read_to_string(&report).unwrap_or_default();
        text.lines()
            .find_map(|l| l.strip_prefix("project ").map(PathBuf::from))
    });
    let marker = |lane: usize| {
        project.join(format!(
            ".fake-buck2-killed-{}",
            crate::load::isolation("reaped", lane)
        ))
    };
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !marker(0).exists() && !marker(1).exists(),
        "killed too soon"
    );
    harness.0.kill().expect("kill the harness");
    harness.0.wait().expect("reaped");
    until("both daemons to be killed", || {
        (marker(0).exists() && marker(1).exists()).then_some(())
    });
}

/// The `sleep` that `leader` started (its child), once there is one.
fn child_sleep_of(leader: u32) -> u32 {
    until("the leader to start sleep", || {
        std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
            let ppid: u32 = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            (ppid == leader && stat.contains("(sleep)")).then_some(pid)
        })
    })
}

#[test]
fn a_harness_killed_by_sigkill_takes_what_its_roles_started_with_it() {
    // Catches: the grandchildren of a role (the actions of a native driver, in the role's
    // process group) surviving a SIGKILL of the harness: the kernel's parent-death signal
    // reaches the role only, and without the sentinel's kill of the recorded groups the
    // `sleep` that `find` started runs on, holding the runner. The helper is a harness with
    // a sentinel and a role (`find`) that started `sleep`; the harness is killed with no
    // chance to run anything. Mutant: the sentinel does not kill the recorded groups
    // (`kill_groups` is not called).
    let d = crate::scratch("grouped");
    let report = d.join("report");
    let mut harness = Killed(
        helper_process("grouped", &report)
            .spawn()
            .expect("the helper"),
    );
    let find = children(&report, 1)[0];
    let sleeper = child_sleep_of(find);
    let _cleanup = Cleanup(vec![find, sleeper]);
    assert!(alive(sleeper));
    harness.0.kill().expect("kill the harness");
    harness.0.wait().expect("reaped");
    until("the grandchild to die with the harness", || {
        (!alive(sleeper)).then_some(())
    });
}

#[test]
fn what_a_killed_harness_recorded_survives_the_sweep_of_a_test_that_starts() {
    // Catches: the records the sentinel acts on (the groups of the roles) lying in a
    // directory named for the harness's own process id. The harness is killed; its id is
    // then a dead process's, and the first `scratch` of any test that starts (the CI runs
    // thousands, each in a process of its own) removes the directories of dead ones, the
    // records with them, in the moment before the sentinel reads them: the grandchild then
    // runs on. Here the sweep is run by hand between the kill and the check, which makes
    // the race a certainty. Mutant: the helper makes its directory with `scratch` (named
    // for its own process id) instead of using its test's.
    let d = crate::scratch("sweep-race");
    let report = d.join("report");
    let mut harness = Killed(
        helper_process("grouped", &report)
            .spawn()
            .expect("the helper"),
    );
    let find = children(&report, 1)[0];
    let _cleanup = Cleanup(vec![find, child_sleep_of(find)]);
    let records = until("the helper to say where its records are", || {
        let text = std::fs::read_to_string(&report).unwrap_or_default();
        text.lines()
            .find_map(|l| l.strip_prefix("records ").map(PathBuf::from))
    });
    harness.0.kill().expect("kill the harness");
    harness.0.wait().expect("reaped");
    crate::sweep_dead(d.parent().expect("the root"), &crate::alive);
    assert!(
        pgid_file(&records, "find", 0).exists(),
        "the group record of a killed harness was swept with the directory of its process"
    );
}

#[test]
fn a_process_records_its_group_while_it_runs_and_not_after() {
    // Catches: a role with no record (the sentinel cannot find its group), and a record left
    // after the group was swept (a recycled group id the sentinel would then kill).
    // Mutants: no file written; `sweep` leaves the file.
    let d = crate::scratch("pgid-file");
    let mut proc = Proc::start(&spec("rec", "sleep", &["300"], &d), &d, 0);
    let file = pgid_file(&d, "rec", 0);
    assert_eq!(
        std::fs::read_to_string(&file).expect("recorded").trim(),
        proc.pid().to_string()
    );
    assert_ne!(
        pgid_file(&d, "rec", 1),
        file,
        "a restart has a file of its own"
    );
    proc.kill();
    assert!(!file.exists());
}

#[test]
fn a_group_that_cannot_be_recorded_is_said_and_the_process_still_starts() {
    // Catches: a role that cannot write its record (a full or read-only logs directory)
    // failing silently, or taking the harness down: the record is best effort, because the
    // parent-death signal still kills the role, and the failure goes to stderr. Mutant: the
    // `eprintln!` removed; the error `unwrap`ped.
    let file = Path::new("/proc/no/such/dir/x.pgid");
    write_pgid(file, 7);
    assert!(!file.exists());
}
