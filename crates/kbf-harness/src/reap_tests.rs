//! The sentinel: what it reads, what it cleans up, and the harness's end of it. That a
//! harness killed by SIGKILL has its daemons killed is `procs_tests`'s
//! `a_harness_killed_by_sigkill_leaves_no_buck2_daemon_running`, which needs a process to kill.

use std::io::Cursor;
use std::time::{Duration, Instant};

use super::*;
use crate::alive;
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
        },
        Some(socket),
    )
}

/// How many times the stand-in `buck2` was told to `kill` the daemon of loop `lane`.
fn kills(reap: &Reap, lane: usize) -> usize {
    std::fs::read_to_string(reap.project.join(format!(".fake-buck2-killed-beta-{lane}")))
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
    };
    let r = Reap::new(&daemons, None);
    assert_eq!(r.kill_timeout_ms, 2500);
    let back = r.daemons();
    assert_eq!(
        (back.buck2, back.project, back.lanes, back.kill_timeout),
        (
            daemons.buck2,
            daemons.project,
            4,
            Duration::from_millis(2500)
        )
    );
    // A bound too long for a u64 of milliseconds is as long as there is.
    let huge = Daemons {
        kill_timeout: Duration::MAX,
        ..r.daemons()
    };
    assert_eq!(Reap::new(&huge, None).kill_timeout_ms, u64::MAX);
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
