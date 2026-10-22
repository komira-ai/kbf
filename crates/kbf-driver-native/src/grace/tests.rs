use std::cell::RefCell;

use super::*;

/// A fake machine: `alive` pids, of which `stubborn` ignore SIGTERM, a clock that moves
/// 10 ms per pause, and the calls in order.
struct World {
    alive: RefCell<Vec<i32>>,
    stubborn: Vec<i32>,
    now: RefCell<Duration>,
    calls: RefCell<Vec<&'static str>>,
}

impl World {
    fn new(alive: &[i32], stubborn: &[i32]) -> Self {
        Self {
            alive: RefCell::new(alive.to_vec()),
            stubborn: stubborn.to_vec(),
            now: RefCell::new(Duration::ZERO),
            calls: RefCell::default(),
        }
    }

    fn run(&self, grace: Duration) -> Result<Vec<Survivor>, String> {
        term_first(
            grace,
            &mut || {
                self.calls.borrow_mut().push("term");
                self.alive
                    .borrow_mut()
                    .retain(|p| self.stubborn.contains(p));
                Ok(())
            },
            &mut || {
                self.calls.borrow_mut().push("live");
                Ok(self.alive.borrow().clone())
            },
            &mut |pid| Survivor::of(pid, Some((format!("proc{pid}"), "--flag".to_owned()))),
            &mut || *self.now.borrow(),
            &mut || {
                self.calls.borrow_mut().push("pause");
                *self.now.borrow_mut() += Duration::from_millis(10);
            },
        )
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.borrow().clone()
    }
}

fn survivor(pid: i32, args: &str) -> Survivor {
    Survivor {
        pid,
        name: "p".to_owned(),
        args: args.to_owned(),
    }
}

/// Catches: SIGKILL (the caller's next step) coming before SIGTERM, a grace that is not
/// waited for, and a wait that goes on after the last process ended. SIGTERM is sent
/// once, after a look that found something to end, then the processes are looked at
/// until none is left.
#[test]
fn sigterm_comes_first_and_the_wait_ends_when_the_last_process_has() {
    let world = World::new(&[10, 11], &[]);
    let survivors = world.run(Duration::from_secs(2)).expect("ran");
    assert_eq!(survivors, []);
    assert_eq!(world.calls(), ["live", "term", "live"]);
}

/// Catches: a SIGTERM sent to a lease that has nothing running (a signal at a pid the
/// kernel may have handed on), and a pause before the first look.
#[test]
fn nothing_alive_sends_nothing() {
    let world = World::new(&[], &[]);
    assert_eq!(world.run(Duration::from_secs(2)), Ok(Vec::new()));
    assert_eq!(world.calls(), ["live"]);
}

/// Catches: a zero grace that still sends SIGTERM or waits, so that `0` would not mean
/// SIGKILL at once.
#[test]
fn a_grace_of_zero_touches_nothing() {
    let world = World::new(&[10], &[10]);
    assert_eq!(world.run(Duration::ZERO), Ok(Vec::new()));
    assert_eq!(world.calls(), [] as [&str; 0]);
}

/// Catches: a process that ended on SIGTERM being reported, one that ignored it being
/// left out, and a survivor named wrongly (pid, name and arguments come from the
/// description taken at the end of the grace).
#[test]
fn only_the_processes_that_ignored_sigterm_are_reported() {
    let world = World::new(&[10, 11, 12], &[11]);
    let survivors = world.run(Duration::from_millis(50)).expect("ran");
    assert_eq!(
        survivors,
        [Survivor {
            pid: 11,
            name: "proc11".to_owned(),
            args: "--flag".to_owned()
        }]
    );
}

/// Catches: a wait without a bound (a process that never ends would hold the lease
/// forever) and one that overshoots: with a 100 ms grace and a 10 ms pause the wait is
/// ten pauses, not more.
#[test]
fn the_grace_is_bounded() {
    let world = World::new(&[10], &[10]);
    let survivors = world.run(Duration::from_millis(100)).expect("ran");
    assert_eq!(survivors.len(), 1);
    let pauses = world.calls().iter().filter(|c| **c == "pause").count();
    assert_eq!(pauses, 10);
    assert_eq!(*world.now.borrow(), Duration::from_millis(100));
}

