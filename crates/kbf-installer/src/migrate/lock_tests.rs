//! The two locks of an apply (the old tools', then the installer's): the look the writes are
//! made from is taken while both are held, they are held to the end, they are waited for only
//! so long, and nothing is asked of the frontend while they are held. The first two tests
//! hold a lock from another thread while the verb waits for it, which is the only way to see
//! on which side of the lock the second look falls.

use std::cell::{Cell, RefCell};
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use super::floor::{FrontendFloor, Seed, Source};
use super::moved_tests::{assert_nothing_migrated, never};
use super::old::OldLock;
use super::rig::*;
use super::*;
use crate::layout::Platform;
use crate::state::{Locked, State, StateError};
use crate::testkit::{A, B, C};

/// How long a holder lets the apply wait before it changes the host and lets go. Long
/// enough that an apply which looked at the host without waiting has looked by then; the
/// apply that does wait is held up for exactly this long and no longer, so a slow machine
/// cannot fail a correct one.
const PAUSE: Duration = Duration::from_millis(300);

/// Whether nobody holds the `flock` on `path` (a lock file that is not there is free).
fn is_free(path: &Path) -> bool {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return true,
        Err(e) => panic!("{}: {e}", path.display()),
    };
    match file.try_lock() {
        Ok(()) => {
            file.unlock().expect("unlock");
            true
        }
        Err(TryLockError::WouldBlock) => false,
        Err(TryLockError::Error(e)) => panic!("{}: {e}", path.display()),
    }
}

fn old_lock_path(rig: &Rig) -> PathBuf {
    rig.layout.join(&rig.roots().old_lock)
}

fn installer_lock_path(rig: &Rig) -> PathBuf {
    rig.layout.join(&rig.layout.lock())
}

/// Catches a second look made before the old tools' lock is held (moved above it, or the
/// lock not taken at all): an old verb is mid-way under its lock when the apply has probed
/// the releases, and finishes (a swap back) while the apply waits. The look that the
/// writes are made from must be the one after the wait, so the run ends `Changed` (exit 5)
/// with nothing migrated; a look made before the wait sees the old state `A` and migrates
/// it over the swap.
///
/// Mutants seen red: the second look and its comparison moved above `OldLock::take` (with
/// no look after it); `OldLock::take` removed; the lock let go as soon as it is taken.
#[test]
fn the_second_look_is_made_after_the_wait_for_the_old_tools_lock() {
    let mut rig = Rig::linux("look-after-old-lock");
    let (hand, layout, roots) = (rig.old_hand(), rig.layout.clone(), rig.roots());
    let held = OldLock::take(&layout, &roots, Duration::from_secs(5)).expect("the old lock");
    let (probed, told) = mpsc::channel::<()>();
    let verb = thread::spawn(move || {
        told.recv().expect("the apply has probed");
        thread::sleep(PAUSE);
        hand.swap_back();
        drop(held);
    });
    let runner = runner_between(move || probed.send(()).expect("the old verb is waiting"));
    let started = Instant::now();
    let result = run_with(&mut rig, &runner, &never(), &NoFrontend, Mode::Apply);
    verb.join().expect("the old verb ends");

    let error = result.expect_err("the swap back was seen");
    assert!(
        matches!(&error, MigrateError::Changed(why) if why.contains(&format!("current was {A} and is now {B}"))),
        "{error}"
    );
    assert!(started.elapsed() >= PAUSE, "the apply waited for the lock");
    assert_nothing_migrated(&rig);
}

/// Catches a second look made before the installer's lock is held: another run of the
/// installer holds it, and writes its state (a release activated) while the apply waits.
/// The run must see that state after the wait and refuse (the conflict of the first look);
/// a look made before it finds none, and the apply writes over the installer's state.
///
/// Mutants seen red: the second look made before the installer's lock is acquired (the
/// old tools' lock still taken first); the second look made before both locks; the
/// installer's lock taken only after the stops.
#[test]
fn the_second_look_is_made_after_the_wait_for_the_installers_lock() {
    let mut rig = Rig::linux("look-after-installer-lock");
    let (layout, dir) = (rig.layout.clone(), rig.new_state_dir());
    let held = Locked::acquire(&layout).expect("the installer's lock");
    let (probed, told) = mpsc::channel::<()>();
    let run = thread::spawn(move || {
        told.recv().expect("the apply has probed");
        thread::sleep(PAUSE);
        fs::create_dir_all(&dir).expect("the installer's directory");
        let state = State {
            current: Some(commit(C)),
            ..State::default()
        };
        held.write(&state).expect("the installer's state");
        drop(held);
    });
    let runner = runner_between(move || probed.send(()).expect("the installer is waiting"));
    let result = run_with(&mut rig, &runner, &never(), &NoFrontend, Mode::Apply);
    run.join().expect("the installer ends");

    let error = result.expect_err("the installer's state was seen");
    assert!(matches!(error, MigrateError::Conflict(_)), "{error}");
    let state = rig.json(&rig.new_state_dir().join("state.json"));
    assert_eq!(state["current"], json!(C), "the installer's state is kept");
    assert!(rig.events.borrow().is_empty(), "no service was touched");
}

