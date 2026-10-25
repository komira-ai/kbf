//! C1 over a scripted launchd.

use std::cell::RefCell;
use std::collections::VecDeque;

use super::*;

/// A launchd that answers the `bootout`s from `boots` in turn (the last one for ever) and
/// records what it was asked.
struct Fake {
    asked: RefCell<Vec<String>>,
    pauses: RefCell<usize>,
    boots: RefCell<VecDeque<Said>>,
    /// Every call hangs until its alarm.
    hang: bool,
    clock: RefCell<Duration>,
}

impl Fake {
    fn new(boots: Vec<Said>) -> Self {
        Self {
            asked: RefCell::default(),
            pauses: RefCell::new(0),
            boots: RefCell::new(boots.into()),
            hang: false,
            clock: RefCell::new(Duration::ZERO),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

impl Launchd for Fake {
    fn run(&self, args: &[&str], alarm: Duration) -> Said {
        self.asked.borrow_mut().push(args.join(" "));
        if self.hang {
            *self.clock.borrow_mut() += alarm;
            return Said::TimedOut;
        }
        match args {
            ["bootout", _] => {
                let mut boots = self.boots.borrow_mut();
                if boots.len() > 1 {
                    boots.pop_front().unwrap()
                } else {
                    boots.front().cloned().unwrap_or(Said::TimedOut)
                }
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    fn pause(&self, time: Duration) {
        *self.pauses.borrow_mut() += 1;
        *self.clock.borrow_mut() += time;
    }

    fn now(&self) -> Duration {
        *self.clock.borrow()
    }
}

fn exited(code: i32) -> Said {
    Said::Exited { code }
}

/// Catches a per-label bootout and a verb after the bootouts: the domains `user/700` and
/// `gui/700` are booted out whole, in that order, once each, and nothing else is run.
#[test]
fn the_domains_are_booted_out_whole_once_each_and_nothing_else_is_run() {
    let fake = Fake::new(vec![exited(0)]);
    assert_eq!(end(&fake, 700), Ok(()));
    assert_eq!(fake.asked(), ["bootout user/700", "bootout gui/700"]);
    assert_eq!(*fake.pauses.borrow(), 0);
}

/// Catches a 124 ("tearing down") read as done (a job that launchd is still stopping would
/// be missed), a wait that is not bounded (a domain that never settles would hang the
/// slot), and a wait with no pauses (150 bootouts back to back): 124 and timeouts are
/// waited out by booting the domain out again; any other answer, a missing domain (113)
/// included, ends the domain's wait at once.
#[test]
fn a_domain_tearing_down_is_waited_for_and_the_wait_is_bounded() {
    let fake = Fake::new(vec![exited(124), Said::TimedOut, exited(0)]);
    assert_eq!(end(&fake, 700), Ok(()));
    assert_eq!(*fake.pauses.borrow(), 2);
    assert_eq!(
        fake.asked(),
        [
            "bootout user/700",
            "bootout user/700",
            "bootout user/700",
            "bootout gui/700"
        ]
    );

    let fake = Fake::new(vec![exited(113)]);
    assert_eq!(end(&fake, 700), Ok(()));
    assert_eq!(*fake.pauses.borrow(), 0);

    let fake = Fake::new(vec![exited(124)]);
    let why = end(&fake, 700).expect_err("never settles");
    assert!(why.contains("after 150 bootouts"), "{why}");
    assert_eq!(*fake.pauses.borrow(), POLLS - 1);
    assert_eq!(fake.asked().len(), POLLS);
}

/// Catches a launchctl that cannot be run being read as a clean domain.
#[test]
fn a_launchctl_that_cannot_run_fails_the_step() {
    let fake = Fake::new(vec![Said::Missing("no such file".to_owned())]);
    let why = end(&fake, 700).expect_err("bootout");
    assert!(
        why.starts_with("launchctl bootout user/700: no such file"),
        "{why}"
    );
}

/// Catches a step bounded in bootouts and not in time (150 bootouts of a launchd that
/// hangs for its whole 20 s alarm would hold the daemon's start for fifty minutes): with
/// every call hanging until its alarm, the step fails at the 60 s total, after three
/// calls, the last cut to what was left.
#[test]
fn a_launchctl_that_hangs_fails_the_step_at_the_total_deadline() {
    let mut fake = Fake::new(vec![exited(0)]);
    fake.hang = true;
    let why = end(&fake, 700).expect_err("a hung launchctl passed");
    assert!(why.contains("did not settle within 60s"), "{why}");
    assert_eq!(fake.asked(), ["bootout user/700"; 3]);
    assert!(fake.now() >= WITHIN);
}

/// Catches a bootout that eats the whole budget unseen: a bootout that hangs leaves the
/// next call only what is left, never a fresh 20 s.
#[test]
fn a_call_is_cut_to_what_is_left_of_the_total() {
    struct Slow(Fake, u64);
    impl Launchd for Slow {
        fn run(&self, args: &[&str], alarm: Duration) -> Said {
            if self.0.asked.borrow().is_empty() && self.0.clock.borrow().is_zero() {
                // The first call takes `self.1` s of the minute, whatever its alarm.
                self.0.asked.borrow_mut().push(args.join(" "));
                *self.0.clock.borrow_mut() += Duration::from_secs(self.1);
                return Said::TimedOut;
            }
            assert!(
                alarm <= Duration::from_secs(10),
                "{alarm:?} of the 10 s left"
            );
            self.0.run(args, alarm)
        }
        fn pause(&self, time: Duration) {
            self.0.pause(time);
        }
        fn now(&self) -> Duration {
            self.0.now()
        }
    }
    let slow = Slow(Fake::new(vec![exited(0)]), 50);
    assert_eq!(end(&slow, 700), Ok(()));
    // A first bootout that takes the whole minute leaves the next call no time: the step
    // fails there, before it runs another call.
    let spent = Slow(Fake::new(vec![exited(0)]), 60);
    let why = end(&spent, 700).expect_err("no time left");
    assert!(why.contains("user/700 did not settle"), "{why}");
    assert_eq!(spent.0.asked(), ["bootout user/700"]);
}

/// A launchd that keeps state, and does what the Mac was measured to do: a `print` of a
/// `user/<uid>` that is not there **creates it**, with launchd's own `distnoted` running
/// in it; a `bootout` removes the domain and what runs in it.
struct World {
    domains: RefCell<Vec<String>>,
    asked: RefCell<Vec<String>>,
}

impl World {
    /// A Mac whose slot 700 has just had a lease: both domains exist.
    fn after_a_lease() -> Self {
        Self {
            domains: RefCell::new(vec!["user/700".to_owned(), "gui/700".to_owned()]),
            asked: RefCell::default(),
        }
    }

    /// The processes of uid 700 the Mac runs now: launchd's `distnoted` runs for as long
    /// as `user/700` exists.
    fn uid_processes(&self) -> Vec<&'static str> {
        if self.domains.borrow().iter().any(|d| d == "user/700") {
            vec!["distnoted"]
        } else {
            Vec::new()
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

impl Launchd for World {
    fn run(&self, args: &[&str], _alarm: Duration) -> Said {
        self.asked.borrow_mut().push(args.join(" "));
        let mut domains = self.domains.borrow_mut();
        match args {
            ["bootout", target] => match domains.iter().position(|d| d == target) {
                Some(at) => {
                    domains.remove(at);
                    exited(0)
                }
                None => exited(113),
            },
            // Measured: a root `print` of a missing user domain makes it, and launchd
            // starts its per-user agents in it at once.
            ["print", target] if target.matches('/').count() == 1 => {
                if !domains.iter().any(|d| d == target) {
                    domains.push((*target).to_owned());
                }
                exited(0)
            }
            // A print of anything else (a job) answers, and makes nothing.
            ["print", _] => exited(0),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn pause(&self, _time: Duration) {}

    fn now(&self) -> Duration {
        Duration::ZERO
    }
}

/// Catches an end that leaves a domain behind: the poll that waited for the domain to be
/// empty printed `user/<uid>` after the bootout, and a root print makes the domain, with
/// launchd's agents running in it as the uid, for the next lease to inherit. After the end
/// neither domain exists and no process runs as the uid.
#[test]
fn the_end_leaves_no_domain_and_no_process_of_the_uid() {
    let world = World::after_a_lease();
    assert_eq!(end(&world, 700), Ok(()));
    assert_eq!(*world.domains.borrow(), Vec::<String>::new());
    assert_eq!(world.uid_processes(), Vec::<&str>::new());
}

/// Catches any `launchctl` verb on the uid's domains that can make one after the last
/// bootout: a `print` (measured to make the domain), and so any other look at it. What
/// follows the last bootout is nothing.
#[test]
fn nothing_follows_the_bootouts_on_the_uids_domains() {
    let world = World::after_a_lease();
    end(&world, 700).expect("ended");
    let asked = world.asked();
    let last = asked
        .iter()
        .rposition(|a| a.starts_with("bootout "))
        .expect("a bootout");
    assert_eq!(asked[last + 1..], Vec::<String>::new(), "{asked:?}");
    assert!(
        asked.iter().all(|a| a.starts_with("bootout ")),
        "a verb that can make the domain: {asked:?}"
    );
}
