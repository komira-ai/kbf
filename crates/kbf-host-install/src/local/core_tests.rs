//! The update over a scripted backend: what is asked of the platform, in what order, and
//! what the host remembers and tells.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::rc::Rc;

use kbf_retain::Kept;
use kbf_testkit::dir::TestDir;

use super::answer::{Answer, Code, LastView, PendingView, QueryView};
use super::backend::{Backend, Fault, Pending};
use super::core::Ab;
use super::request::LocalRequest;
use crate::request::{Channel, Version};
use crate::trust::Trust;

pub(super) const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub(super) const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
pub(super) const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
pub(super) const VA: &str = "0.1.5+gaaaaaaaaaaaa-2";
pub(super) const VB: &str = "0.1.7+gbbbbbbbbbbbb-3";
pub(super) const VC: &str = "0.1.8+gcccccccccccc-4";
pub(super) const WINDOW: u64 = 600;

/// What the platform has, and what it was asked.
#[derive(Default)]
pub(super) struct World {
    pub(super) current: Option<String>,
    pub(super) pending: Option<Pending>,
    pub(super) on_disk: BTreeSet<String>,
    /// The commit an install of a version lays out.
    pub(super) lays: Option<String>,
    pub(super) switch_fault: Option<Fault>,
    pub(super) revert_fault: Option<Fault>,
    /// Whether a revert that fails has cleared the marker anyway.
    pub(super) revert_clears: bool,
    pub(super) confirm_fault: Option<Fault>,
    /// The releases the host lists as kept; a prune removes the commits in `prune_removes`.
    pub(super) kept: Vec<Kept>,
    pub(super) kept_fault: bool,
    /// The file that names the releases kept whatever their age; none is a host with no
    /// release directories, where no prune is asked for.
    pub(super) retain_file: Option<PathBuf>,
    pub(super) prune_say: String,
    pub(super) prune_fault: Option<Fault>,
    pub(super) prune_removes: Vec<String>,
    /// A version another writer (a stage) records in the retain file just after the next
    /// listing is taken: the commit and its text.
    pub(super) recorded_after_listing: Option<(String, String)>,
    pub(super) calls: Vec<String>,
}

#[derive(Clone)]
pub(super) struct Fake {
    pub(super) world: Rc<RefCell<World>>,
    pub(super) lock: PathBuf,
}

impl Backend for Fake {
    fn lock_file(&self) -> PathBuf {
        self.lock.clone()
    }

    fn node_id(&self) -> Option<String> {
        Some("n1".to_owned())
    }

    fn current(&self) -> Result<Option<String>, Fault> {
        Ok(self.world.borrow().current.clone())
    }

    fn pending(&self) -> Result<Option<Pending>, Fault> {
        Ok(self.world.borrow().pending.clone())
    }

    fn has(&self, commit: &str) -> bool {
        self.world.borrow().on_disk.contains(commit)
    }

    fn stage(&self, version: &Version, channel: &Channel) -> Result<String, Fault> {
        let mut world = self.world.borrow_mut();
        world
            .calls
            .push(format!("stage {} {}", version.as_str(), channel.as_str()));
        let commit = world
            .lays
            .clone()
            .ok_or_else(|| Fault::refused(Code::UnknownRelease, "none"))?;
        world.on_disk.insert(commit.clone());
        Ok(commit)
    }

    fn switch(&self, commit: &str, window_secs: u64, now: u64) -> Result<u64, Fault> {
        let mut world = self.world.borrow_mut();
        world
            .calls
            .push(format!("switch {} {window_secs}", &commit[..1]));
        if let Some(fault) = world.switch_fault.clone() {
            return Err(fault);
        }
        let from = world.current.clone().unwrap();
        world.pending = Some(Pending {
            commit: commit.to_owned(),
            from,
            deadline_unix: Some(now + window_secs),
            activation: "act".to_owned(),
        });
        world.current = Some(commit.to_owned());
        Ok(now + window_secs)
    }

