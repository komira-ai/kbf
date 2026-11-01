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

/// Catches: a lost race logged at ERROR (it reads as data loss), logged twice, left out of
/// the race count, or counted as a dropped write. Two frontends over one store race for one
/// queued operation; the one that loses logs one WARN line that names the operation and
/// says the other frontend won, no ERROR line, one lost race and no dropped write; the
/// winner counts neither.
#[test]
#[cfg_attr(
    feature = "farm-core-js",
    ignore = "known-different-on-js: the js core logs a lost race at ERROR and counts it among its dropped writes"
)]
fn a_lost_race_logs_one_warning_and_counts_a_race_not_a_drop() {
    let log = logs(async {
        let disk = Disk::new();
        let (node_a, node_b) = (WorkerId::new("node-a"), WorkerId::new("node-b"));
        let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
        submit(&a, request(1)).expect("submitted");
        b.intake();
        let (_, mut inbox_b) = register_as(&b, &node_b);
        assert_eq!(starts(&mut inbox_b).len(), 1, "the winner did not start it");

        let (_, mut inbox_a) = register_as(&a, &node_a);

        assert!(starts(&mut inbox_a).is_empty(), "the loser started it");
        assert_eq!((a.lost_races(), a.dropped_writes()), (1, 0));
        assert_eq!((b.lost_races(), b.dropped_writes()), (0, 0));
    });
    let warnings: Vec<_> = log
        .lines()
        .filter(|line| line.contains("another frontend won the race"))
        .collect();
    let [warning] = &warnings[..] else {
        panic!("one race line expected: {log}");
    };
    assert!(warning.contains("WARN"), "{warning}");
    assert!(warning.contains("operations=op-0"), "{warning}");
    assert!(!log.contains("ERROR"), "{log}");
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
        assert_eq!((a.dropped_writes(), a.lost_races()), (1, 0));
    });
    let errors = log.lines().filter(|line| line.contains("ERROR")).count();
    assert_eq!(errors, 2, "{log}");
    assert!(log.contains("the state store did not take"), "{log}");
    assert!(log.contains("remaining effects were dropped"), "{log}");
    assert!(!log.contains("another frontend won the race"), "{log}");
}
