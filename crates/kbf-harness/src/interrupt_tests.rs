//! The flag a run is asked to stop with. The handlers that set the process's flag are
//! installed (and a signal sent) in `procs_tests`, in a process of its own: a test here that
//! set the process flag would stop every other test of this binary that looks at it.

use super::*;

#[test]
fn a_flag_of_its_own_is_set_by_its_own_set_alone() {
    // Catches: flags that share state (a test that asks one run to stop stopping another's),
    // and a `set` that does not show.
    let (a, b) = (Interrupt::new(), Interrupt::default());
    assert!(!a.is_set() && !b.is_set());
    a.set();
    assert!(a.is_set());
    assert!(!b.is_set());
    // A clone is the same flag, as a run hands it to its parts.
    let c = a.clone();
    assert!(c.is_set());
    let d = b.clone();
    d.set();
    assert!(b.is_set());
}

#[test]
fn the_process_flag_is_not_a_flag_of_its_own() {
    // Catches: `process()` giving a private flag, which the signal handlers would never set.
    assert!(!Interrupt::new().is_set());
    assert!(Interrupt::process().own.is_none());
    assert!(Interrupt::new().own.is_some());
}