    fn confirm(&self, pending: &Pending) -> Result<(), Fault> {
        let mut world = self.world.borrow_mut();
        world.calls.push(format!("confirm {}", pending.activation));
        if let Some(fault) = world.confirm_fault.clone() {
            return Err(fault);
        }
        world.pending = None;
        Ok(())
    }

    fn revert(&self, pending: &Pending) -> Result<(), Fault> {
        let mut world = self.world.borrow_mut();
        world.calls.push("revert".to_owned());
        if let Some(fault) = world.revert_fault.clone() {
            if world.revert_clears {
                world.pending = None;
                world.current = Some(pending.from.clone());
            }
            return Err(fault);
        }
        world.pending = None;
        world.current = Some(pending.from.clone());
        Ok(())
    }

    fn kept(&self) -> Result<Vec<Kept>, Fault> {
        let world = self.world.borrow();
        if world.kept_fault {
            return Err(Fault::failed(Code::StateUnavailable, "listing"));
        }
        let kept = world.kept.clone();
        drop(world);
        let late = self.world.borrow_mut().recorded_after_listing.take();
        if let (Some((commit, version)), Some(file)) =
            (late, self.world.borrow().retain_file.clone())
        {
            kbf_retain::Retained::update(&file, |r| {
                r.versions
                    .insert(kbf_retain::CommitId::new(&commit).unwrap(), version);
            })
            .unwrap();
        }
        Ok(kept)
    }

    fn retain_file(&self) -> Option<PathBuf> {
        self.world.borrow().retain_file.clone()
    }

    fn prune(&self, keep: usize) -> Result<String, Fault> {
        let mut world = self.world.borrow_mut();
        world.calls.push(format!("prune {keep}"));
        if let Some(fault) = world.prune_fault.clone() {
            return Err(fault);
        }
        let gone = world.prune_removes.clone();
        world
            .kept
            .retain(|k| !gone.iter().any(|g| g == k.commit.as_str()));
        Ok(world.prune_say.clone())
    }
}

pub(super) struct Fx {
    pub(super) dir: TestDir,
    pub(super) world: Rc<RefCell<World>>,
    pub(super) clock: Rc<Cell<u64>>,
    pub(super) ab: Ab<Fake>,
}

impl Fx {
    pub(super) fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
        let world = Rc::new(RefCell::new(World {
            current: Some(A.to_owned()),
            lays: Some(B.to_owned()),
            ..World::default()
        }));
        world.borrow_mut().on_disk.insert(A.to_owned());
        let clock = Rc::new(Cell::new(1000));
        let ab = Self::process(&dir, &world, &clock);
        Self {
            dir,
            world,
            clock,
            ab,
        }
    }

    /// A process of the program over the same host: what it starts with is on disk.
    pub(super) fn process(
        dir: &TestDir,
        world: &Rc<RefCell<World>>,
        clock: &Rc<Cell<u64>>,
    ) -> Ab<Fake> {
        let uid = fs::metadata(&**dir).unwrap().uid();
        let clock = Rc::clone(clock);
        Ab::new(
            Fake {
                world: Rc::clone(world),
                lock: dir.join("lock"),
            },
            dir.join("state.json"),
            Trust::owned_by(uid, dir),
            WINDOW,
            Box::new(move || clock.get()),
        )
    }

    pub(super) fn restarted(&self) -> Ab<Fake> {
        Self::process(&self.dir, &self.world, &self.clock)
    }

    pub(super) fn ask(&self, line: &str) -> Answer {
        self.ab.handle(&LocalRequest::parse(line).unwrap())
    }

    pub(super) fn install(&self, version: &str) -> Answer {
        self.ask(&format!("install kbf={version} from stable"))
    }

    pub(super) fn query(&self) -> QueryView {
        match self.ask("query") {
            Answer::Queried(view) => view,
            other => panic!("{other:?}"),
        }
    }

    pub(super) fn calls(&self) -> Vec<String> {
        self.world.borrow().calls.clone()
    }
}

