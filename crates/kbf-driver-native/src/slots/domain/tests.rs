//! C1 over a scripted launchd.

use std::cell::RefCell;
use std::collections::VecDeque;

use super::*;

/// A launchd that answers the `print user/<uid>` looks from `looks` in turn (the last
/// one for ever), a job's print from `jobs`, and records what it was asked.
struct Fake {
    asked: RefCell<Vec<String>>,
    pauses: RefCell<usize>,
    looks: RefCell<VecDeque<Said>>,
    jobs: Vec<(&'static str, Said)>,
    bootout: Said,
    /// Every call hangs until its alarm.
    hang: bool,
    clock: RefCell<Duration>,
}

impl Fake {
    fn new(looks: Vec<Said>) -> Self {
        Self {
            asked: RefCell::default(),
            pauses: RefCell::new(0),
            looks: RefCell::new(looks.into()),
            jobs: Vec::new(),
            bootout: exited(0, ""),
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
            ["bootout", _] => self.bootout.clone(),
            ["print", target] if target.matches('/').count() == 1 => {
                let mut looks = self.looks.borrow_mut();
                if looks.len() > 1 {
                    looks.pop_front().unwrap()
                } else {
                    looks.front().cloned().unwrap_or(Said::TimedOut)
                }
            }
            ["print", target] => (self.jobs.iter())
                .find(|(label, _)| target.ends_with(label))
                .map_or_else(|| exited(113, ""), |(_, said)| said.clone()),
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

fn exited(code: i32, stdout: &str) -> Said {
    Said::Exited {
        code,
        stdout: stdout.to_owned(),
    }
}

fn listing(labels: &[&str]) -> Said {
    let rows: String = labels
        .iter()
        .map(|label| format!("\t\t   0      -   {label}\n"))
        .collect();
    exited(
        0,
        &format!("user/700 = {{\n\tservices = {{\n{rows}\t}}\n}}\n"),
    )
}

fn plist(path: &str) -> Said {
    exited(0, &format!("x = {{\n\tpath = {path}\n}}\n"))
}

/// Catches a per-label bootout (the `asked` list below has none): the domains `user/700` and `gui/700` are booted out whole, in that order, once
/// each, and only then is the domain printed; a job in the domain gets a `print` of its
/// own and never a `bootout`.
#[test]
fn the_domains_are_booted_out_whole_and_no_job_by_its_label() {
    let mut fake = Fake::new(vec![listing(&["kbf.agent"]), listing(&[])]);
    fake.jobs = vec![(
        "kbf.agent",
        plist("/Volumes/pool/p/Library/LaunchAgents/kbf.plist"),
    )];
    assert_eq!(end(&fake, 700), Ok(()));
    assert_eq!(
        fake.asked(),
        [
            "bootout user/700",
            "bootout gui/700",
            "print user/700",
            "print user/700/kbf.agent",
            "print user/700",
        ]
    );
    assert_eq!(*fake.pauses.borrow(), 1);
}

/// Catches a label-based judge (a lease's agent labelled `com.apple.*` would be left
/// loaded and pass), one that counts launchd's own agents as the lease's (the wait would
/// never end: they stay loaded in every user domain), and a missing plist read as
/// launchd's: only a plist under the sealed folders is launchd's own.
#[test]
fn only_a_plist_in_a_system_folder_is_launchds_own() {
    let mut fake = Fake::new(vec![listing(&["com.apple.cfprefsd.xpc.agent"])]);
    fake.jobs = vec![(
        "com.apple.cfprefsd.xpc.agent",
        plist("/System/Library/LaunchAgents/com.apple.cfprefsd.xpc.agent.plist"),
    )];
    assert_eq!(
        end(&fake, 700),
        Ok(()),
        "launchd's own agent is left loaded"
    );

    for lease in [
        plist("/Volumes/pool/p/Library/LaunchAgents/com.apple.fake.plist"),
        exited(0, "x = {\n}\n"),
        exited(113, ""),
        Said::TimedOut,
    ] {
        let mut fake = Fake::new(vec![listing(&["com.apple.fake"])]);
        fake.jobs = vec![("com.apple.fake", lease)];
        let why = end(&fake, 700).expect_err("a lease's job passed as launchd's own");
        assert!(
            why.contains("still lists jobs") && why.contains("com.apple.fake"),
            "{why}"
        );
    }
}

/// Catches a 124 ("tearing down") read as a failure or as a gone domain (a job that
/// launchd is still stopping would be missed), a wait that is not bounded (a domain that
/// never settles would hang the slot), a wait with no pauses (150 prints back to back),
/// and a timed-out print read as an empty domain: 124 and timeouts are waited out; the
/// domain's end, a missing domain (113), ends the wait at once.
#[test]
fn a_domain_tearing_down_is_waited_for_and_the_wait_is_bounded() {
    let fake = Fake::new(vec![
        exited(124, "Domain is tearing down"),
        Said::TimedOut,
        listing(&[]),
    ]);
    assert_eq!(end(&fake, 700), Ok(()));
    assert_eq!(*fake.pauses.borrow(), 2);

    let fake = Fake::new(vec![exited(113, "Could not find service")]);
    assert_eq!(end(&fake, 700), Ok(()));
    assert_eq!(*fake.pauses.borrow(), 0);

    let fake = Fake::new(vec![exited(124, "Domain is tearing down")]);
    let why = end(&fake, 700).expect_err("never settles");
    assert!(why.contains("after 150 looks"), "{why}");
    assert_eq!(*fake.pauses.borrow(), POLLS - 1);
    let prints = fake
        .asked()
        .iter()
        .filter(|a| *a == "print user/700")
        .count();
    assert_eq!(prints, POLLS);
}

/// Catches a launchctl that cannot be run being read as a clean domain: both the
/// bootout and the print report it.
#[test]
fn a_launchctl_that_cannot_run_fails_the_step() {
    let mut fake = Fake::new(vec![listing(&[])]);
    fake.bootout = Said::Missing("no such file".to_owned());
    let why = end(&fake, 700).expect_err("bootout");
    assert!(
        why.starts_with("launchctl bootout user/700: no such file"),
        "{why}"
    );
    let fake = Fake::new(vec![Said::Missing("denied".to_owned())]);
    let why = end(&fake, 700).expect_err("print");
    assert!(why.starts_with("launchctl print user/700: denied"), "{why}");
}

/// Catches a step bounded in looks and not in time (150 prints of a launchd that hangs
/// for its whole 20 s alarm would hold the daemon's start for fifty minutes): with every
/// call hanging until its alarm, the step fails at the 60 s total, after three calls
/// (two bootouts and one print), the last cut to what was left.
#[test]
fn a_launchctl_that_hangs_fails_the_step_at_the_total_deadline() {
    let mut fake = Fake::new(vec![listing(&[])]);
    fake.hang = true;
    let why = end(&fake, 700).expect_err("a hung launchctl passed");
    assert!(why.contains("did not settle within 60s"), "{why}");
    assert_eq!(
        fake.asked(),
        ["bootout user/700", "bootout gui/700", "print user/700"]
    );
    assert!(fake.now() >= WITHIN);
}

/// Catches a bootout that eats the whole budget unseen: a bootout that hangs leaves the
/// poll only what is left, never a fresh 20 s.
#[test]
fn a_call_is_cut_to_what_is_left_of_the_total() {
    struct Slow(Fake, u64);
    impl Launchd for Slow {
        fn run(&self, args: &[&str], alarm: Duration) -> Said {
            if args == ["bootout", "user/700"] {
                // Takes `self.1` s of the minute, whatever its alarm.
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
    let slow = Slow(Fake::new(vec![listing(&[])]), 50);
    assert_eq!(end(&slow, 700), Ok(()));
    // A first bootout that takes the whole minute leaves the second no time: the step
    // fails there, before it runs another call.
    let spent = Slow(Fake::new(vec![listing(&[])]), 60);
    let why = end(&spent, 700).expect_err("no time left");
    assert!(why.contains("gui/700 did not settle"), "{why}");
    assert_eq!(spent.0.asked(), Vec::<String>::new());
}
