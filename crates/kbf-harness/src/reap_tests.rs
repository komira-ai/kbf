//! The sentinel: what it reads, what it cleans up, and the harness's end of it. That a
//! harness killed by SIGKILL has its daemons killed is `procs_tests`'s
//! `a_harness_killed_by_sigkill_leaves_no_buck2_daemon_running`, which needs a process to kill.

use std::io::Cursor;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use super::*;
use crate::alive;
use crate::load::isolation;
use crate::stand_in::{FAKE_BUCK2, sibling_bin};

/// The kbf-beta binary, which answers `reap`.
fn kbf_beta() -> PathBuf {
    sibling_bin("kbf-beta")
}

/// What to clean up after a harness with `lanes` loops over the stand-in `buck2`, in a fresh
/// project directory; and a control socket (a file) in it.
fn reap(name: &str, lanes: usize) -> Reap {
    let dir = crate::scratch(name);
    let project = dir.join("project");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::write(project.join(".fake-buck2"), "ok").expect("mode");
    let socket = dir.join("farm.sock");
    std::fs::write(&socket, "").expect("socket");
    Reap::new(
        &Daemons {
            buck2: sibling_bin(FAKE_BUCK2),
            project,
            lanes,
            kill_timeout: Duration::from_secs(30),
            // Starts with `-`, as an empty CI run id and the pid make one: it must reach the sentinel.
            run_id: "-7".to_owned(),
        },
        Some(socket),
        Some(dir),
    )
}

/// How many times the stand-in `buck2` was told to `kill` the daemon of loop `lane`.
fn kills(reap: &Reap, lane: usize) -> usize {
    std::fs::read_to_string(reap.project.join(format!(
        ".fake-buck2-killed-{}",
        isolation(&reap.run_id, lane)
    )))
    .map_or(0, |text| text.lines().count())
}

/// Calls `found` until it gives something; a bound that ends a hang only.
fn until<T>(what: &str, mut found: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(found) = found() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A reader that fails.
struct Broken;

impl std::io::Read for Broken {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("the pipe broke"))
    }
}

#[test]
fn the_sentinel_is_done_only_when_it_is_told_so() {
    // Catches: a sentinel that takes anything it reads for the all-clear (a harness's stray
    // line would then leave the daemons of a run that died), one that takes the end of the
    // input for it (the whole point), and one that does not see a `done` on the last line
    // with no newline, or after other lines. Mutants: `Ok(_)` is done; the end is done.
    let said = |text: &str| word(&mut Cursor::new(text.to_owned()));
    assert_eq!(said("done\n"), Word::Done);
    assert_eq!(said("done"), Word::Done);
    assert_eq!(said("noise\ndone\n"), Word::Done);
    assert_eq!(said(" done \n"), Word::Done);
    assert_eq!(said(""), Word::Lost);
    assert_eq!(said("noise\n"), Word::Lost);
    assert_eq!(said("not done\n"), Word::Lost);
    // An input that fails is an input that is gone.
    assert_eq!(
        word(&mut std::io::BufReader::new(Broken)),
        Word::Lost,
        "a read error is the end"
    );
}

#[test]
fn a_harness_that_died_has_its_daemons_killed_and_its_socket_removed() {
    // Catches: the cleanup that does not clean: a daemon per loop left running, holding the
    // farm's connection, and the control socket left in the temporary directory. Mutants:
    // the lost branch kills no daemon; it removes no socket; it kills one lane too few.
    let r = reap("reap-lost", 3);
    assert_eq!(r.run(&mut Cursor::new(String::new())), 1);
    for lane in 0..3 {
        assert_eq!(kills(&r, lane), 1, "lane {lane}");
    }
    assert!(!r.socket.as_ref().expect("a socket").exists());
}

#[test]
fn a_harness_that_ended_well_has_nothing_cleaned_up() {
    // Catches: a sentinel that kills the daemons of a run that ended well (the loops have
    // killed their own; a second kill waits on a daemon for nothing) or removes a socket
    // the next run may own by then. Mutant: the done branch cleans up as well.
    let r = reap("reap-done", 2);
    assert_eq!(r.run(&mut Cursor::new("done\n".to_owned())), 0);
    assert_eq!((kills(&r, 0), kills(&r, 1)), (0, 0));
    assert!(r.socket.as_ref().expect("a socket").exists());
}