fn refused(code: Code) -> impl Fn(&Answer) -> bool {
    move |answer| matches!(answer, Answer::Refused(c, _) if *c == code)
}

fn failed(code: Code) -> impl Fn(&Answer) -> bool {
    move |answer| matches!(answer, Answer::Failed(c, _) if *c == code)
}

fn last(target: &str, outcome: &'static str, reason: Option<&'static str>) -> Option<LastView> {
    Some(LastView {
        target: target.to_owned(),
        outcome,
        reason,
    })
}

/// A whole update in order: the release is laid out first and nothing moves; the switch puts
/// it on trial with a deadline a window away and the query shows the trial; the confirm keeps
/// it, and the query then tells the release, its version and that the update succeeded.
#[test]
fn an_update_stages_switches_and_confirms_in_that_order() {
    let fx = Fx::new("ab-whole");
    let staged = fx.install(VB);
    assert_eq!(
        staged,
        Answer::Staged {
            version: VB.into(),
            commit: B.into(),
            current: false
        }
    );
    let before = fx.query();
    assert_eq!(before.pending, None, "an install moves nothing");
    assert_eq!(before.commit.as_deref(), Some(A));

    let switched = fx.ask(&format!("switch kbf={VB}"));
    assert_eq!(
        switched,
        Answer::Switched {
            version: VB.into(),
            commit: B.into(),
            deadline_unix: 1000 + WINDOW
        }
    );
    let trial = fx.query();
    assert_eq!(
        trial.pending,
        Some(PendingView {
            version: Some(VB.into()),
            commit: B.into(),
            deadline_unix: Some(1000 + WINDOW)
        })
    );

    assert_eq!(
        fx.ask(&format!("confirm kbf={VB}")),
        Answer::Confirmed { version: VB.into() }
    );
    let after = fx.query();
    assert_eq!(after.pending, None);
    assert_eq!(after.commit.as_deref(), Some(B));
    assert_eq!(after.version.as_deref(), Some(VB));
    assert_eq!(after.node_id.as_deref(), Some("n1"));
    assert_eq!(after.last, last(VB, "succeeded", None));
    assert!(!after.busy);
    assert_eq!(
        fx.calls(),
        [
            "stage 0.1.7+gbbbbbbbbbbbb-3 stable",
            "switch b 600",
            "confirm act"
        ]
    );
}

/// A host that runs the release already fetches nothing: the install says so, and learns the
/// version's text, which the files do not hold, so the next heartbeat can name it.
#[test]
fn an_install_of_the_running_release_fetches_nothing_and_learns_its_version() {
    let fx = Fx::new("ab-current");
    assert_eq!(fx.query().version, None);
    let answer = fx.install(VA);
    assert_eq!(
        answer,
        Answer::Staged {
            version: VA.into(),
            commit: A.into(),
            current: true
        }
    );
    assert_eq!(fx.calls(), Vec::<String>::new());
    assert_eq!(fx.query().version.as_deref(), Some(VA));
}

/// An install asked again after it was answered is answered from what is laid out: the
/// release is not fetched twice. One whose files are gone is fetched again.
#[test]
fn an_install_repeated_does_not_fetch_again() {
    let fx = Fx::new("ab-twice");
    fx.install(VB);
    let again = fx.install(VB);
    assert!(matches!(again, Answer::Staged { current: false, .. }));
    assert_eq!(fx.calls().len(), 1, "{:?}", fx.calls());
    fx.world.borrow_mut().on_disk.remove(B);
    fx.install(VB);
    assert_eq!(fx.calls().len(), 2, "{:?}", fx.calls());
}

