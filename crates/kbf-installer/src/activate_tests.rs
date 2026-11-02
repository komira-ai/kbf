use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, symlink};
use std::process::Output;
use std::time::{Duration, UNIX_EPOCH};

use kbf_host::exec::{Entry, Exec, Meta};
use kbf_host::path::{HostPath, Root};
use kbf_host::step::{Cmd, HttpUrl, Step};
use kbf_host::testkit::ScratchHost;
use kbf_host::trust::Untrusted;

use super::*;
use crate::release;
use crate::state::{
    self,
    tests::{A, B, C, c},
};

const D: &str = "dddddddddddddddddddddddddddddddddddddddd";
const E: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

/// A scratch host with the directories a Linux host has above the layout.
fn host(name: &str) -> (ScratchHost, Layout) {
    let exec = ScratchHost::new(&format!("activate-{name}"));
    for sub in [
        "run",
        "var/lib/kbf-installer",
        "var/lib/kbf-installer/releases",
    ] {
        fs::create_dir_all(exec.root.join(sub)).expect("seed");
    }
    let layout = Layout::linux(Root::new(&exec.root).expect("absolute"));
    (exec, layout)
}

/// An installed release: a directory named for the commit holding `kbf`, which holds
/// the commit's name. `age` orders the directories for `gc`.
fn seed(exec: &ScratchHost, commit: &str, age: u64) {
    let dir = exec.at(&format!("/var/lib/kbf-installer/releases/{commit}"));
    fs::create_dir_all(&dir).expect("dir");
    fs::write(dir.join("kbf"), commit).expect("kbf");
    fs::File::open(&dir)
        .expect("open")
        .set_modified(UNIX_EPOCH + Duration::from_secs(age))
        .expect("mtime");
}

/// A host that runs `commit` (confirmed), with the releases `also` installed beside it.
fn running(exec: &ScratchHost, layout: &Layout, commit: &str, also: &[&str]) {
    for (i, release) in std::iter::once(&commit).chain(also).enumerate() {
        seed(exec, release, 1_000 + i as u64);
    }
    // The state first, as a swap writes it: a link with no state is a lost state (S21).
    Locked::acquire(layout)
        .expect("lock")
        .write(&State {
            current: Some(c(commit)),
            ..State::default()
        })
        .expect("state");
    symlink(commit, exec.at("/var/lib/kbf-installer/releases/current")).expect("link");
}

/// An order for `commit`. The third argument is only a label for the reader of a test
/// (the number of the order in the story); an order carries no serial.
fn order(commit: &str, id: &str, _label: u64) -> Order {
    Order {
        commit: c(commit),
        order_id: id.to_owned(),
    }
}

const START: Arm = Arm::FromFirstStart { starts: 3 };

fn go(exec: &mut ScratchHost, layout: &Layout, o: &Order, arm: Arm) -> Result<Activated, String> {
    activate(exec, layout, o, arm, 1_000, &mut Vec::new()).map_err(|e| e.to_string())
}

fn state_of(layout: &Layout) -> State {
    state::read_unlocked(layout)
        .expect("read")
        .expect("a state")
}

/// Where a link under `/var/lib/kbf-installer/releases` points; `None` when there is no link.
fn link(exec: &ScratchHost, name: &str) -> Option<String> {
    let target = fs::read_link(exec.at(&format!("/var/lib/kbf-installer/releases/{name}"))).ok()?;
    Some(target.display().to_string())
}

fn at(commit: &str) -> Option<String> {
    Some(commit.to_owned())
}

/// How many link steps the host has applied.
fn link_steps(exec: &ScratchHost) -> usize {
    exec.log
        .iter()
        .filter(|s| matches!(s, Step::Symlink { .. }))
        .count()
}

/// A host that is killed: it applies `left` steps, then every step fails, the way a
/// process killed between two steps never reaches the second.
struct Killed<'a> {
    host: &'a mut ScratchHost,
    left: usize,
}