/// Catches: a failed SIGTERM round or process-table read treated as success, which
/// would go on to kill with nobody told.
#[test]
fn failures_are_returned() {
    let mut zero = Duration::ZERO;
    let mut pause = || {};
    let mut describe = |pid| Survivor::of(pid, None);
    let mut elapsed = || zero;
    let failed = term_first(
        Duration::from_secs(1),
        &mut || Err("no signal".to_owned()),
        &mut || Ok(vec![1]),
        &mut describe,
        &mut elapsed,
        &mut pause,
    );
    assert_eq!(failed, Err("no signal".to_owned()));
    let mut elapsed = || {
        zero += Duration::from_secs(1);
        zero
    };
    let mut describe = |pid| Survivor::of(pid, None);
    let mut pause = || {};
    let unreadable = term_first(
        Duration::from_secs(1),
        &mut || Ok(()),
        &mut || Err("table".to_owned()),
        &mut describe,
        &mut elapsed,
        &mut pause,
    );
    assert_eq!(unreadable, Err("table".to_owned()));
}

/// Catches: a process that is gone by the time it is described getting no name at all
/// (an empty line in the note), and a name or arguments taken from the wrong place.
#[test]
fn a_survivor_that_cannot_be_read_is_named_with_a_question_mark() {
    assert_eq!(
        Survivor::of(5, None),
        Survivor {
            pid: 5,
            name: "?".to_owned(),
            args: String::new()
        }
    );
    assert_eq!(
        Survivor::of(5, Some(("sleep".to_owned(), "sleep 9".to_owned()))).args,
        "sleep 9"
    );
}

/// Catches: no line for no survivors, the count not matching the survivors, the
/// pid/name/arguments missing from a line, arguments not cut at the limit, and a note
/// that grows with the number of survivors (more than the cap are only counted).
#[test]
fn the_note_names_survivors_cut_and_capped() {
    assert_eq!(note(&[]), None);
    let one = note(&[survivor(7, "sleep 100")]).expect("a note");
    assert_eq!(
        one,
        "kbf: 1 process(es) outlasted SIGTERM and were killed: pid 7 p [sleep 100]\n"
    );
    let long = "x".repeat(ARGS_SHOWN + 5);
    let cut = note(&[survivor(7, &long)]).expect("a note");
    assert!(
        cut.contains(&format!("[{}...]", "x".repeat(ARGS_SHOWN))),
        "{cut}"
    );
    let exact = note(&[survivor(7, &"y".repeat(ARGS_SHOWN))]).expect("a note");
    assert!(!exact.contains("..."), "{exact}");
    let many: Vec<Survivor> = (1..=11).map(|pid| survivor(pid, "")).collect();
    let capped = note(&many).expect("a note");
    assert!(capped.starts_with("kbf: 11 process(es)"), "{capped}");
    assert!(capped.contains("pid 8 p"), "{capped}");
    assert!(!capped.contains("pid 9 p"), "{capped}");
    assert!(capped.trim_end().ends_with("; and 3 more"), "{capped}");
}

/// Catches: the arguments of thousands of survivors being read (each read is costly) when
/// only eight are named, and a count that stops matching: every survivor is returned,
/// only the first eight are described.
#[test]
fn only_the_survivors_that_are_named_are_described() {
    let pids: Vec<i32> = (1..=12).collect();
    let world = World::new(&pids, &pids);
    let described = RefCell::new(Vec::new());
    let survivors = term_first(
        Duration::from_millis(20),
        &mut || Ok(()),
        &mut || Ok(world.alive.borrow().clone()),
        &mut |pid| {
            described.borrow_mut().push(pid);
            Survivor::of(pid, Some(("p".to_owned(), String::new())))
        },
        &mut || *world.now.borrow(),
        &mut || *world.now.borrow_mut() += Duration::from_millis(10),
    )
    .expect("ran");
    assert_eq!(survivors.len(), 12);
    assert_eq!(*described.borrow(), (1..=8).collect::<Vec<i32>>());
}

/// Catches: a failed SIGTERM round skipping the kill that follows (the caller kills
/// whatever `lenient` hands back), and a survivor list invented from a failure.
#[test]
fn a_failed_round_names_nobody_and_lets_the_kill_run() {
    assert_eq!(lenient(Err("fork".to_owned()), "test"), []);
    assert_eq!(
        lenient(Ok(vec![survivor(3, "")]), "test"),
        [survivor(3, "")]
    );
}

/// Catches: an argument with a line break forging a line in the lease's stderr or the
/// log, and the log line carrying arguments (they can hold a secret).
#[test]
fn control_characters_are_escaped_and_the_log_line_has_no_arguments() {
    let forged = note(&[survivor(7, "a\nkbf: forged\u{1b}[0m")]).expect("a note");
    assert_eq!(forged.matches('\n').count(), 1, "{forged:?}");
    assert!(forged.contains("a\\nkbf: forged\\u{1b}[0m"), "{forged}");
    assert_eq!(brief(&[]), None);
    let log = brief(&[survivor(7, "--token=abc")]).expect("a line");
    assert_eq!(
        log,
        "1 process(es) outlasted SIGTERM and were killed: pid 7 p"
    );
}