/// What an install is refused for, before anything is fetched: a release on trial, a host
/// with no release that runs, a version with no commit in it. A release the channel does not
/// have is the platform's refusal, passed on.
#[test]
fn an_install_is_refused_for_a_trial_no_current_release_or_no_commit() {
    let fx = Fx::new("ab-refused");
    assert!(refused(Code::UnknownRelease)(&fx.install("1.2.3-1")));
    fx.world.borrow_mut().lays = None;
    assert!(refused(Code::UnknownRelease)(&fx.install(VB)));
    assert_eq!(fx.calls().len(), 1, "only the second asked the platform");

    fx.world.borrow_mut().lays = Some(B.to_owned());
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
    assert!(refused(Code::Pending)(&fx.install(VC)));

    let bare = Fx::new("ab-nocurrent");
    bare.world.borrow_mut().current = None;
    assert!(refused(Code::NoCurrent)(&bare.install(VB)));
    assert_eq!(bare.calls().len(), 0);
}

/// A switch names a version an install laid out and that is still there: another version, or
/// files that are gone, are refused and the platform is not asked to move.
#[test]
fn a_switch_needs_the_version_that_was_laid_out() {
    let fx = Fx::new("ab-notstaged");
    assert!(refused(Code::NotStaged)(
        &fx.ask(&format!("switch kbf={VB}"))
    ));
    fx.install(VB);
    assert!(refused(Code::NotStaged)(
        &fx.ask(&format!("switch kbf={VC}"))
    ));
    fx.world.borrow_mut().on_disk.remove(B);
    assert!(refused(Code::NotStaged)(
        &fx.ask(&format!("switch kbf={VB}"))
    ));
    assert!(!fx.calls().iter().any(|c| c.starts_with("switch")));
}

/// The same switch asked again is the trial already under way, with its deadline; a switch to
/// another release while one is on trial is refused.
#[test]
fn a_switch_repeated_is_the_same_trial_and_another_is_refused() {
    let fx = Fx::new("ab-repeat");
    fx.install(VB);
    let first = fx.ask(&format!("switch kbf={VB}"));
    fx.clock.set(1100);
    assert_eq!(
        fx.ask(&format!("switch kbf={VB}")),
        first,
        "the deadline is the first"
    );
    fx.world.borrow_mut().lays = Some(C.to_owned());
    // Another release cannot even be installed while this one is on trial.
    assert!(refused(Code::Pending)(&fx.install(VC)));
    let staged_c = {
        let mut world = fx.world.borrow_mut();
        world.on_disk.insert(C.to_owned());
        world.pending.take()
    };
    // With the trial over, VC is staged, and then a trial of VB found by a switch to VC is in the way.
    assert!(staged_c.is_some());
    fx.install(VC);
    fx.world.borrow_mut().pending = staged_c;
    assert!(refused(Code::Pending)(&fx.ask(&format!("switch kbf={VC}"))));
    assert_eq!(
        fx.calls()
            .iter()
            .filter(|c| c.starts_with("switch"))
            .count(),
        1
    );
}

/// A release that did not come up: the platform's fault is the answer, the host remembers
/// that the update failed and why, and nothing is on trial.
#[test]
fn a_failed_switch_is_remembered_and_leaves_nothing_on_trial() {
    let fx = Fx::new("ab-switchfail");
    fx.install(VB);
    fx.world.borrow_mut().switch_fault = Some(Fault::failed(Code::SwitchFailed, "unit x"));
    assert!(failed(Code::SwitchFailed)(
        &fx.ask(&format!("switch kbf={VB}"))
    ));
    let view = fx.query();
    assert_eq!(view.pending, None);
    assert_eq!(view.last, last(VB, "failed", Some("switch_failed")));
    assert_eq!(view.commit.as_deref(), Some(A));
}