impl Exec for Killed<'_> {
    fn euid(&self) -> u32 {
        self.host.euid()
    }

    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>> {
        self.host.inspect(path)
    }

    fn stat(&self, path: &HostPath) -> io::Result<Option<Meta>> {
        self.host.stat(path)
    }

    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        self.host.query(cmd)
    }

    fn http_status(&self, url: &HttpUrl) -> io::Result<u16> {
        self.host.http_status(url)
    }

    fn apply(&mut self, step: &Step) -> io::Result<()> {
        if self.left == 0 {
            return Err(io::Error::other("killed"));
        }
        self.left -= 1;
        self.host.apply(step)
    }

    fn check_binary(&self) -> Result<(), Untrusted> {
        self.host.check_binary()
    }
}

/// Catches an activation that does not install the new release on trial as the design
/// says: `current` and `previous` both moved (a rollback target exists), the staged
/// slot cleared, the order, the start budget and an
/// unstarted window recorded. It also catches a swap that touches a release's files:
/// the binary of the release that was running keeps its bytes and its inode, so a
/// process running it is undisturbed.
///
/// Catches: `previous` not moved; `staged` left.
#[test]
fn an_order_swaps_a_staged_release_in_on_trial_and_changes_no_release_file() {
    let (mut exec, layout) = host("swap");
    running(&exec, &layout, A, &[B]);
    let old = exec.at(&format!("/var/lib/kbf-installer/releases/{A}/kbf"));
    let inode = fs::metadata(&old).expect("stat").ino();
    let mut was = state_of(&layout);
    was.staged = Some(c(B));
    Locked::acquire(&layout)
        .expect("lock")
        .write(&was)
        .expect("w");

    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 5), START),
        Ok(Activated::Swapped)
    );

    assert_eq!(link(&exec, "current"), at(B));
    assert_eq!(link(&exec, "previous"), at(A));
    assert_eq!(
        state_of(&layout),
        State {
            current: Some(c(B)),
            previous: Some(c(A)),
            staged: None,
            pending_confirm: Some(PendingConfirm {
                commit: c(B),
                previous: Some(c(A)),
                activation: "o1".to_owned(),
                deadline_wall: None,
                deadline_mono_s: None,
                starts_left: Some(3),
            }),
            ..State::default()
        }
    );
    assert_eq!(fs::read_to_string(&old).expect("read"), A);
    assert_eq!(fs::metadata(&old).expect("stat").ino(), inode);
    let new = exec.at(&format!("/var/lib/kbf-installer/releases/{B}/kbf"));
    assert_eq!(fs::read_to_string(new).expect("read"), B);
}

/// Catches an order for a release nobody installed being linked to (a `current` that
/// points nowhere and a service that cannot start), and the state written before the
/// refusal.
///
/// Catches: the installed check dropped.
#[test]
fn a_release_that_is_not_installed_is_refused_and_nothing_changes() {
    let (mut exec, layout) = host("not-installed");
    running(&exec, &layout, A, &[]);
    let was = state_of(&layout);
    let err = go(&mut exec, &layout, &order(B, "o1", 1), START).expect_err("not installed");
    assert!(
        err.contains(&format!("release {B} is not installed")),
        "{err}"
    );
    assert_eq!(state_of(&layout), was);
    assert_eq!(link(&exec, "current"), at(A));
    assert_eq!(link_steps(&exec), 0);
}

/// Catches a floor that refuses a rollback: a release is ordered down by ordering the old
/// release again, so an order whose number is below anything recorded is carried out, and
/// the recorded floor (an old build's field) is left as it was.
///
/// Catches: the old floor check put back (the order below 7 is refused).
#[test]
fn an_order_is_never_refused_for_its_number_and_the_floor_stays() {
    let (mut exec, layout) = host("floor");
    running(&exec, &layout, A, &[B]);
    let mut was = state_of(&layout);
    was.floor.order_id = 7;
    Locked::acquire(&layout)
        .expect("lock")
        .write(&was)
        .expect("w");
    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 6), START),
        Ok(Activated::Swapped)
    );
    assert_eq!(state_of(&layout).floor.order_id, 7);
}

