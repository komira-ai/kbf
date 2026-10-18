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
/// and waits to be killed; `interruptible` supervises two `sleep`s until it is asked to stop,
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
    let dir = crate::scratch(&format!("helper-{role}"));
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
    // alone, in the dark: the frontend, nodes, daemons and Envoy of a run that ended would
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
    // frontend, nodes, daemons and Envoy of the run behind, each holding its ports into the
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