/// A confirm keeps the release on trial; one asked again after it was answered is answered
/// the same (the answer may have been lost); with nothing on trial and nothing confirmed it is
/// refused, and so is a confirm of a version other than the one on trial. A platform that
/// cannot clear its marker leaves the release on trial and says so.
#[test]
fn a_confirm_is_idempotent_and_only_for_the_release_on_trial() {
    let fx = Fx::new("ab-confirm");
    assert!(refused(Code::NothingPending)(
        &fx.ask(&format!("confirm kbf={VB}"))
    ));
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
    assert!(refused(Code::Pending)(
        &fx.ask(&format!("confirm kbf={VC}"))
    ));

    fx.world.borrow_mut().confirm_fault = Some(Fault::failed(Code::StateUnavailable, "disk"));
    assert!(failed(Code::StateUnavailable)(
        &fx.ask(&format!("confirm kbf={VB}"))
    ));
    assert!(fx.query().pending.is_some(), "still on trial");
    assert_eq!(fx.query().last, None);
    fx.world.borrow_mut().confirm_fault = None;

    let done = Answer::Confirmed { version: VB.into() };
    assert_eq!(fx.ask(&format!("confirm kbf={VB}")), done);
    assert_eq!(fx.ask(&format!("confirm kbf={VB}")), done, "a repeat");
    assert!(refused(Code::NothingPending)(
        &fx.ask(&format!("confirm kbf={VC}"))
    ));
}

/// A revert on request puts the host back on the release that ran, tells which one, and
/// remembers that the update failed because it was reverted; a revert with nothing on trial
/// is refused.
#[test]
fn a_revert_goes_back_and_remembers_it() {
    let fx = Fx::new("ab-revert");
    assert!(refused(Code::NothingPending)(&fx.ask("revert")));
    fx.install(VA);
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
    assert_eq!(
        fx.ask("revert"),
        Answer::Reverted {
            version: Some(VA.into())
        }
    );
    let view = fx.query();
    assert_eq!((view.pending, view.commit.as_deref()), (None, Some(A)));
    assert_eq!(view.last, last(VB, "failed", Some("reverted")));
    // The release stays laid out, but it is not staged for a switch any more.
    assert!(refused(Code::NotStaged)(
        &fx.ask(&format!("switch kbf={VB}"))
    ));
}

/// B never confirms: before the deadline the tick leaves the trial alone; at the deadline it
/// puts the host back on the release that ran, with no request and no help from the release
/// on trial, and the host then says the update failed by timeout.
#[test]
fn a_release_that_is_never_confirmed_is_reverted_at_the_deadline() {
    let fx = Fx::new("ab-timeout");
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
    fx.clock.set(1000 + WINDOW - 1);
    assert_eq!(fx.ab.tick(), None);
    assert!(fx.query().pending.is_some());
    fx.clock.set(1000 + WINDOW);
    assert_eq!(fx.ab.tick(), Some(Answer::Reverted { version: None }));
    let view = fx.query();
    assert_eq!((view.pending, view.commit.as_deref()), (None, Some(A)));
    assert_eq!(view.last, last(VB, "failed", Some("timeout")));
    assert_eq!(fx.ab.tick(), None, "nothing is left to do");
}

/// A host that went down with a release on trial: the marker is on disk, and a program
/// started after the deadline passed reverts at its first tick; one started before it waits
/// for the rest of the window.
#[test]
fn a_marker_found_at_start_is_reverted_once_its_deadline_has_passed() {
    let fx = Fx::new("ab-reboot");
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));

    fx.clock.set(1000 + WINDOW / 2);
    let early = fx.restarted();
    assert_eq!(early.tick(), None, "the window is not over");

    fx.clock.set(1000 + WINDOW + 5000);
    let late = fx.restarted();
    assert!(matches!(late.tick(), Some(Answer::Reverted { .. })));
    assert_eq!(fx.world.borrow().current.as_deref(), Some(A));
    let again = fx.restarted();
    let answer = again.handle(&LocalRequest::Query);
    let Answer::Queried(view) = answer else {
        panic!("{answer:?}")
    };
    assert_eq!(
        view.last,
        last(VB, "failed", Some("timeout")),
        "the new process knows"
    );
}