#[test]
fn a_cleanup_without_a_socket_kills_the_daemons_alone() {
    // Catches: a missing socket (a run that was killed before its frontend started) being an
    // error that stops the cleanup before, or instead of, the kills.
    let mut r = reap("reap-no-socket", 1);
    r.socket = None;
    assert_eq!(r.run(&mut Cursor::new(String::new())), 1);
    assert_eq!(kills(&r, 0), 1);
}

#[test]
fn the_cleanup_is_the_daemons_it_was_made_from() {
    // Catches: a Duration that does not survive the command line (the kill bound becoming
    // zero seconds would give up on every daemon at once), a lane count or a path that does
    // not. Mutant: the bound is read as seconds.
    let daemons = Daemons {
        buck2: PathBuf::from("/b/buck2"),
        project: PathBuf::from("/p"),
        lanes: 4,
        kill_timeout: Duration::from_millis(2500),
        run_id: "-9-1".to_owned(),
    };
    let r = Reap::new(&daemons, None, None);
    assert_eq!(r.kill_timeout_ms, 2500);
    let back = r.daemons();
    assert_eq!(
        (
            back.buck2,
            back.project,
            back.lanes,
            back.kill_timeout,
            back.run_id
        ),
        (
            daemons.buck2,
            daemons.project,
            4,
            Duration::from_millis(2500),
            "-9-1".to_owned()
        )
    );
    // A bound too long for a u64 of milliseconds is as long as there is.
    let huge = Daemons {
        kill_timeout: Duration::MAX,
        ..r.daemons()
    };
    assert_eq!(Reap::new(&huge, None, None).kill_timeout_ms, u64::MAX);
}

#[test]
fn the_command_line_names_the_socket_only_when_there_is_one() {
    // Catches: `--socket` given with no value (the sentinel then refuses its command line and
    // a killed harness is not cleaned up).
    let mut r = reap("reap-args", 2);
    let with = r.args();
    assert_eq!(
        with.first().map(|a| a.to_string_lossy().into_owned()),
        Some("reap".to_owned())
    );
    assert!(with.iter().any(|a| a == "--socket"));
    r.socket = None;
    assert!(!r.args().iter().any(|a| a == "--socket"));
}

#[test]
fn a_sentinel_that_is_dropped_is_told_done_and_leaves_everything_alone() {
    // Catches: a harness that ends well while its sentinel kills the daemons of a finished run
    // (the pipe closed with no `done`), or that leaves the sentinel running for ever. The
    // sentinel is a real process, the kbf-beta binary. Mutant: `Drop` closes the pipe and
    // says nothing.
    let r = reap("sentinel-done", 2);
    let sentinel =
        Sentinel::start(&kbf_beta(), &r, &r.project.join("reaper.err")).expect("a sentinel");
    let pid = sentinel.pid();
    assert!(alive(pid));
    drop(sentinel);
    assert!(!alive(pid), "the sentinel ended with the harness's drop");
    assert_eq!((kills(&r, 0), kills(&r, 1)), (0, 0));
    assert!(r.socket.as_ref().expect("a socket").exists());
}

#[test]
fn a_sentinel_whose_pipe_closes_with_no_done_cleans_up() {
    // Catches: a sentinel that is not wired to its input (a pipe that is not its standard
    // input, or a standard input it never reads): the harness dies, the kernel closes the pipe,
    // and nothing happens. Here the pipe is closed by hand, which is what the kernel does.
    // Mutants: the sentinel is started with no input (it cleans up at once, so the "nothing
    // yet" check fails); its input is never read.
    let r = reap("sentinel-lost", 2);
    let mut sentinel =
        Sentinel::start(&kbf_beta(), &r, &r.project.join("reaper.err")).expect("a sentinel");
    let pid = sentinel.pid();
    // It waits while the pipe is open.
    assert!(alive(pid));
    assert_eq!((kills(&r, 0), kills(&r, 1)), (0, 0));
    drop(sentinel.input.take());
    until("the daemons to be killed", || {
        (kills(&r, 0) == 1 && kills(&r, 1) == 1).then_some(())
    });
    until("the socket to be removed", || {
        (!r.socket.as_ref().expect("a socket").exists()).then_some(())
    });
    until("the sentinel to end", || (!alive(pid)).then_some(()));
}