/// Catches a repeated order that swaps twice (the host restarts twice and `previous`
/// becomes the release on trial, so a revert goes nowhere), and a second order that
/// takes a swap on trial over.
///
/// Catches: the order id not compared (the repeat is refused as another
/// order); the pending check dropped (the other order swaps).
#[test]
fn a_repeated_order_changes_nothing_and_another_order_waits_for_the_pending_one() {
    let (mut exec, layout) = host("repeat");
    running(&exec, &layout, A, &[B, C]);
    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 5), START),
        Ok(Activated::Swapped)
    );
    let after = state_of(&layout);
    let steps = link_steps(&exec);

    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 5), START),
        Ok(Activated::Repeat)
    );
    assert_eq!(state_of(&layout), after);
    assert_eq!(link_steps(&exec), steps);

    let err = go(&mut exec, &layout, &order(C, "o2", 6), START).expect_err("pending");
    assert!(
        err.contains(&format!("a commit-confirm deploy of {B} is pending")),
        "{err}"
    );
    assert_eq!(state_of(&layout), after);
    assert_eq!(link(&exec, "current"), at(B));
}

/// Catches a repeat of a confirmed order, after the host was restarted by the helper's
/// own crash, that swaps the running release in over itself: `previous` would become
/// `current` and a revert would have nothing to go back to.
///
/// Catches: the already-current check dropped.
#[test]
fn an_order_for_the_release_that_is_current_is_a_no_op() {
    let (mut exec, layout) = host("current");
    running(&exec, &layout, A, &[B]);
    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 5), START),
        Ok(Activated::Swapped)
    );
    assert_eq!(
        confirm(&layout, "o1", &c(B)).map_err(|e| e.to_string()),
        Ok(Confirmed::Kept)
    );
    let after = state_of(&layout);

    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 5), START),
        Ok(Activated::AlreadyCurrent)
    );
    assert_eq!(
        go(&mut exec, &layout, &order(B, "o9", 9), START),
        Ok(Activated::AlreadyCurrent)
    );
    assert_eq!(state_of(&layout), after);
    assert_eq!(state_of(&layout).previous, Some(c(A)));
}

/// Catches a confirm of the wrong order keeping a swap it did not make, a confirm that
/// leaves the swap pending, a confirm after a revert taken for a repeat, and a repeat
/// that is an error.
///
/// Catches: the order id not compared; the pending record not cleared; the
/// commit not compared.
#[test]
fn a_confirm_keeps_the_swap_only_for_the_order_that_made_it() {
    let (mut exec, layout) = host("confirm");
    running(&exec, &layout, A, &[B]);
    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 5), START),
        Ok(Activated::Swapped)
    );
    let pending = state_of(&layout);

    let err = confirm(&layout, "o2", &c(B))
        .expect_err("another order")
        .to_string();
    assert!(err.contains("was made by o1, not by order \"o2\""), "{err}");
    let err = confirm(&layout, "o1", &c(C))
        .expect_err("another release")
        .to_string();
    assert!(err.contains("was made by o1"), "{err}");
    assert_eq!(state_of(&layout), pending);

    assert_eq!(
        confirm(&layout, "o1", &c(B)).expect("kept"),
        Confirmed::Kept
    );
    assert_eq!(state_of(&layout).pending_confirm, None);
    assert_eq!(state_of(&layout).current, Some(c(B)));
    assert_eq!(
        confirm(&layout, "o1", &c(B)).expect("repeat"),
        Confirmed::AlreadyKept
    );

    // A swap that was reverted is not a swap that was kept.
    seed(&exec, C, 1_002);
    assert_eq!(
        go(&mut exec, &layout, &order(C, "o3", 6), START),
        Ok(Activated::Swapped)
    );
    revert(&mut exec, &layout, &mut Vec::new()).expect("revert");
    let err = confirm(&layout, "o3", &c(C))
        .expect_err("reverted")
        .to_string();
    assert!(
        err.contains(&format!(
            "no swap is pending, and {C} is not the current release"
        )),
        "{err}"
    );
}

