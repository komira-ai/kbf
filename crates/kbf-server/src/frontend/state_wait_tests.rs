//! The wait for a state store that is not up yet (`state_wait`): the attempts and waits it
//! makes over a scripted store and a recording sleep, the exit code each end maps to, and
//! the flag.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Duration;

use kbf_state::ManualClock;
use kbf_state::fake::{Fake, Flaw};

use super::exit_of;
use super::state_wait::{NotReached, open_within};
use super::tests::parsed;
use crate::farm::{EpochPolicy, StateError};
use crate::test_log::capture;
use crate::{OpenFailure, StateStore};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn open_store() -> StateStore {
    StateStore {
        store: Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        )),
        epoch: EpochPolicy::Keep,
        members: crate::members::MembershipConfig::default(),
        history: None,
    }
}

fn not_up() -> OpenFailure {
    OpenFailure::NotUp(StateError::Open("connection refused".to_owned()))
}

fn refused() -> OpenFailure {
    OpenFailure::Refused(StateError::Open("a database that is not there".to_owned()))
}

/// What a scripted attempt answers.
#[derive(Clone, Copy)]
enum Answer {
    Opens,
    NotUp,
    Refuses,
}

use Answer::{NotUp, Opens, Refuses};

/// What one run of `open_within` did: the sleeps it asked for, the attempts it made, and
/// its result.
struct Run {
    sleeps: Vec<Duration>,
    attempts: u32,
    result: Result<StateStore, Box<dyn std::error::Error>>,
}

/// Runs `open_within` over a store that answers with `script` in order (the last answer
/// repeats), recording the sleeps instead of making them.
fn run(window: Duration, script: &[Answer]) -> Run {
    let attempts = Cell::new(0_u32);
    let sleeps = RefCell::new(Vec::new());
    let result = open_within(
        window,
        &mut || {
            let n = attempts.get() as usize;
            attempts.set(attempts.get() + 1);
            match script[n.min(script.len() - 1)] {
                Opens => Ok(open_store()),
                NotUp => Err(not_up()),
                Refuses => Err(refused()),
            }
        },
        &mut |wait| sleeps.borrow_mut().push(wait),
    );
    Run {
        sleeps: sleeps.into_inner(),
        attempts: attempts.get(),
        result,
    }
}

/// A store that refuses to be reached three times and then opens: it is opened on the
/// fourth attempt, after three waits that start at 250 ms and double. Catches a start that
/// gives up on the first "not up" (the bug), one that waits a fixed time, and one that
/// counts the attempt that succeeds as a wait.
#[test]
fn a_store_not_up_for_three_attempts_is_opened_on_the_fourth() {
    let got = run(secs(300), &[NotUp, NotUp, NotUp, Opens]);
    assert!(got.result.is_ok());
    assert_eq!(got.attempts, 4);
    assert_eq!(got.sleeps, [ms(250), ms(500), ms(1000)]);
}

/// The waits double up to 10 s and stay there: twelve waits are 250 ms, 500 ms, 1 s, 2 s,
/// 4 s, 8 s, then 10 s each. Catches a cap that is missing, or that is applied before the
/// doubling instead of after it.
#[test]
fn the_waits_double_up_to_ten_seconds() {
    let mut script = vec![NotUp; 12];
    script.push(Opens);
    let got = run(secs(3600), &script);
    assert!(got.result.is_ok());
    assert_eq!(got.attempts, 13);
    let expected: Vec<Duration> = [250, 500, 1000, 2000, 4000, 8000]
        .into_iter()
        .map(ms)
        .chain(std::iter::repeat_n(secs(10), 6))
        .collect();
    assert_eq!(got.sleeps, expected);
}

/// A store that stays down for the whole window: the last wait is cut to what the window
/// has left, the waits add up to the window and no more, the last attempt is made after
/// the last wait, and the result is `NotReached`, which exits 1. Catches a window that is
/// overshot by a whole wait, a start that gives up before the window is used up, and one
/// that exits 2.
#[test]
fn a_store_down_for_the_whole_window_gives_up_with_exit_one() {
    let got = run(ms(2000), &[NotUp]);
    // 250 + 500 + 1000 = 1750 ms, and 250 ms of the window are left for the fourth wait.
    assert_eq!(got.sleeps, [ms(250), ms(500), ms(1000), ms(250)]);
    assert_eq!(got.sleeps.iter().sum::<Duration>(), ms(2000));
    assert_eq!(got.attempts, 5);
    let err = got.result.err().expect("the store never opened");
    let reached = err.downcast_ref::<NotReached>().expect("the transient end");
    assert_eq!(reached.attempts, 5);
    assert_eq!(reached.waited, ms(2000));
    let said = err.to_string();
    assert!(said.contains("5 attempts"), "{said}");
    assert!(said.contains("connection refused"), "{said}");
    assert_eq!(
        format!("{:?}", exit_of(err.as_ref())),
        format!("{:?}", std::process::ExitCode::from(1))
    );
}