#[test]
fn a_sentinel_that_cannot_be_started_says_so() {
    // Catches: a run that goes on with no sentinel because the start error was swallowed: it
    // would promise a cleanup it cannot do. A program that is not there, and a log that
    // cannot be made.
    let r = reap("sentinel-fails", 1);
    let log = r.project.join("reaper.err");
    let gone = Sentinel::start(Path::new("/nonexistent/kbf-beta"), &r, &log);
    assert!(gone.is_err());
    let no_log = Sentinel::start(&kbf_beta(), &r, &r.project.join("no/such/dir/reaper.err"));
    assert!(no_log.is_err());
}

/// A `sleep` leading a group of its own: the group id it has.
fn grouped_sleeper() -> Child {
    Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .expect("sleep")
}

#[test]
fn the_groups_the_files_name_are_killed_and_nothing_else_is() {
    // Catches: a sentinel that leaves the grandchildren of a killed harness's roles running
    // (the kernel kills a role, not what it started), and one that signals what a file
    // should not make it signal: a number that is not a group (0 and 1), text, a file that
    // is not a `.pgid` (the `leave.txt` names a live group that must survive). Mutants:
    // `kill_groups` does nothing (the named group survives); the extension filter removed
    // (`leave.txt` is killed).
    let dir = crate::scratch("kill-groups");
    let (mut named, mut other) = (grouped_sleeper(), grouped_sleeper());
    std::fs::write(dir.join("a.pgid"), format!("{}\n", named.id())).expect("pgid");
    std::fs::write(dir.join("b.pgid"), "0\n").expect("pgid");
    std::fs::write(dir.join("c.pgid"), "1\n").expect("pgid");
    std::fs::write(dir.join("d.pgid"), "not a number").expect("pgid");
    std::fs::write(dir.join("leave.txt"), format!("{}\n", other.id())).expect("txt");
    kill_groups(&dir);
    let status = until("the named group to end", || named.try_wait().expect("wait"));
    assert!(!status.success());
    assert!(other.try_wait().expect("wait").is_none());
    other.kill().expect("kill");
    other.wait().expect("reaped");
    // A directory that is not there is no error.
    kill_groups(&dir.join("absent"));
}

#[test]
fn a_group_named_by_a_file_is_killed_with_the_children_in_it() {
    // Catches: a sentinel that signals the process a file names and not its group: a role's
    // children (the actions of a native driver) are in the role's group but are not its
    // leader, and they would be left running after the harness died. The role leads a group
    // of its own; the child is started into that group (`process_group(role)`, as a fork of
    // the role is) and leads none. Mutant: `kill_groups` kills the pid the file names
    // (`kill_process`), not the group (the child survives).
    let dir = crate::scratch("kill-groups-child");
    let mut role = grouped_sleeper();
    let mut child = Command::new("sleep")
        .arg("300")
        .process_group(i32::try_from(role.id()).expect("a pid"))
        .spawn()
        .expect("sleep");
    let child_pid = child.id();
    assert!(alive(child_pid));
    std::fs::write(dir.join("role.pgid"), format!("{}\n", role.id())).expect("pgid");
    kill_groups(&dir);
    let status = until("the role to end", || role.try_wait().expect("wait"));
    assert!(!status.success());
    let status = until("the role's child to end", || {
        child.try_wait().expect("wait")
    });
    assert!(!status.success());
}

#[test]
fn the_command_line_names_the_group_directory_only_when_there_is_one() {
    // Catches: `--pgids` lost on the way to the sentinel (a killed harness's grandchildren
    // survive again), or given with no value. Mutant: `args` leaves it out.
    let mut r = reap("reap-pgids-args", 1);
    let args: Vec<String> = r
        .args()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let at = args.iter().position(|a| a == "--pgids").expect("--pgids");
    assert_eq!(
        args[at + 1],
        r.pgids.as_ref().expect("a directory").display().to_string()
    );
    r.pgids = None;
    assert!(!r.args().iter().any(|a| a == "--pgids"));
}