/// Catches a revert that leaves `current` on the release on trial, keeps a `previous`
/// that is now the current release (the state keeps no release before it), leaves the
/// swap pending, or deletes the release it came from.
///
/// Catches: `current` not moved back; `previous` kept; `pending_confirm` kept.
#[test]
fn a_revert_puts_the_previous_release_back_and_keeps_the_release_it_left() {
    let (mut exec, layout) = host("revert");
    running(&exec, &layout, A, &[B]);
    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 5), START),
        Ok(Activated::Swapped)
    );

    let done = revert(&mut exec, &layout, &mut Vec::new()).expect("revert");
    assert_eq!(
        done,
        Reverted {
            from: Some(c(B)),
            to: c(A)
        }
    );
    assert_eq!(link(&exec, "current"), at(A));
    assert_eq!(link(&exec, "previous"), None);
    let state = state_of(&layout);
    assert_eq!(
        (state.current, state.previous, state.pending_confirm),
        (Some(c(A)), None, None)
    );
    assert_eq!(state.floor, crate::state::Floor::default());
    assert!(
        exec.at(&format!("/var/lib/kbf-installer/releases/{B}/kbf"))
            .exists()
    );
}

/// Catches a revert with no pending swap that does nothing, or one that goes where
/// there is nothing to go to.
#[test]
fn a_revert_with_nothing_pending_goes_to_the_previous_release_and_with_none_is_refused() {
    let (mut exec, layout) = host("revert-plain");
    running(&exec, &layout, A, &[B]);
    let err = revert(&mut exec, &layout, &mut Vec::new())
        .expect_err("none")
        .to_string();
    assert_eq!(err, "no previous release to roll back to");

    // A confirmed swap: `previous` is the release before it.
    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 5), START),
        Ok(Activated::Swapped)
    );
    confirm(&layout, "o1", &c(B)).expect("kept");
    let done = revert(&mut exec, &layout, &mut Vec::new()).expect("revert");
    assert_eq!(done.to, c(A));
    assert_eq!(link(&exec, "current"), at(A));
}

/// Catches a revert that gives up the release it is on before it knows it can go back:
/// with the previous release's files gone it must change nothing and leave the swap
/// pending, so the host reports it and an operator decides.
///
/// Catches: the state written before the check; the previous link removed
/// before the check.
#[test]
fn a_revert_whose_previous_release_is_gone_changes_nothing() {
    let (mut exec, layout) = host("revert-gone");
    running(&exec, &layout, A, &[B]);
    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 5), START),
        Ok(Activated::Swapped)
    );
    fs::remove_file(exec.at(&format!("/var/lib/kbf-installer/releases/{A}/kbf"))).expect("gone");
    let was = state_of(&layout);
    let steps = link_steps(&exec);

    let err = revert(&mut exec, &layout, &mut Vec::new())
        .expect_err("gone")
        .to_string();
    assert_eq!(
        err,
        format!("the previous release {A} is gone (no binary at its directory)")
    );
    assert_eq!(state_of(&layout), was);
    assert_eq!(link(&exec, "current"), at(B));
    assert_eq!(link(&exec, "previous"), at(A));
    assert_eq!(link_steps(&exec), steps);
}

/// Catches a window that starts at the swap on Linux (a frontend that waits hours in
/// drain between the swap and its restart would be reverted while healthy), a window
/// that is re-armed by every start (a crash loop would never run out), a start budget
/// that is off by one, and a release that starts after its budget is spent.
///
/// Catches: the window armed at the swap; `deadline.or(..)` replaced by a new
/// deadline each start; the budget checked after the start; no budget.
#[test]
fn the_window_starts_at_the_first_start_and_a_crash_loop_runs_out_of_starts() {
    let (mut exec, layout) = host("boot");
    running(&exec, &layout, A, &[B]);
    let arm = Arm::FromFirstStart { starts: 2 };
    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 5), arm),
        Ok(Activated::Swapped)
    );

    // Hours pass between the swap and the first start: nothing is due.
    let mut out = Vec::new();
    assert_eq!(
        expire(&mut exec, &layout, 1_000 + 7 * 3_600, &mut out).expect("e"),
        Expiry::NotStarted
    );

    let mut check = |now| boot_check(&mut exec, &layout, now, 600, &mut Vec::new()).expect("boot");
    assert_eq!(
        check(50_000),
        Boot::Start {
            starts_left: Some(1)
        }
    );
    assert_eq!(
        check(50_100),
        Boot::Start {
            starts_left: Some(0)
        }
    );
    // The deadline is the first start's plus the window, not moved by the second.
    let p = state_of(&layout).pending_confirm.expect("pending");
    assert_eq!(p.deadline_wall, Some(50_600));
    // Two starts without a confirm: the third reverts, before the service starts.
    assert_eq!(
        check(50_200),
        Boot::Reverted(Reverted {
            from: Some(c(B)),
            to: c(A)
        })
    );
    assert_eq!(link(&exec, "current"), at(A));
    assert_eq!(state_of(&layout).pending_confirm, None);
    // Nothing is on trial now.
    assert_eq!(
        boot_check(&mut exec, &layout, 50_300, 600, &mut Vec::new()).expect("b"),
        Boot::Nothing
    );
}