/// A refusal on the first attempt is the result at once, with no wait and no second
/// attempt, and exits 2: a wrong URL, login, database or schema does not get better with
/// time. Catches a refusal that is retried, and one that is reported as the transient end.
#[test]
fn a_refusal_is_not_waited_for_and_exits_two() {
    let got = run(secs(300), &[Refuses, Opens]);
    assert_eq!(got.attempts, 1);
    assert!(got.sleeps.is_empty());
    let err = got.result.err().expect("refused");
    assert!(err.downcast_ref::<NotReached>().is_none());
    assert!(err.downcast_ref::<StateError>().is_some());
    assert_eq!(
        format!("{:?}", exit_of(err.as_ref())),
        format!("{:?}", std::process::ExitCode::from(2))
    );
}

/// A refusal after some waits stops the waiting there: the store came up and said no.
/// Catches a loop that goes on to the end of the window after a refusal.
#[test]
fn a_refusal_after_waiting_stops_the_wait() {
    let got = run(secs(300), &[NotUp, NotUp, Refuses]);
    assert_eq!(got.attempts, 3);
    assert_eq!(got.sleeps, [ms(250), ms(500)]);
    let err = got.result.err().expect("refused");
    assert!(err.downcast_ref::<StateError>().is_some());
}

/// With a window of 0 a store that is not up is the result at once, as it was before the
/// flag: one attempt, no sleep, the store's own error, exit 2. Catches a window of 0 that
/// still waits, and one that reports `NotReached`.
#[test]
fn a_window_of_zero_does_not_wait() {
    let got = run(Duration::ZERO, &[NotUp, Opens]);
    assert_eq!(got.attempts, 1);
    assert!(got.sleeps.is_empty());
    let err = got.result.err().expect("not up");
    assert_eq!(
        err.to_string(),
        "the state store cannot be opened: connection refused"
    );
    assert_eq!(
        format!("{:?}", exit_of(err.as_ref())),
        format!("{:?}", std::process::ExitCode::from(2))
    );
}

/// A store that is up on the first attempt is opened with no wait.
#[test]
fn a_store_that_is_up_is_opened_at_once() {
    let got = run(secs(300), &[Opens]);
    assert!(got.result.is_ok());
    assert_eq!(got.attempts, 1);
    assert!(got.sleeps.is_empty());
}

/// The wait's start is logged once with the window; every attempt that did not open the
/// store is logged with its number, the time waited so far, the window, what remains of it
/// and the classified reason; the end says it gave up, and an open after waiting says
/// so. Catches an attempt that is retried silently.
#[test]
fn each_attempt_is_logged_with_its_number_wait_and_reason() {
    let log = capture();
    let got = run(ms(750), &[NotUp]);
    assert!(got.result.is_err());
    let text = log.text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert!(lines[0].contains("waiting up to 0.75 s"), "{text}");
    assert!(lines[0].contains("window_secs=0.75"), "{text}");
    assert!(lines[1].contains("attempt=1"), "{text}");
    assert!(lines[1].contains("waited_secs=0.0"), "{text}");
    assert!(lines[1].contains("window_secs=0.75"), "{text}");
    assert!(lines[1].contains("remaining_secs=0.75"), "{text}");
    assert!(lines[1].contains("reason=\"not_up\""), "{text}");
    assert!(lines[2].contains("attempt=2"), "{text}");
    assert!(lines[2].contains("waited_secs=0.25"), "{text}");
    assert!(lines[2].contains("remaining_secs=0.5"), "{text}");
    assert!(lines[3].contains("attempt=3"), "{text}");
    assert!(lines[3].contains("waited_secs=0.75"), "{text}");
    assert!(lines[3].contains("remaining_secs=0.0"), "{text}");
    assert!(lines[3].contains("giving up"), "{text}");

    drop(log);
    let opened = capture();
    assert!(run(secs(300), &[NotUp, Opens]).result.is_ok());
    let text = opened.text();
    assert!(text.contains("reachable, opened"), "{text}");
    assert!(text.contains("attempt=2"), "{text}");
}

/// The flag defaults to 300 s (so a reboot recovers with no configuration), takes 0 to
/// 3600, and refuses what is outside that or is not a number.
#[test]
fn the_flag_has_its_default_and_bounds() {
    let window = |more: &[&str]| parsed(more).map(|f| f.state_store_wait);
    assert_eq!(window(&[]).expect("parses"), secs(300));
    assert_eq!(
        window(&["--state-store-wait-secs=0"]).expect("parses"),
        Duration::ZERO
    );
    assert_eq!(
        window(&["--state-store-wait-secs=3600"]).expect("parses"),
        secs(3600)
    );
    for bad in ["3601", "-1", "soon", ""] {
        let flag = format!("--state-store-wait-secs={bad}");
        assert!(window(&[flag.as_str()]).is_err(), "{bad}");
    }
}
