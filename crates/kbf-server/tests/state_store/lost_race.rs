//! What a frontend logs and counts when the state store refuses one of its writes. Losing a
//! race for an operation to another frontend is how two frontends resolve a race by design:
//! the store keeps the winner's write, so it is a warning and a `lost_races` count. A refusal
//! that is not the race (here, a write for a node another frontend owns now) stays an error
//! and counts as a dropped write.

use super::*;

/// The log written while `scenario` runs on a runtime of its own.
fn logs(scenario: impl std::future::Future<Output = ()>) -> String {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a runtime");
    thread_log::logged(|| runtime.block_on(scenario))
}

/// The line a lost race is logged with: its message, then the `operations` field.
const LOST_RACE: &str = "another frontend won the race for the operation: this frontend's \
                         write is dropped and its placement undone";

/// Catches: a lost race logged at ERROR (it reads as data loss), logged twice, logged with
/// another WARN beside it, with its message cut short or without the exact `operations`
/// field, left out of the race count, or counted as a dropped write. Two frontends over one
/// store race for one queued operation; the one that loses logs exactly one WARN line, the
/// whole race message ending in `operations=op-0`, no ERROR line, one lost race and no
/// dropped write; the winner counts neither.
#[test]
fn a_lost_race_logs_one_warning_and_counts_a_race_not_a_drop() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a runtime");
    let disk = Disk::new();
    let (node_a, node_b) = (WorkerId::new("node-a"), WorkerId::new("node-b"));
    let (a, b) = runtime.block_on(async {
        let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
        submit(&a, request(1)).expect("submitted");
        b.intake();
        (a, b)
    });
    let (_stream_b, mut inbox_b) = runtime.block_on(async { register_as(&b, &node_b) });
    assert_eq!(starts(&mut inbox_b).len(), 1, "the winner did not start it");

    // Only the race is logged here: the loser's node joins, and its grant is refused.
    let log = thread_log::logged(|| {
        runtime.block_on(async {
            let (_, mut inbox_a) = register_as(&a, &node_a);
            assert!(starts(&mut inbox_a).is_empty(), "the loser started it");
        });
    });

    assert_eq!((a.lost_races(), a.dropped_writes()), (Some(1), 0));
    assert_eq!((b.lost_races(), b.dropped_writes()), (Some(0), 0));
    let warnings: Vec<_> = log.lines().filter(|line| line.contains(" WARN ")).collect();
    let [warning] = &warnings[..] else {
        panic!("one WARN line expected: {log}");
    };
    let wanted = format!(": {LOST_RACE} operations=op-0");
    assert!(warning.ends_with(&wanted), "{warning}");
    assert!(!log.contains(" ERROR "), "{log}");
}

/// Catches: the race treatment applied to every refusal. A result reported to the frontend
/// a daemon has left is refused because the node's owner is another frontend: that stays
/// two ERROR lines (the store did not take it; the record's remaining effects were dropped),
/// counts a dropped write and no lost race.
#[test]
fn a_write_refused_for_a_moved_node_still_logs_errors_and_counts_a_drop() {
    let log = logs(async {
        let disk = Disk::new();
        let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
        let (_, mut inbox_a) = register(&a);
        submit(&a, request(1)).expect("submitted");
        let [lease] = starts(&mut inbox_a)[..] else {
            panic!("one lease expected");
        };
        let (_stream, _inbox) = register_as(&b, &node());

        let ack = a.report(&node(), invalid(lease)).await;

        assert!(ack.is_none_or(|ack| !ack.accepted));
        assert_eq!(a.dropped_writes(), 1);
        assert_eq!(
            a.lost_races(),
            Some(0),
            "a moved node counted as a lost race"
        );
    });
    let errors = log.lines().filter(|line| line.contains("ERROR")).count();
    assert_eq!(errors, 2, "{log}");
    assert!(log.contains("the state store did not take"), "{log}");
    assert!(log.contains("remaining effects were dropped"), "{log}");
    assert!(!log.contains("another frontend won the race"), "{log}");
}