/// Catches a confirm window that runs out and is not enforced at the next start, and a
/// start that the window had not run out for being reverted.
///
/// Catches: the deadline not compared at a start.
#[test]
fn a_start_after_the_deadline_reverts_and_one_before_it_does_not() {
    let (mut exec, layout) = host("boot-late");
    running(&exec, &layout, A, &[B]);
    assert_eq!(
        go(
            &mut exec,
            &layout,
            &order(B, "o1", 5),
            Arm::FromFirstStart { starts: 9 }
        ),
        Ok(Activated::Swapped)
    );
    assert_eq!(
        boot_check(&mut exec, &layout, 2_000, 300, &mut Vec::new()).expect("b"),
        Boot::Start {
            starts_left: Some(8)
        }
    );
    assert_eq!(
        boot_check(&mut exec, &layout, 2_299, 300, &mut Vec::new()).expect("b"),
        Boot::Start {
            starts_left: Some(7)
        }
    );
    let done = boot_check(&mut exec, &layout, 2_300, 300, &mut Vec::new()).expect("b");
    assert_eq!(
        done,
        Boot::Reverted(Reverted {
            from: Some(c(B)),
            to: c(A)
        })
    );
}

/// Catches a swap that is never reverted when nobody confirms it (macOS: the timer is
/// the only trigger), one reverted a second early, one confirmed in time that is
/// reverted anyway, and a revert that happens at the exact second as a wait.
///
/// Catches: `now < deadline` written `now <= deadline`; the confirm not
/// clearing the record.
#[test]
fn a_swap_not_confirmed_within_its_window_is_reverted() {
    let (mut exec, layout) = host("expire");
    running(&exec, &layout, A, &[B, C]);
    let arm = Arm::FromActivation { window_secs: 300 };
    assert_eq!(
        go(&mut exec, &layout, &order(B, "o1", 5), arm),
        Ok(Activated::Swapped)
    );
    assert_eq!(
        state_of(&layout).pending_confirm.expect("p").deadline_wall,
        Some(1_300)
    );

    let mut out = Vec::new();
    assert_eq!(
        expire(&mut exec, &layout, 1_299, &mut out).expect("e"),
        Expiry::Waiting { secs_left: 1 }
    );
    assert_eq!(link(&exec, "current"), at(B));
    let done = expire(&mut exec, &layout, 1_300, &mut out).expect("e");
    assert_eq!(
        done,
        Expiry::Reverted(Reverted {
            from: Some(c(B)),
            to: c(A)
        })
    );
    assert_eq!(link(&exec, "current"), at(A));

    // Confirmed in time: a later expiry finds nothing.
    seed(&exec, C, 1_002);
    assert_eq!(
        go(&mut exec, &layout, &order(C, "o2", 6), arm),
        Ok(Activated::Swapped)
    );
    confirm(&layout, "o2", &c(C)).expect("kept");
    assert_eq!(
        expire(&mut exec, &layout, 99_999, &mut out).expect("e"),
        Expiry::Nothing
    );
    assert_eq!(link(&exec, "current"), at(C));
}