/// A swap armed by something else has no deadline of ours: it is left to the one that
/// armed it, and a plain revert request still goes back.
#[test]
fn a_trial_without_our_deadline_is_not_timed_out() {
    let fx = Fx::new("ab-foreign");
    fx.world.borrow_mut().pending = Some(Pending {
        commit: B.into(),
        from: A.into(),
        deadline_unix: None,
        activation: "5-1".into(),
    });
    fx.clock.set(u64::MAX);
    assert_eq!(fx.ab.tick(), None);
    let view = fx.query();
    assert_eq!(
        view.pending,
        Some(PendingView {
            version: None,
            commit: B.into(),
            deadline_unix: None
        })
    );
    assert!(matches!(fx.ask("revert"), Answer::Reverted { .. }));
}

/// A revert that could not move anything leaves the marker, and the next tick tries again; a
/// revert that put the release back and could not start it ends the update all the same, and
/// says it failed.
#[test]
fn a_failed_revert_is_retried_unless_it_already_moved_the_host() {
    let fx = Fx::new("ab-revertfail");
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
    fx.world.borrow_mut().revert_fault = Some(Fault::failed(Code::RevertFailed, "links"));
    fx.clock.set(5000);
    assert!(matches!(
        fx.ab.tick(),
        Some(Answer::Failed(Code::RevertFailed, _))
    ));
    assert!(fx.query().pending.is_some(), "the marker stays");
    assert_eq!(fx.query().last, None);
    fx.world.borrow_mut().revert_fault = None;
    assert!(matches!(fx.ab.tick(), Some(Answer::Reverted { .. })));

    let fx = Fx::new("ab-revertfail2");
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
    {
        let mut world = fx.world.borrow_mut();
        world.revert_fault = Some(Fault::failed(Code::RevertFailed, "unit did not start"));
        world.revert_clears = true;
    }
    assert!(failed(Code::RevertFailed)(&fx.ask("revert")));
    let view = fx.query();
    assert_eq!((view.pending, view.commit.as_deref()), (None, Some(A)));
    assert_eq!(view.last, last(VB, "failed", Some("reverted")));
}

/// Another install, switch or restart holds the install lock: a request that changes the
/// host is answered busy and does not run; a query still answers and says busy; the tick
/// leaves a due revert for the next time.
#[test]
fn a_held_install_lock_makes_a_request_busy_and_a_query_say_so() {
    let fx = Fx::new("ab-busy");
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
    let held = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(fx.dir.join("lock"))
        .unwrap();
    held.try_lock().unwrap();
    let calls = fx.calls().len();
    assert!(failed(Code::Busy)(&fx.install(VC)));
    assert!(failed(Code::Busy)(&fx.ask(&format!("confirm kbf={VB}"))));
    assert!(failed(Code::Busy)(&fx.ask("revert")));
    assert!(fx.query().busy);
    fx.clock.set(99_999);
    assert_eq!(fx.ab.tick(), None, "the next tick finds the lock free");
    assert_eq!(fx.calls().len(), calls);
    drop(held);
    crate::locks::wait_lockable(&fx.dir.join("lock"));
    assert!(matches!(fx.ab.tick(), Some(Answer::Reverted { .. })));
}

