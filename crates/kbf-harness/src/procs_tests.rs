//! A process's group and its death with the harness. Some tests run this test binary again
//! as the process they need (`helper`, told what to be by `KBF_HARNESS_HELPER`): a harness
//! that is killed cannot be a test thread.

use std::io::Write as _;
use std::process::Command;
use std::time::Duration;

use super::*;
use crate::alive;

/// The variable that tells the re-run test binary what to be, and the file it reports in.
const HELPER: &str = "KBF_HARNESS_HELPER";
const REPORT: &str = "KBF_HARNESS_REPORT";

/// Not a test: the process the tests below run again. Without [`HELPER`] it does nothing.
/// `deathsig` asks for the parent-death signal and says what it is set to; `orphan` starts
/// `sleep` and exits, leaving it; any other role is a harness: it starts a [`Proc`] running
/// `sleep` and waits to be killed.
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
    // removes it.
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
        _ => {
            let proc = Proc::start(&sleeper("sleeper"), &dir, 0);
            say(format!("child {}", proc.pid()));
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
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
    // Catches: a role killed while what it started (its children) runs on, alone, in the
    // dark: the processes of a run that ended would leave their own children behind. `find` starts `sleep` and stays; killing `find`
    // must kill `sleep`. Catches: `kill` signals the process alone, not its group.
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
    // exits; the supervisor sweeps the group when it finds the leader ended. Catches:
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
    // frontend and nodes of the run behind, each holding its ports into the
    // next job on the runner. The helper is a harness holding one `sleep`; it is killed
    // without a chance to run anything. Catches: no parent-death signal in `Proc::start`.
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
    // says what the kernel then holds. Catches: the function asks for nothing.
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
fn log_hits_names_the_file_and_line_of_every_generation_and_stream() {
    // Catches: a scan of the current generation only (a restart's predecessor is where a
    // drop is logged), of stdout only, of lines that do not hold the needle, or of files
    // that are not logs (a `.txt`, a directory).
    // Catches: `.err` skipped; the generation filter; the needle test inverted.
    let d = crate::scratch("log-hits");
    let (out0, err0) = log_files(&d, "node-1", 0);
    let (out1, err1) = log_files(&d, "node-1", 1);
    std::fs::write(&out0, "fine\nWARN run dropped; cleaning up\n").expect("write");
    std::fs::write(&err0, "fine\n").expect("write");
    std::fs::write(&out1, "").expect("write");
    std::fs::write(&err1, b"run dropped again\xff\n").expect("write");
    std::fs::write(d.join("node-1.txt"), "run dropped\n").expect("write");
    std::fs::create_dir_all(d.join("node-1.out.d")).expect("dir");
    assert_eq!(
        log_hits(&d, "run dropped"),
        [
            "node-1.1.err: run dropped again\u{fffd}".to_owned(),
            "node-1.out: WARN run dropped; cleaning up".to_owned(),
        ]
    );
    assert!(log_hits(&d, "lease failed").is_empty());
    assert!(log_hits(&d.join("absent"), "x").is_empty());
}