/// Catches a migration that queues for ever behind an old verb: with the old tools' lock
/// held past its wait, the verb ends busy (exit 4) naming the lock, after waiting that long
/// and not much longer, with nothing changed.
///
/// Red before the deadline existed (the verb waited for the holder, which lets go after
/// five seconds, and migrated). Mutant seen red: the lock let go as soon as it is taken.
#[test]
fn an_old_tools_lock_held_past_the_wait_ends_the_run_busy() {
    let mut rig = Rig::linux("busy-old-lock");
    rig.lock_wait = Duration::from_millis(200);
    let (layout, roots) = (rig.layout.clone(), rig.roots());
    let held = OldLock::take(&layout, &roots, Duration::from_secs(5)).expect("the old lock");
    let (ended, told) = mpsc::channel::<()>();
    let verb = thread::spawn(move || {
        let _ = told.recv_timeout(Duration::from_secs(5));
        drop(held);
    });
    let started = Instant::now();
    let result = run_migrate(&mut rig, &never(), Mode::Apply);
    let waited = started.elapsed();
    // (A verb that has let go already, because the run waited it out, is not told.)
    let _ = ended.send(());
    verb.join().expect("the old verb ends");

    let error = result.expect_err("busy");
    assert!(
        matches!(&error, MigrateError::State(StateError::Busy { path }) if path.ends_with("kbf-deploy.lock")),
        "{error}"
    );
    assert_eq!(error.exit().code(), 4);
    assert!(waited >= Duration::from_millis(200), "waited {waited:?}");
    assert!(waited < Duration::from_secs(3), "waited {waited:?}");
    assert_nothing_migrated(&rig);
}

/// Catches the same for the installer's lock, which is waited for with the old tools' lock
/// already held (so an old verb is held up too while it is waited for): busy (exit 4)
/// naming the lock, and the old tools' lock is let go with the error.
///
/// Red before the deadline existed (the verb waited the holder out and migrated; the busy
/// error is exit 4 by the table in refusal_tests). Mutant seen red: the installer's lock
/// taken only after the stops.
#[test]
fn an_installers_lock_held_past_the_wait_ends_the_run_busy_and_lets_go_of_the_old_lock() {
    let mut rig = Rig::linux("busy-installer-lock");
    rig.lock_wait = Duration::from_millis(200);
    let held = Locked::acquire(&rig.layout).expect("the installer's lock");
    let (ended, told) = mpsc::channel::<()>();
    let other = thread::spawn(move || {
        let _ = told.recv_timeout(Duration::from_secs(5));
        drop(held);
    });
    let result = run_migrate(&mut rig, &never(), Mode::Apply);
    let old_free = is_free(&old_lock_path(&rig));
    let _ = ended.send(());
    other.join().expect("the installer ends");

    let error = result.expect_err("busy");
    assert!(
        matches!(&error, MigrateError::State(StateError::Busy { path }) if path.ends_with("kbf-installer/lock")),
        "{error}"
    );
    assert_eq!(error.exit().code(), 4);
    assert!(old_free, "the old tools' lock was let go");
    assert_nothing_migrated(&rig);
}

