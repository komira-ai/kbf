//! The Mac backend's lease count before a switch, through the socket's core: with a live
//! lease nothing moves and the answer is `leases_live`; with none the release is switched to
//! and confirmed; a count that cannot be made refuses the switch with its reason.

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;

use kbf_installer::helper::leases::LeaseSource;

use super::answer::{Answer, Code};
use super::core::Ab;
use super::mac_backend::MacBackend;
use super::request::LocalRequest;
use crate::mac::kit;
use crate::mac::tests::fx::{Fx, NEW, NEW_VERSION, OLD, Sim, mkdir};
use crate::runner::{Cmd, Exec, Runner};

/// A lease source whose answer the test sets, and that counts how often it was asked.
#[derive(Clone)]
pub(crate) struct Leases {
    answer: Rc<RefCell<Result<usize, String>>>,
    asked: Rc<Cell<usize>>,
}

impl Leases {
    /// A host on which no lease runs.
    pub(crate) fn idle() -> Self {
        Self {
            answer: Rc::new(RefCell::new(Ok(0))),
            asked: Rc::default(),
        }
    }

    fn say(&self, answer: Result<usize, String>) {
        *self.answer.borrow_mut() = answer;
    }
}

impl LeaseSource for Leases {
    fn live(&self) -> Result<usize, String> {
        self.asked.set(self.asked.get() + 1);
        self.answer.borrow().clone()
    }
}

/// A runner that is the scripted Mac, kept by the test.
struct Shared(Rc<Sim>);

impl Runner for Shared {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        self.0.run(cmd)
    }
}

struct Rig {
    fx: Fx,
    sim: Rc<Sim>,
    leases: Leases,
    ab: Ab<MacBackend>,
}

impl Rig {
    /// A Mac that runs `OLD`, with `NEW` staged by an install, and no lease yet.
    fn staged(name: &str) -> Self {
        let fx = Fx::new(name);
        mkdir(&fx.host.work, 0o711);
        let sim = Rc::new(fx.sim());
        let leases = Leases::idle();
        let backend = MacBackend::new(
            fx.host.clone(),
            Box::new(Shared(Rc::clone(&sim))),
            Box::new(leases.clone()),
        );
        let ab = Ab::new(
            backend,
            fx.host.work.join("state.json"),
            fx.host.trust.clone(),
            600,
            Box::new(|| 100),
        );
        let rig = Self {
            fx,
            sim,
            leases,
            ab,
        };
        assert!(matches!(
            rig.ask(&format!("install kbf={NEW_VERSION} from stable")),
            Answer::Staged { current: false, .. }
        ));
        rig
    }

    fn ask(&self, line: &str) -> Answer {
        self.ab.handle(&LocalRequest::parse(line).unwrap())
    }

    fn switch(&self) -> Answer {
        self.ask(&format!("switch kbf={NEW_VERSION}"))
    }

    fn current(&self) -> Option<String> {
        kit::links(&self.fx.host).unwrap().current
    }

    /// Whether the daemon's job was touched at all.
    fn restarts(&self) -> u32 {
        self.sim.daemon_bootouts.get() + self.sim.daemon_bootstraps.get()
    }

    /// Nothing of the host changed: the release that ran still runs, no marker, no restart.
    #[track_caller]
    fn untouched(&self) {
        assert_eq!(self.current().as_deref(), Some(OLD));
        assert!(!self.fx.host.pending_marker().exists());
        assert_eq!(self.restarts(), 0);
        let Answer::Queried(view) = self.ask("query") else {
            panic!("not a query")
        };
        assert!(view.pending.is_none());
    }
}

/// Catches a Mac that switches under a live lease, or one whose refusal reads as a failed
/// update: with two leases live the switch is refused `leases_live` with the count in the
/// detail, nothing moves (no marker, the old release current, the daemon's job not touched),
/// and the host's last end is not a failure of the target. The lease count is asked before
/// anything else of the switch.
#[test]
fn an_order_with_a_live_lease_is_refused_leases_live_and_nothing_moves() {
    let rig = Rig::staged("mbl-live");
    let asked = rig.leases.asked.get();
    rig.leases.say(Ok(2));
    assert_eq!(
        rig.switch(),
        Answer::Refused(
            Code::LeasesLive,
            "2 leases are live; not switching".to_owned()
        )
    );
    assert_eq!(rig.leases.asked.get(), asked + 1);
    rig.untouched();
    let Answer::Queried(view) = rig.ask("query") else {
        panic!("not a query")
    };
    assert_eq!(view.last, None, "a deferral is not an end of the update");
}

/// Catches a Mac that never takes the order once its leases have ended: after a refusal for
/// a live lease, the same switch with none live is made (the marker written, the links moved,
/// the daemon restarted on the new release) and confirmed, and the new release stays.
#[test]
fn with_no_live_lease_the_release_is_activated_and_confirmed() {
    let rig = Rig::staged("mbl-idle");
    rig.leases.say(Ok(1));
    assert!(matches!(rig.switch(), Answer::Refused(Code::LeasesLive, _)));
    rig.leases.say(Ok(0));
    assert!(matches!(
        rig.switch(),
        Answer::Switched {
            deadline_unix: 700,
            ..
        }
    ));
    assert_eq!(rig.current().as_deref(), Some(NEW));
    assert!(rig.fx.host.pending_marker().exists());
    assert_eq!(rig.sim.daemon_bootstraps.get(), 1);
    assert!(matches!(
        rig.ask(&format!("confirm kbf={NEW_VERSION}")),
        Answer::Confirmed { .. }
    ));
    assert!(!rig.fx.host.pending_marker().exists());
    assert_eq!(rig.current().as_deref(), Some(NEW));
}

/// Catches a lease count that cannot be made read as idle: the switch is refused
/// `state_unavailable`, the detail is the source's reason in full, and nothing moves.
#[test]
fn a_lease_record_that_cannot_be_read_refuses_the_switch_with_the_reason() {
    let rig = Rig::staged("mbl-unread");
    rig.leases
        .say(Err("/Volumes/kbf/data/leases is a link".to_owned()));
    assert_eq!(
        rig.switch(),
        Answer::Refused(
            Code::StateUnavailable,
            "cannot tell whether leases are live: /Volumes/kbf/data/leases is a link".to_owned()
        )
    );
    rig.untouched();
}
