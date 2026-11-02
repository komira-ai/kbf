//! `activate` in the core, over a scripted backend: what it refuses before anything changes,
//! what it records, and what it tells when the host's records cannot be read or written.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::rc::Rc;

use kbf_retain::{CommitId, Kept};
use kbf_testkit::dir::TestDir;

use super::answer::{Answer, Code};
use super::backend::{Backend, Fault, Pending};
use super::core::Ab;
use super::core::activate::{How, LOCK_TAKES};
use super::request::{LocalRequest, Target};
use crate::request::{Channel, Version};
use crate::trust::Trust;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const VB: &str = "0.1.7+gbbbbbbbbbbbb-3";

/// What the host keeps and how its records answer.
#[derive(Default)]
struct Shelf {
    kept: Vec<Kept>,
    kept_fails: bool,
    current_fails: bool,
    activate_fault: Option<Fault>,
    retain_file: Option<PathBuf>,
    activated: Vec<String>,
}

#[derive(Clone)]
struct Host {
    shelf: Rc<RefCell<Shelf>>,
    lock: PathBuf,
}

impl Backend for Host {
    fn lock_file(&self) -> PathBuf {
        self.lock.clone()
    }
    fn node_id(&self) -> Option<String> {
        None
    }
    fn current(&self) -> Result<Option<String>, Fault> {
        if self.shelf.borrow().current_fails {
            return Err(Fault::failed(Code::StateUnavailable, "links"));
        }
        Ok(Some(A.to_owned()))
    }
    fn pending(&self) -> Result<Option<Pending>, Fault> {
        Ok(None)
    }
    fn has(&self, _commit: &str) -> bool {
        true
    }
    fn stage(&self, _version: &Version, _channel: &Channel) -> Result<String, Fault> {
        Err(Fault::refused(Code::UnknownRelease, "not here"))
    }
    fn switch(&self, _commit: &str, _window_secs: u64, _now: u64) -> Result<u64, Fault> {
        Err(Fault::refused(Code::NotStaged, "not here"))
    }
    fn confirm(&self, _pending: &Pending) -> Result<(), Fault> {
        Ok(())
    }
    fn revert(&self, _pending: &Pending) -> Result<(), Fault> {
        Ok(())
    }
    fn activate(&self, commit: &str) -> Result<(), Fault> {
        let mut shelf = self.shelf.borrow_mut();
        shelf.activated.push(commit.to_owned());
        shelf.activate_fault.clone().map_or(Ok(()), Err)
    }
    fn kept(&self) -> Result<Vec<Kept>, Fault> {
        let shelf = self.shelf.borrow();
        if shelf.kept_fails {
            return Err(Fault::failed(Code::StateUnavailable, "releases/"));
        }
        Ok(shelf.kept.clone())
    }
    fn retain_file(&self) -> Option<PathBuf> {
        self.shelf.borrow().retain_file.clone()
    }
}

fn kept(commit: &str, version: Option<&str>) -> Kept {
    Kept {
        commit: CommitId::new(commit).unwrap(),
        version: version.map(str::to_owned),
        why: Vec::new(),
        version_left_out: false,
    }
}

struct Rig {
    dir: TestDir,
    shelf: Rc<RefCell<Shelf>>,
    notes: Rc<RefCell<Vec<String>>>,
}

impl Rig {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
        let shelf = Rc::new(RefCell::new(Shelf {
            kept: vec![kept(B, None), kept(A, None)],
            ..Shelf::default()
        }));
        Self {
            dir,
            shelf,
            notes: Rc::default(),
        }
    }

    fn ab(&self, hold: Option<PathBuf>) -> Ab<Host> {
        let uid = fs::metadata(&*self.dir).unwrap().uid();
        let notes = Rc::clone(&self.notes);
        let ab = Ab::new(
            Host {
                shelf: Rc::clone(&self.shelf),
                lock: self.dir.join("lock"),
            },
            self.dir.join("state.json"),
            Trust::owned_by(uid, &self.dir),
            600,
            Box::new(|| 7),
        )
        .keeping(2, Box::new(move |t| notes.borrow_mut().push(t.to_owned())));
        match hold {
            Some(hold) => ab.holding(hold),
            None => ab,
        }
    }

    fn activated(&self) -> Vec<String> {
        self.shelf.borrow().activated.clone()
    }
}