/// Catches a window the order (or a hostile frontend) can set to a year or to nothing:
/// the host's bounds are enforced at both places a window is taken, inclusive.
///
/// Catches: either bound moved by one; the check missing from `boot_check`.
#[test]
fn a_window_outside_the_hosts_bounds_is_refused() {
    let (mut exec, layout) = host("bounds");
    running(&exec, &layout, A, &[B]);
    for secs in [WINDOW_MIN_SECS - 1, WINDOW_MAX_SECS + 1, 0, 31_536_000] {
        let err = go(
            &mut exec,
            &layout,
            &order(B, "o1", 5),
            Arm::FromActivation { window_secs: secs },
        )
        .expect_err("window");
        assert!(
            err.contains(&format!("window of {secs} s is outside")),
            "{err}"
        );
        let err = boot_check(&mut exec, &layout, 1, secs, &mut Vec::new()).expect_err("window");
        assert!(
            err.to_string().contains(&format!("window of {secs} s")),
            "{err}"
        );
    }
    assert_eq!(link_steps(&exec), 0);
    for secs in [WINDOW_MIN_SECS, WINDOW_MAX_SECS] {
        let (mut exec, layout) = host(&format!("bounds-{secs}"));
        running(&exec, &layout, A, &[B]);
        assert_eq!(
            go(
                &mut exec,
                &layout,
                &order(B, "o1", 5),
                Arm::FromActivation { window_secs: secs }
            ),
            Ok(Activated::Swapped)
        );
    }
}

/// Catches an activation or a revert that moves the links before the state says where
/// they are going, or `current` before `previous`. The host is killed after 0, 1 and 2
/// applied steps of each verb: whatever was done, the state already names the target,
/// and `reconcile` carries the links to it, so the host ends on one release, never a
/// mix, with the release a revert goes to intact. After the first step of a swap the
/// host still runs the old release and `previous` already names it.
///
/// Catches: the links moved before the state is written (the state is the old
/// one after a kill at 0 steps); `current` moved before `previous` (a kill after one
/// step leaves `current` on the new release with no `previous`).
#[test]
fn a_host_killed_between_any_two_steps_of_a_swap_or_a_revert_converges() {
    for kill_after in 0..=2 {
        let (mut exec, layout) = host(&format!("crash-swap-{kill_after}"));
        running(&exec, &layout, A, &[B]);
        let o = order(B, "o1", 5);
        let killed = activate(
            &mut Killed {
                host: &mut exec,
                left: kill_after,
            },
            &layout,
            &o,
            START,
            1_000,
            &mut Vec::new(),
        );
        if kill_after < 2 {
            killed.expect_err("killed");
        } else {
            // Two steps are all a swap has; the kill came after both.
            assert!(killed.is_ok());
        }
        // The state was written before any link moved.
        let state = state_of(&layout);
        assert_eq!(state.current, Some(c(B)), "kill after {kill_after}");
        assert_eq!(
            state.pending_confirm.as_ref().map(|p| p.previous.clone()),
            Some(Some(c(A)))
        );
        if kill_after == 1 {
            assert_eq!(link(&exec, "current"), at(A));
            assert_eq!(link(&exec, "previous"), at(A));
        }
        // The release a revert goes to is whole whatever moved.
        assert!(
            exec.at(&format!("/var/lib/kbf-installer/releases/{A}/kbf"))
                .exists()
        );

        reconcile(&mut exec, &layout, &mut Vec::new()).expect("reconcile");
        assert_eq!(link(&exec, "current"), at(B), "kill after {kill_after}");
        assert_eq!(link(&exec, "previous"), at(A), "kill after {kill_after}");

        // The same for the revert.
        let killed = revert(
            &mut Killed {
                host: &mut exec,
                left: kill_after,
            },
            &layout,
            &mut Vec::new(),
        );
        if kill_after < 2 {
            killed.expect_err("killed");
        } else {
            assert!(killed.is_ok());
        }
        let state = state_of(&layout);
        assert_eq!(
            (state.current.clone(), state.pending_confirm.clone()),
            (Some(c(A)), None),
            "kill after {kill_after}"
        );
        reconcile(&mut exec, &layout, &mut Vec::new()).expect("reconcile");
        assert_eq!(link(&exec, "current"), at(A), "kill after {kill_after}");
        assert_eq!(link(&exec, "previous"), None, "kill after {kill_after}");
    }
}