/// Catches an old-tools lock that is not the one the deploy kit takes. The kit's `Locked`
/// (kbf-deploy `host/state.rs`, and the dev installer's `state.rs` that took over from it)
/// opens `/run/kbf-deploy.lock` (macOS: `/var/run/kbf-deploy.lock`) and takes `flock(2)` on
/// it, which is what `File::lock` does: so a handle opened that way while the migration
/// holds the lock must be refused, and be let in once the lock is dropped. Which builds a
/// host actually runs is a check on the host, before the first migration.
///
/// Mutants seen red: the lock taken on another file; the lock let go as soon as it is taken.
#[test]
fn the_old_lock_is_the_flock_the_deploy_kit_takes_on_its_path() {
    let rig = Rig::linux("old-lock-is-flock");
    assert_eq!(
        rig.roots().old_lock.as_str(),
        "/run/kbf-deploy.lock",
        "the kit's path on Linux"
    );
    assert_eq!(
        Roots::new(Platform::MacOs).old_lock.as_str(),
        "/var/run/kbf-deploy.lock",
        "and on macOS"
    );
    let path = old_lock_path(&rig);
    assert!(is_free(&path), "free before");
    let held = OldLock::take(&rig.layout, &rig.roots(), Duration::from_secs(5)).expect("take");

    let kit = File::options()
        .read(true)
        .write(true)
        .open(&path)
        .expect("the file is where the kit has it");
    assert!(
        matches!(kit.try_lock(), Err(TryLockError::WouldBlock)),
        "held: the kit's flock is refused"
    );
    drop(held);
    kit.try_lock().expect("free once dropped");
}

/// Catches a network call made while the locks are held, and made twice: with the frontend
/// as the release source the floor's generation is asked of it once, before the old tools'
/// lock and the installer's are taken (an answer that is slow would otherwise hold up every
/// old verb and every verb of the installer), and the answer is what the floor is seeded
/// with under the locks.
///
/// Red before the claim was made once (asked twice, with both locks held the second time).
/// Mutant seen red: the frontend asked again by the second look.
#[test]
fn the_frontend_is_asked_once_and_with_no_lock_held() {
    struct Watcher {
        asked: Cell<u32>,
        free: RefCell<Vec<(bool, bool)>>,
        old: PathBuf,
        installer: PathBuf,
    }
    impl FrontendFloor for Watcher {
        fn current(&self) -> Result<(u64, u64), String> {
            self.asked.set(self.asked.get() + 1);
            self.free
                .borrow_mut()
                .push((is_free(&self.old), is_free(&self.installer)));
            Ok((3, 30))
        }
    }

    let mut rig = Rig::linux("frontend-once");
    let watcher = Watcher {
        asked: Cell::new(0),
        free: RefCell::new(Vec::new()),
        old: old_lock_path(&rig),
        installer: installer_lock_path(&rig),
    };
    let m = Migration {
        seed: Seed {
            source: Source::Frontend,
            min_generation: None,
        },
        ..rig.migration()
    };
    let report = migrate(
        &m,
        &mut rig.host,
        &Hooks {
            runner: &runner(),
            verifier: &never(),
            frontend: &watcher,
        },
        Mode::Apply,
    )
    .expect("migrate");

    assert_eq!(report.outcome, Outcome::Migrated);
    assert_eq!(watcher.asked.get(), 1, "asked once");
    assert_eq!(
        *watcher.free.borrow(),
        vec![(true, true)],
        "with neither lock held"
    );
    let state = rig.json(&rig.new_state_dir().join("state.json"));
    assert_eq!(
        state["floor"],
        json!({"epoch": 3, "order_id": 42, "generation": 30}),
        "the floor is the memory's pair with the frontend's generation"
    );
}

/// Catches a lock let go before the work is done: every service command of an apply (the
/// stops, the reload, the starts) is made with both locks held, as are the writes between
/// them, so no old verb and no verb of the installer changes the host half way through.
///
/// Mutants seen red: the old tools' lock let go after the second look; the installer's lock
/// taken only after the stops; the old tools' lock taken on another file or let go at once.
#[test]
fn both_locks_are_held_for_every_service_command() {
    let mut rig = Rig::linux("locks-held-to-the-end");
    let (old, installer) = (old_lock_path(&rig), installer_lock_path(&rig));
    let seen = Rc::new(RefCell::new(Vec::<String>::new()));
    rig.on_command({
        let seen = seen.clone();
        move |verb, unit| {
            seen.borrow_mut().push(format!(
                "{verb} {unit}: old held={} installer held={}",
                !is_free(&old),
                !is_free(&installer)
            ));
        }
    });
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");

    let seen = seen.borrow();
    for verb in ["is-active", "stop", "disable", "daemon-reload", "start"] {
        assert!(
            seen.iter().any(|line| line.starts_with(verb)),
            "{verb} was not seen: {seen:?}"
        );
    }
    for line in seen.iter() {
        assert!(
            line.ends_with("old held=true installer held=true"),
            "{line}"
        );
    }
}