/// The program's own state file, root's (here the test's) and 0600, as `text`.
fn put_state(rig: &Rig, text: &str) {
    let path = rig.dir.join("state.json");
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn target(text: &str) -> Target {
    Target::parse(text).unwrap()
}

const PIN: How = How {
    pin: true,
    force_lock: false,
};

/// The version text this program staged a release as names it when the host recorded none, and
/// is told back; one the listing left out as too long is not filled in. A `staged` that names
/// the release is cleared, one that names another stays.
///
/// Catches: a release that cannot be named by the text this program knows it by; `staged` left
/// naming the release that runs.
#[test]
fn the_state_names_the_release_and_a_staged_one_is_cleared() {
    let rig = Rig::new("ca-state");
    put_state(
        &rig,
        &format!(
            "{{\"versions\":{{\"{B}\":\"{VB}\"}},\"staged\":{{\"version\":\"{VB}\",\"commit\":\"{B}\"}}}}"
        ),
    );
    let ab = rig.ab(None);
    assert_eq!(
        ab.activate(&target(VB), How::default()),
        Answer::Activated {
            commit: CommitId::new(B).unwrap(),
            version: Some(VB.to_owned()),
            from: Some(CommitId::new(A).unwrap()),
            pinned: false,
            lock_taken: false,
        }
    );
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(rig.dir.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["staged"], serde_json::Value::Null);
    assert_eq!(
        state["last"],
        serde_json::json!({"target": VB, "ended": "succeeded", "why": null})
    );

    // Left out of the listing as too long: the state's text is not put back, so the version does
    // not name it, and the staged one (another release) stays.
    let rig = Rig::new("ca-leftout");
    rig.shelf.borrow_mut().kept[0].version_left_out = true;
    put_state(
        &rig,
        &format!(
            "{{\"versions\":{{\"{B}\":\"{VB}\"}},\"staged\":{{\"version\":\"9-1\",\"commit\":\"{A}\"}}}}"
        ),
    );
    let ab = rig.ab(None);
    assert_eq!(
        ab.activate(&target("9.9-1"), How::default()).json_line(),
        format!(
            "{{\"result\":\"refused\",\"reason\":\"not_kept\",\"releases\":[{{\"commit\":\"{B}\",\"version\":null,\"version_left_out\":true}},{{\"commit\":\"{A}\",\"version\":null}}]}}"
        )
    );
    assert!(matches!(
        ab.activate(&target(B), How::default()),
        Answer::Activated { .. }
    ));
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(rig.dir.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["staged"]["commit"], A);
    assert_eq!(state["last"]["target"], B, "no text: the commit");
}

/// What is refused or fails before anything changes: a pin with no hold file, a lock file that
/// cannot be made, a state file that cannot be read, a listing or links that cannot be read, a
/// take of the lock that cannot be recorded. The backend is never asked to swap.
///
/// Catches: a swap after the host's records could not be read; a take of the lock that is not
/// recorded and goes ahead.
#[test]
fn what_cannot_be_read_or_recorded_stops_the_activate_before_the_swap() {
    let rig = Rig::new("ca-stops");
    let ab = rig.ab(None);
    assert_eq!(
        ab.activate(&target(B), PIN),
        Answer::Refused(Code::ConfigInvalid, "this host has no hold file".to_owned())
    );

    let host_ab = Ab::new(
        Host {
            shelf: Rc::clone(&rig.shelf),
            lock: rig.dir.join("no/such/dir/lock"),
        },
        rig.dir.join("state.json"),
        Trust::owned_by(fs::metadata(&*rig.dir).unwrap().uid(), &rig.dir),
        600,
        Box::new(|| 7),
    );
    let answer = host_ab.activate(&target(B), How::default());
    assert_eq!(
        answer.json_line(),
        "{\"result\":\"failed\",\"reason\":\"state_unavailable\"}"
    );
    assert!(answer.log_detail().unwrap().contains("no/such/dir/lock"));

    fs::write(rig.dir.join("state.json"), "not json").unwrap();
    assert!(matches!(
        ab.activate(&target(B), How::default()),
        Answer::Failed(Code::StateUnavailable, _)
    ));
    fs::remove_file(rig.dir.join("state.json")).unwrap();

    rig.shelf.borrow_mut().kept_fails = true;
    assert!(matches!(
        ab.activate(&target(B), How::default()),
        Answer::Failed(Code::StateUnavailable, _)
    ));
    rig.shelf.borrow_mut().kept_fails = false;

    rig.shelf.borrow_mut().current_fails = true;
    assert!(matches!(
        ab.activate(&target(B), How::default()),
        Answer::Failed(Code::StateUnavailable, _)
    ));
    rig.shelf.borrow_mut().current_fails = false;

    // A record of takes that cannot be appended to (a directory in its place).
    fs::create_dir(rig.dir.join(LOCK_TAKES)).unwrap();
    let _holder = crate::host::take_lock(&rig.dir.join("lock")).unwrap();
    let force = How {
        pin: false,
        force_lock: true,
    };
    let answer = ab.activate(&target(B), force);
    assert!(
        matches!(&answer, Answer::Failed(Code::StateUnavailable, d)
            if d.starts_with("the take of the install lock cannot be recorded: lock-takes.jsonl: ")),
        "{answer:?}"
    );
    assert!(rig.activated().is_empty(), "never asked to swap");
}

/// A hold file whose directory is not there (no agent; a path with no directory) is refused
/// before anything changes. A refused swap pins nothing. A hold file that cannot be written once
/// the links moved (something the agent's account made in the way that cannot be removed), or a
/// release that cannot be pinned in `retain.json`, fails: the release runs and the answer says it
/// is not pinned. A host with no `retain.json` writes only the hold file.
///
/// Catches: a hold file naming a release the host never swapped to; a pin that failed reported
/// as done.
#[test]
fn a_pin_follows_the_swap_and_a_hold_directory_that_is_not_there_is_refused_first() {
    let rig = Rig::new("ca-pin");
    for hold in [rig.dir.join("no-agent/pin.json"), PathBuf::from("/")] {
        let answer = rig.ab(Some(hold.clone())).activate(&target(B), PIN);
        assert!(
            matches!(&answer, Answer::Refused(Code::ConfigInvalid, d)
                if d.ends_with("is not there: without an agent, leave out --pin")),
            "{hold:?}: {answer:?}"
        );
    }
    assert!(rig.activated().is_empty(), "never asked to swap");

    let agent = rig.dir.join("agent");
    fs::create_dir_all(&agent).unwrap();
    rig.shelf.borrow_mut().activate_fault = Some(Fault::refused(Code::Busy, "an apply runs"));
    let answer = rig
        .ab(Some(agent.join("pin.json")))
        .activate(&target(B), PIN);
    assert_eq!(
        answer,
        Answer::Refused(Code::Busy, "an apply runs".to_owned())
    );
    assert!(
        !agent.join("pin.json").exists(),
        "a refused swap pins nothing"
    );
    rig.shelf.borrow_mut().activate_fault = None;

    let not_pinned = |ab: &Ab<Host>| {
        let answer = ab.activate(&target(B), PIN);
        assert!(
            matches!(&answer, Answer::Failed(Code::StateUnavailable, d)
                if d.starts_with(&format!("{B} runs now, but it is not pinned: "))),
            "{answer:?}"
        );
    };
    fs::create_dir_all(agent.join(".pin.json.new/full")).unwrap();
    not_pinned(&rig.ab(Some(agent.join("pin.json"))));
    // A hold path with a directory and no name (`<agent>/..`).
    not_pinned(&rig.ab(Some(agent.join(".."))));
    fs::remove_dir_all(agent.join(".pin.json.new")).unwrap();
    rig.shelf.borrow_mut().retain_file = Some(rig.dir.join("no/such/retain.json"));
    not_pinned(&rig.ab(Some(agent.join("pin.json"))));
    assert_eq!(
        rig.activated(),
        [B, B, B, B],
        "the refused swap was asked, then three were done"
    );
    assert!(
        rig.notes
            .borrow()
            .iter()
            .all(|n| n.starts_with("activate: the pin: "))
    );

    rig.shelf.borrow_mut().retain_file = None;
    let answer = rig
        .ab(Some(agent.join("pin.json")))
        .activate(&target(B), PIN);
    assert!(
        matches!(answer, Answer::Activated { pinned: true, .. }),
        "{answer:?}"
    );
    assert!(agent.join("pin.json").exists());
}

/// A take of the lock is recorded at once, whatever the activate does next: a release that is
/// not kept, after the lock was taken from its holder, leaves the record.
///
/// Catches: a lock taken from its holder with no trace when the activate then stops.
#[test]
fn a_take_of_the_lock_is_recorded_even_when_the_activate_then_stops() {
    let rig = Rig::new("ca-take");
    let _holder = crate::host::take_lock(&rig.dir.join("lock")).unwrap();
    let force = How {
        pin: false,
        force_lock: true,
    };
    let answer = rig.ab(None).activate(&target("cccccccccccc"), force);
    assert!(
        matches!(
            answer,
            Answer::Unresolved {
                code: Code::NotKept,
                ..
            }
        ),
        "{answer:?}"
    );
    let record = fs::read_to_string(rig.dir.join(LOCK_TAKES)).unwrap();
    assert_eq!(
        record,
        format!(
            "{{\"at_unix\":7,\"by\":\"activate\",\"target\":\"cccccccccccc\",\
             \"holder_pid\":{},\"holder_alive\":true}}\n",
            std::process::id()
        )
    );
    assert!(rig.activated().is_empty());
}

/// Units that do not come up after the links moved: the failure is told, and the state still
/// records the release's version and clears a `staged` that names it, keeping `last`.
///
/// Catches: a state that forgets the swap because the restart failed.
#[test]
fn a_failed_restart_still_records_the_swap_in_the_state() {
    let rig = Rig::new("ca-restart");
    rig.shelf.borrow_mut().kept[0].version = Some(VB.to_owned());
    put_state(
        &rig,
        &format!("{{\"staged\":{{\"version\":\"{VB}\",\"commit\":\"{B}\"}}}}"),
    );
    rig.shelf.borrow_mut().activate_fault = Some(Fault::failed(Code::RestartFailed, "down"));
    let answer = rig.ab(None).activate(&target(B), How::default());
    assert_eq!(
        answer,
        Answer::Failed(Code::RestartFailed, "down".to_owned())
    );
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(rig.dir.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["staged"], serde_json::Value::Null);
    assert_eq!(state["versions"][B], VB);
    assert_eq!(state["last"], serde_json::Value::Null);
}

/// A backend that cannot swap tells it; a state file that cannot be written after the swap is
/// told to the log, and the answer stands. A backend that keeps no release directories refuses
/// through the socket as well (`not_kept`, with nothing to list).
///
/// Catches: a swap reported as failed because the state file could not be written.
#[test]
fn what_the_backend_says_is_told_and_a_lost_state_is_only_noted() {
    let rig = Rig::new("ca-backend");
    rig.shelf.borrow_mut().activate_fault = Some(Fault::failed(
        Code::RestartFailed,
        "the units did not come up",
    ));
    let ab = rig.ab(None);
    assert_eq!(
        ab.activate(&target(B), How::default()),
        Answer::Failed(Code::RestartFailed, "the units did not come up".to_owned())
    );
    rig.shelf.borrow_mut().activate_fault = None;

    fs::create_dir_all(rig.dir.join("state.json.new/full")).unwrap();
    assert!(matches!(
        ab.activate(&target(B), How::default()),
        Answer::Activated { .. }
    ));
    assert_eq!(rig.notes.borrow().len(), 1);
    assert!(
        rig.notes.borrow()[0].starts_with("activate: state.json: "),
        "{:?}",
        rig.notes
    );

    let answer = ab.handle(&LocalRequest::parse(&format!("activate kbf={B}")).unwrap());
    assert!(matches!(answer, Answer::Activated { .. }), "{answer:?}");

    // The trait's own answer, for a backend that keeps no release directories.
    let fake = NoReleases(rig.dir.join("fake.lock"));
    assert_eq!(
        fake.activate(B).unwrap_err(),
        Fault::refused(
            Code::NotKept,
            "this host keeps no release directories to activate"
        )
    );
    let uid = fs::metadata(&*rig.dir).unwrap().uid();
    let ab = Ab::new(
        fake,
        rig.dir.join("fake.json"),
        Trust::owned_by(uid, &rig.dir),
        600,
        Box::new(|| 7),
    );
    assert_eq!(
        ab.handle(&LocalRequest::parse(&format!("activate kbf={B}")).unwrap())
            .json_line(),
        "{\"result\":\"refused\",\"reason\":\"not_kept\",\"releases\":[]}"
    );
}

/// The hold file is published only as the file written: an owner of the directory that swaps
/// the file aside for its own (or a link) between the write and the rename makes the write an
/// error, not a published hold; one that removes it, too. Untouched, the name holds the text,
/// root's (here the test's), 0644, a regular file.
///
/// Catches: a hold reported as written while the name holds what the directory's owner put there.
#[test]
fn a_hold_swapped_aside_before_its_rename_is_an_error() {
    use super::core::activate::write_hold_for_test;
    use rustix::fs::{AtFlags, Mode, OFlags};
    let rig = Rig::new("ca-swap");
    let dir = rig.dir.join("agent");
    fs::create_dir_all(&dir).unwrap();
    let hold = dir.join("pin.json");
    let swap = |dirfd: &rustix::fd::OwnedFd| {
        rustix::fs::unlinkat(dirfd, ".pin.json.new", AtFlags::empty()).unwrap();
        let theirs = rustix::fs::openat(
            dirfd,
            ".pin.json.new",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(0o644),
        )
        .unwrap();
        rustix::io::write(&theirs, b"{\"commit\":\"theirs\"}").unwrap();
    };
    let err = write_hold_for_test(&hold, "{\"commit\":\"ours\"}", &swap).unwrap_err();
    assert_eq!(
        err.to_string(),
        "the hold file was replaced while it was written"
    );

    let link = |dirfd: &rustix::fd::OwnedFd| {
        rustix::fs::unlinkat(dirfd, ".pin.json.new", AtFlags::empty()).unwrap();
        rustix::fs::symlinkat("/etc/hostname", dirfd, ".pin.json.new").unwrap();
    };
    let err = write_hold_for_test(&hold, "{\"commit\":\"ours\"}", &link).unwrap_err();
    assert_eq!(
        err.to_string(),
        "the hold file was replaced while it was written"
    );

    let gone = |dirfd: &rustix::fd::OwnedFd| {
        rustix::fs::unlinkat(dirfd, ".pin.json.new", AtFlags::empty()).unwrap();
    };
    assert!(write_hold_for_test(&hold, "{\"commit\":\"ours\"}", &gone).is_err());

    write_hold_for_test(&hold, "{\"commit\":\"ours\"}", &|_| {}).unwrap();
    let meta = fs::symlink_metadata(&hold).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.permissions().mode() & 0o7777, 0o644);
    assert_eq!(meta.uid(), fs::metadata(&*rig.dir).unwrap().uid());
    assert_eq!(fs::read_to_string(&hold).unwrap(), "{\"commit\":\"ours\"}");
}

/// A backend that keeps no release directories and says nothing of its own about `activate`.
struct NoReleases(PathBuf);

impl Backend for NoReleases {
    fn lock_file(&self) -> PathBuf {
        self.0.clone()
    }
    fn node_id(&self) -> Option<String> {
        None
    }
    fn current(&self) -> Result<Option<String>, Fault> {
        Ok(None)
    }
    fn pending(&self) -> Result<Option<Pending>, Fault> {
        Ok(None)
    }
    fn has(&self, _commit: &str) -> bool {
        false
    }
    fn stage(&self, _version: &Version, _channel: &Channel) -> Result<String, Fault> {
        Err(Fault::refused(Code::UnknownRelease, "-"))
    }
    fn switch(&self, _commit: &str, _window_secs: u64, _now: u64) -> Result<u64, Fault> {
        Err(Fault::refused(Code::NotStaged, "-"))
    }
    fn confirm(&self, _pending: &Pending) -> Result<(), Fault> {
        Ok(())
    }
    fn revert(&self, _pending: &Pending) -> Result<(), Fault> {
        Ok(())
    }
}