/// Catches a reconcile that moves a healthy host's links or reports a move that did not
/// happen, one that does nothing for links that disagree with the state, and one that
/// invents a `current` for an empty host.
///
/// Catches: the moved flag always true; the converge skipped.
#[test]
fn reconcile_moves_links_to_the_state_and_leaves_a_consistent_host_alone() {
    let (mut exec, layout) = host("reconcile");
    assert!(!reconcile(&mut exec, &layout, &mut Vec::new()).expect("empty"));
    assert_eq!(link(&exec, "current"), None);

    running(&exec, &layout, A, &[B]);
    assert!(!reconcile(&mut exec, &layout, &mut Vec::new()).expect("healthy"));
    assert_eq!(link_steps(&exec), 0);

    let mut state = state_of(&layout);
    state.current = Some(c(B));
    state.previous = Some(c(A));
    Locked::acquire(&layout)
        .expect("lock")
        .write(&state)
        .expect("w");
    assert!(reconcile(&mut exec, &layout, &mut Vec::new()).expect("repair"));
    assert_eq!(
        (link(&exec, "current"), link(&exec, "previous")),
        (at(B), at(A))
    );
    assert!(!reconcile(&mut exec, &layout, &mut Vec::new()).expect("again"));
}

/// Catches a retention that loses the rollback target: after two confirmed swaps the
/// running release, the previous one and the four newest of the rest (the default) are
/// kept, the others removed, and a revert still works.
///
/// Catches: `gc` not keeping the state's previous; the newest-first order
/// reversed; a default of fewer than four.
#[test]
fn retention_keeps_the_running_the_previous_and_the_four_newest_others() {
    assert_eq!(crate::retain::DEFAULT_KEEP, 4);
    let (mut exec, layout) = host("retention");
    running(&exec, &layout, A, &[B, C, D, E]);
    // Three older ones: with A, D and E that makes six others, so two go.
    let (x, y, z) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
    for (release, age) in [(&x, 10), (&y, 20), (&z, 30)] {
        seed(&exec, release, age);
    }
    for (n, (release, id)) in [(B, "o1"), (C, "o2")].into_iter().enumerate() {
        assert_eq!(
            go(&mut exec, &layout, &order(release, id, 5 + n as u64), START),
            Ok(Activated::Swapped)
        );
        confirm(&layout, id, &c(release)).expect("kept");
    }
    assert_eq!(
        (link(&exec, "current"), link(&exec, "previous")),
        (at(C), at(B))
    );

    release::gc(
        &mut exec,
        &layout,
        crate::retain::DEFAULT_KEEP,
        &Default::default(),
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect("gc");
    let mut left: Vec<String> = fs::read_dir(exec.at("/var/lib/kbf-installer/releases"))
        .expect("list")
        .map(|i| i.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name != "current" && name != "previous")
        .collect();
    left.sort();
    let mut want = [A, B, C, D, E, z.as_str()];
    want.sort_unstable();
    assert_eq!(
        left, want,
        "the running, the previous and the four newest others"
    );

    let done = revert(&mut exec, &layout, &mut Vec::new()).expect("revert");
    assert_eq!(done.to, c(B));
}

/// Catches a first release that is put on trial, or whose links and state disagree: on a host
/// with no release `first` writes the state with `current` and no trial (nothing to go back
/// to, so no pending confirm), points `current` at it and leaves `previous` unset, and a
/// repeat changes nothing.
///
/// Catches: `pending_confirm` set in `first`; the state written after the links;
/// the repeat arm removed (the repeat then fails with `NotFirst`).
#[test]
fn the_first_release_becomes_current_with_no_trial() {
    let (mut exec, layout) = host("first");
    seed(&exec, A, 1_000);
    first(&mut exec, &layout, &c(A), &mut Vec::new()).expect("first");
    let state = Locked::acquire(&layout)
        .expect("lock")
        .read()
        .expect("read")
        .expect("a state");
    assert_eq!(state.current, Some(c(A)));
    assert_eq!((state.previous, state.pending_confirm), (None, None));
    assert_eq!(link(&exec, "current"), at(A));
    assert_eq!(link(&exec, "previous"), None);
    first(&mut exec, &layout, &c(A), &mut Vec::new()).expect("a repeat");
    assert_eq!(link(&exec, "current"), at(A));
}

/// Catches a first release taken for a host that already runs one, and one that is not on
/// the host: another release current is `NotFirst` (setup never replaces a release: that is
/// an update), and a release with no binary is `NotInstalled`; neither writes a state.
///
/// Catches: the `NotFirst` guard removed; the `has_binary` check removed.
#[test]
fn a_first_release_never_replaces_one_and_must_be_installed() {
    let (mut exec, layout) = host("first-refused");
    running(&exec, &layout, A, &[B]);
    let err = first(&mut exec, &layout, &c(B), &mut Vec::new()).expect_err("refused");
    assert!(
        matches!(&err, ReleaseError::NotFirst(x) if *x == c(B)),
        "{err:?}"
    );
    assert_eq!(
        state_of(&layout).current,
        Some(c(A)),
        "the state is as it was"
    );
    let (mut bare, layout) = host("first-missing");
    let err = first(&mut bare, &layout, &c(D), &mut Vec::new()).expect_err("refused");
    assert!(
        matches!(&err, ReleaseError::NotInstalled(x) if *x == c(D)),
        "{err:?}"
    );
    assert!(state::read_unlocked(&layout).expect("read").is_none());
}

/// Catches a frontend stage of a release that is not on the host: it records a `staged` that
/// names nothing a confirm could swap in, and holds every other update off as busy. A release
/// with no directory and one whose directory holds no binary are both `NotInstalled`, and
/// neither writes the state; the same release installed is staged, and staging it again
/// changes nothing.
///
/// Catches: the `has_binary` check removed from `stage_frontend`; the state written
/// before the check.
#[test]
fn a_frontend_stage_needs_the_release_on_the_host_and_writes_nothing_otherwise() {
    let (exec, layout) = host("stage-frontend");
    running(&exec, &layout, A, &[B]);
    fs::create_dir_all(exec.at(&format!("/var/lib/kbf-installer/releases/{E}"))).expect("empty");
    let was = state_of(&layout);

    for missing in [D, E] {
        let err = stage_frontend(&exec, &layout, &c(missing)).expect_err("not on the host");
        assert!(
            matches!(&err, ReleaseError::NotInstalled(x) if *x == c(missing)),
            "{err:?}"
        );
        assert_eq!(state_of(&layout), was, "{missing}: the state is as it was");
    }

    stage_frontend(&exec, &layout, &c(B)).expect("staged");
    let staged = state_of(&layout);
    assert_eq!(staged.staged, Some(c(B)));
    assert_eq!(
        (&staged.current, &staged.previous),
        (&was.current, &was.previous)
    );
    stage_frontend(&exec, &layout, &c(B)).expect("a repeat");
    assert_eq!(state_of(&layout), staged);
}

/// Catches a first release that stays staged once it is current, as an `activate` of a staged
/// release does not: a `staged` that names the first release (the operator staged it before
/// setup made it current) is cleared, and a `staged` that names another release stays (it
/// is another decision's).
///
/// Catches: `staged` carried over unchanged; `staged` cleared whatever it names.
#[test]
fn a_first_release_that_was_staged_is_current_and_no_longer_staged() {
    for (staged, want) in [(A, None), (B, Some(c(B)))] {
        let (mut exec, layout) = host(&format!("first-staged-{staged}"));
        seed(&exec, A, 1_000);
        Locked::acquire(&layout)
            .expect("lock")
            .write(&State {
                staged: Some(c(staged)),
                ..State::default()
            })
            .expect("state");
        first(&mut exec, &layout, &c(A), &mut Vec::new()).expect("first");
        let state = state_of(&layout);
        assert_eq!(state.current, Some(c(A)), "{staged}");
        assert_eq!(state.staged, want, "{staged}");
    }
}