/// A state file that is not ours is not guessed at: the host says its records are
/// unavailable, and does not ask the platform for anything. A revert does not need it.
#[test]
fn an_unreadable_state_file_is_unavailable_and_a_revert_does_not_need_it() {
    let fx = Fx::new("ab-state");
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
    fs::write(fx.dir.join("state.json"), "{\"versions\": 3").unwrap();
    assert!(failed(Code::StateUnavailable)(&fx.ask("query")));
    assert!(failed(Code::StateUnavailable)(
        &fx.ask(&format!("confirm kbf={VB}"))
    ));
    assert!(failed(Code::StateUnavailable)(
        &fx.ask(&format!("switch kbf={VB}"))
    ));
    assert!(matches!(fx.ask("revert"), Answer::Reverted { .. }));
    // The revert wrote a fresh record.
    assert!(matches!(fx.ask("query"), Answer::Queried(_)));

    fs::remove_file(fx.dir.join("state.json")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/state", fx.dir.join("state.json")).unwrap();
    assert!(failed(Code::StateUnavailable)(&fx.ask("query")));
    let before = fx.calls().len();
    assert!(failed(Code::StateUnavailable)(&fx.install(VC)));
    assert_eq!(fx.calls().len(), before, "nothing was fetched on a guess");
}

/// A state file that cannot be written after the platform staged the release is a failure the
/// caller sees; the release stays laid out.
#[test]
fn a_state_file_that_cannot_be_written_fails_the_install() {
    let fx = Fx::new("ab-nowrite");
    let broken = Ab::new(
        Fake {
            world: Rc::clone(&fx.world),
            lock: fx.dir.join("lock"),
        },
        fx.dir.join("no-such-dir/state.json"),
        Trust::owned_by(fs::metadata(&*fx.dir).unwrap().uid(), &fx.dir),
        WINDOW,
        Box::new(|| 1),
    );
    let line = format!("install kbf={VB} from stable");
    let answer = broken.handle(&LocalRequest::parse(&line).unwrap());
    assert!(failed(Code::StateUnavailable)(&answer), "{answer:?}");
    assert!(fx.world.borrow().on_disk.contains(B));
}

/// The record of versions is bounded, and the commit just named is never the one forgotten.
#[test]
fn the_versions_remembered_are_bounded() {
    let fx = Fx::new("ab-bound");
    for n in 0..20_u32 {
        let commit = format!("{n:012x}{}", "0".repeat(28));
        let version = format!("0.1.{n}+g{n:012x}-1");
        fx.world.borrow_mut().current = Some(commit.clone());
        let answer = fx.install(&version);
        assert!(
            matches!(answer, Answer::Staged { current: true, .. }),
            "{answer:?}"
        );
        assert_eq!(fx.query().version.as_deref(), Some(version.as_str()));
    }
    let text = fs::read_to_string(fx.dir.join("state.json")).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(saved["versions"].as_object().unwrap().len(), 16);
}

/// A state file whose directory is not a directory cannot be looked at: that is the records being
/// unavailable, not a host with none.
#[test]
fn a_state_file_that_cannot_be_looked_at_is_unavailable() {
    let fx = Fx::new("ab-enotdir");
    fs::write(fx.dir.join("lock"), "a file").unwrap();
    let ab = Ab::new(
        Fake {
            world: Rc::clone(&fx.world),
            lock: fx.dir.join("lock2"),
        },
        fx.dir.join("lock/state.json"),
        Trust::owned_by(fs::metadata(&*fx.dir).unwrap().uid(), &fx.dir),
        WINDOW,
        Box::new(|| 1),
    );
    let answer = ab.handle(&LocalRequest::Query);
    assert!(failed(Code::StateUnavailable)(&answer), "{answer:?}");
}

/// A confirm asked again is answered only for the release that was confirmed: a host that runs
/// a release whose last update ended another way, or for another version, is not told it was
/// confirmed.
#[test]
fn a_repeated_confirm_is_for_the_release_that_was_confirmed() {
    let fx = Fx::new("ab-confirm-last");
    // The host learns that it runs VA, and VB failed by revert.
    fx.install(VA);
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
    fx.ask("revert");
    fx.install(VA);
    assert!(refused(Code::NothingPending)(
        &fx.ask(&format!("confirm kbf={VA}"))
    ));
    assert!(refused(Code::NothingPending)(
        &fx.ask(&format!("confirm kbf={VB}"))
    ));
}

#[path = "core_retain_tests.rs"]
mod retain;

#[path = "compat_tests.rs"]
mod compat;
