//! The deploy lock is free again the moment its [`Locked`] is dropped, whatever other
//! threads of the process are doing.

use std::time::Duration;

use kbf_installer::state::Locked;
use kbf_installer::testkit::host_root;
use kbf_testkit::forked::ForkedChild;

/// Catches: a lock released only by closing its file. An `flock` belongs to the open file
/// description, which lives until every copy of the descriptor is closed, and a process
/// forked while the lock is held (any other test of the binary starting a program, or a
/// spawn of the installer's own) keeps a copy until it execs. The lock then outlives the
/// drop, and taking it again is refused (`the_lock_is_held_while_a_locked_lives` failed
/// that way on kbf#1133's CI). Each child here is held before exec until the lock has
/// been taken again, so the interleaving is the failing one every time. Both ways of
/// taking the lock are dropped with a child alive that holds a copy: the one that waits
/// ([`Locked::acquire`]) and the one that gives up ([`Locked::acquire_within`], with no
/// time to wait, so a lock still held says so at once).
///
/// Mutant: the `Drop` of `Locked` removed (the close alone releases the lock): the
/// retake is refused as busy, with the child between fork and exec.
#[test]
fn a_child_forked_while_the_lock_was_held_does_not_keep_it_after_the_drop() {
    let (host, _dir) = host_root("state-lock-forked");
    let waited = Locked::acquire(&host).expect("the first lock");
    let first = ForkedChild::before_exec();
    drop(waited);
    let gave_up = Locked::acquire_within(&host, Duration::ZERO);
    let gave_up = gave_up.expect("a lock after the drop, with a forked child still alive");
    let second = ForkedChild::before_exec();
    drop(gave_up);
    let again = Locked::acquire_within(&host, Duration::ZERO);
    let again = again.expect("a lock after the drop of one taken without waiting");
    drop(again);
    second.finish();
    first.finish();
}
