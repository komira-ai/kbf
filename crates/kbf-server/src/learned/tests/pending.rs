//! The runs a key's stored summaries are owed, from the attempt that taught the estimator
//! to the write that applies them to the store's rows.

use super::*;

/// Behaviour: every attempt shown to the estimator is owed to the store once, in order,
/// with the day and bounds it was judged under; handing the batch over empties it.
/// Catches: an attempt dropped before it is written, or handed over twice.
#[test]
fn every_attempt_is_owed_to_the_store_once_in_order() {
    let mut learned = shadow();
    learned.observe(&success(MIB), &bounds());
    learned.observe(&success(2 * MIB), &bounds());
    let owed = learned.due_now();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].0, key());
    let peaks: Vec<_> = owed[0].1.iter().map(|s| s.run.peak_memory_bytes).collect();
    assert_eq!(peaks, [Some(MIB), Some(2 * MIB)]);
    assert!(owed[0].1.iter().all(|s| s.bounds == bounds()));
    assert!(owed[0].1.iter().all(|s| s.day == learned.day()));
    assert_eq!(learned.due_now(), Vec::new());
}

/// Behaviour: runs a store refused come back before the runs seen since, so a retry
/// applies them in the order they ran. Catches: a refused batch put behind newer runs.
#[test]
fn refused_runs_are_offered_again_before_newer_ones() {
    let mut learned = shadow();
    learned.observe(&success(MIB), &bounds());
    let refused = learned.due_now();
    learned.observe(&success(2 * MIB), &bounds());
    learned.unwritten(&refused);
    let owed = learned.due_now();
    let peaks: Vec<_> = owed[0].1.iter().map(|s| s.run.peak_memory_bytes).collect();
    assert_eq!(peaks, [Some(MIB), Some(2 * MIB)]);
}

/// Behaviour: a store that takes nothing does not make the farm hold runs without bound;
/// the oldest of a key go first. Catches: memory growing with every run of an outage.
#[test]
fn a_key_holds_a_bounded_number_of_runs_for_a_store_that_takes_none() {
    let mut learned = shadow();
    for i in 0..PENDING_CAP as u64 + 5 {
        learned.observe(&success((i + 1) * MIB), &bounds());
    }
    let owed = learned.due_now();
    assert_eq!(owed[0].1.len(), PENDING_CAP);
    assert_eq!(owed[0].1[0].run.peak_memory_bytes, Some(6 * MIB));
    learned.unwritten(&owed);
    learned.unwritten(&owed);
    assert_eq!(learned.due_now()[0].1.len(), PENDING_CAP);
}
