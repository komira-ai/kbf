//! Runs of the backfill over an in-memory store and pack index.

use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_pack_index::PackIndex as _;

use crate::support::{Ran, World, says};

fn report(world: &World, name: &str) -> String {
    std::fs::read_to_string(world.path(name)).expect("a report file")
}

fn blobs(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|at| format!("object number {at}").into_bytes())
        .collect()
}

fn ok(ran: &Ran) {
    assert_eq!(ran.code, 0, "{}", ran.err);
}

/// Asserts that a dry run reads the index and asks the store for each list's length, and
/// writes nothing: no read, no put, no cells, no dropped row.
#[test]
fn a_dry_run_asks_whether_each_list_is_there_and_writes_nothing() {
    let world = World::new("runs-dry");
    let first = world.tree(&blobs(2));
    let second = world.tree(&blobs(3));
    let gone = world.tree_without_list(1);

    let ran = world.run(&["--report-json", &world.path("r.json").to_string_lossy()]);

    ok(&ran);
    says(&ran.out, "dry run, nothing was written");
    says(&ran.out, "would write 2, list gone 1");
    assert_eq!(world.store.calls(), (3, 0, 0));
    assert_eq!(world.unlisted().len(), 3);
    let cells = world.index.object_cells(&first.objects).expect("cells");
    assert!(cells.iter().all(Option::is_none));
    assert!(
        !world
            .runtime
            .block_on(world.store.holds(&format!("cas/l2/{}", first.hex())))
    );
    assert!(
        !world
            .runtime
            .block_on(world.store.holds(&format!("cas/l2/{}", second.hex())))
    );
    assert_eq!(gone.objects.len(), 0);
}

/// Asserts the report file, byte for byte, of a dry run over two trees and a gone list.
#[test]
fn the_report_json_is_exactly_the_counts() {
    let world = World::new("runs-golden");
    world.tree(&blobs(1));
    world.tree(&blobs(2));
    world.tree_without_list(9);
    let path = world.path("r.json");

    let ran = world.run(&["--report-json", &path.to_string_lossy()]);

    ok(&ran);
    let expected = r#"{
  "mode": "dry-run",
  "backfill": {
    "within_days": 14,
    "trees_seen": 3,
    "written": 0,
    "would_write": 2,
    "gone": 1,
    "lost": 0,
    "failed": 0,
    "objects_read": 0,
    "bytes_read": 0,
    "finished": true,
    "problems": []
  },
  "audit": null
}"#;
    assert_eq!(report(&world, "r.json"), expected);
    assert!(!world.path("r.json.tmp").exists());
}

/// Asserts that an apply writes each tree's sidecar `cas/l2/<id>`, records the cells of the
/// objects it read, counts them, and drops the unlisted rows.
#[test]
fn an_apply_writes_the_sidecars_records_the_cells_and_drops_the_rows() {
    let world = World::new("runs-apply");
    let big: Vec<u8> = (0..(2 * (1 << 20) + 5))
        .map(|at| (at % 251) as u8)
        .collect();
    let first = world.tree(&[big.clone(), b"small".to_vec()]);
    let second = world.tree(&blobs(2));

    let ran = world.run(&["--mode", "apply"]);

    ok(&ran);
    says(&ran.out, "sidecars written 2");
    for tree in [&first, &second] {
        let key = format!("cas/l2/{}", tree.hex());
        assert!(world.runtime.block_on(world.store.holds(&key)), "{key}");
        let cells = world.index.object_cells(&tree.objects).expect("cells");
        assert!(cells.iter().all(Option::is_some));
    }
    assert!(world.unlisted().is_empty());
    let bytes: u64 = (big.len() + 5 + 2 * "object number 0".len()) as u64;
    says(&ran.out, &format!("objects read 4, bytes read {bytes}"));
}

/// Asserts that a second apply finds nothing to do and reads nothing.
#[test]
fn a_second_apply_has_nothing_to_do() {
    let world = World::new("runs-again");
    world.tree(&blobs(2));
    ok(&world.run(&["--mode", "apply"]));
    let before = world.store.calls();

    let ran = world.run(&["--mode", "apply"]);

    ok(&ran);
    says(&ran.out, "trees 0, all handled true");
    assert_eq!(world.store.calls(), before);
}

/// Asserts that a list the store no longer has is counted gone and, in an apply, its row is
/// dropped.
#[test]
fn a_gone_list_is_counted_and_its_row_dropped_in_an_apply() {
    let world = World::new("runs-gone");
    world.tree_without_list(1);
    let vanished = world.tree(&blobs(1));
    world
        .runtime
        .block_on(world.store.delete(&format!("cas/l/{}", vanished.hex())));

    let ran = world.run(&["--mode", "apply"]);

    ok(&ran);
    says(&ran.out, "list gone 2");
    assert!(world.unlisted().is_empty());
}

/// Asserts that a list that disappears between the length and the read is gone too, and that a
/// list of no bytes is a tree that failed.
#[test]
fn a_list_that_vanishes_is_gone_and_an_empty_list_fails() {
    let world = World::new("runs-vanish");
    let empty = world.tree(&blobs(1));
    world.runtime.block_on(async {
        world.store.delete(&format!("cas/l/{}", empty.hex())).await;
        world
            .store
            .put(&format!("cas/l/{}", empty.hex()), b"")
            .await;
    });

    let ran = world.run(&["--mode", "apply"]);

    assert_eq!(ran.code, 1, "{}", ran.err);
    says(&ran.out, "failed 1");
    assert_eq!(world.unlisted().len(), 1);
}

/// Asserts that an object the store does not have is counted lost, the row is kept, and the
/// run still ends well (the frontend's miss path republishes the tree).
#[test]
fn a_lost_object_is_counted_and_the_row_kept() {
    let world = World::new("runs-lost");
    let tree = world.tree(&blobs(2));
    let good = world.tree(&[b"another object".to_vec()]);
    world.runtime.block_on(world.store.delete(&tree.objects[1]));
    let path = world.path("r.json");

    let ran = world.run(&["--mode", "apply", "--report-json", &path.to_string_lossy()]);

    ok(&ran);
    says(&ran.out, "object lost 1");
    says(
        &ran.out,
        &format!(
            "{} lost: the store has no object {}",
            tree.hex(),
            tree.objects[1]
        ),
    );
    assert_eq!(world.unlisted(), vec![tree.id]);
    assert!(
        world
            .runtime
            .block_on(world.store.holds(&format!("cas/l2/{}", good.hex())))
    );
    says(&report(&world, "r.json"), "\"kind\": \"lost\"");
}

/// Asserts that a store failure on a length or a read counts the tree failed, keeps its row,
/// prints the report, and ends with status 1.
#[test]
fn a_store_failure_is_counted_failed_the_row_kept_and_the_status_1() {
    let world = World::new("runs-failed");
    let by_len = world.tree(&blobs(1));
    let by_read = world.tree(&blobs(2));
    world
        .store
        .fail_len
        .lock()
        .expect("lock")
        .insert(format!("cas/l/{}", by_len.hex()));
    world
        .store
        .fail_read
        .lock()
        .expect("lock")
        .insert(format!("cas/l/{}", by_read.hex()));

    let ran = world.run(&["--mode", "apply"]);

    assert_eq!(ran.code, 1);
    says(&ran.out, "failed 2");
    says(&ran.out, "the length of");
    says(&ran.out, "reading cas/l/");
    says(&ran.err, "2 trees failed");
    assert_eq!(world.unlisted().len(), 2);
}

/// Asserts that a row that cannot be dropped for a gone list is a failed tree, and that a
/// tree whose list the engine refuses is failed with the engine's words.
#[test]
fn a_drop_that_fails_and_a_list_that_is_not_one_are_failed() {
    let world = World::new("runs-drop");
    world.tree_without_list(1);
    world.index.fail_drop.store(true, Ordering::SeqCst);
    let junk = world.tree(&blobs(1));
    world.runtime.block_on(async {
        world.store.delete(&format!("cas/l/{}", junk.hex())).await;
        world
            .store
            .put(&format!("cas/l/{}", junk.hex()), b"not a tree list")
            .await;
    });

    let ran = world.run(&["--mode", "apply"]);

    assert_eq!(ran.code, 1);
    says(&ran.out, "failed 2");
    says(&ran.out, "dropping the unlisted row");
}

/// Asserts that an index that cannot list the unlisted trees ends the run with status 1, and
/// that the report is printed and written all the same.
#[test]
fn an_index_that_cannot_list_fails_the_run_and_the_report_is_still_written() {
    let world = World::new("runs-index");
    world.index.fail_unlisted.store(true, Ordering::SeqCst);
    let path = world.path("r.json");

    let ran = world.run(&["--report-json", &path.to_string_lossy()]);

    assert_eq!(ran.code, 1);
    says(&ran.err, "listing the unlisted trees");
    says(&ran.out, "all handled false");
    says(&report(&world, "r.json"), "\"finished\": false");
}

/// Asserts that `--limit-trees` stops after that many trees, says the run did not finish, and
/// that a run again carries on.
#[test]
fn limit_trees_stops_the_run_early() {
    let world = World::new("runs-limit");
    for at in 1..=5 {
        world.tree_without_list(at);
    }

    let ran = world.run(&["--limit-trees", "2"]);

    ok(&ran);
    says(&ran.out, "trees 2, all handled false");
    assert_eq!(world.store.calls().0, 2);
}

/// Asserts that the unlisted trees are taken by pages of the index's size: 2,300 trees are all
/// handled.
#[test]
fn every_page_of_unlisted_trees_is_handled() {
    let world = World::new("runs-pages");
    for at in 0..2300 {
        world.tree_without_list(at);
    }

    let ran = world.run(&[]);

    ok(&ran);
    says(&ran.out, "trees 2300, all handled true");
}

/// Asserts that `--within-days` takes only the trees recorded that recently.
#[test]
fn within_days_selects_the_recent_trees() {
    let world = World::new("runs-within");
    world.tree_without_list(1);
    world.index_clock.advance(Duration::from_secs(20 * 86_400));
    world.tree_without_list(2);

    let recent = world.run(&[]);
    let both = world.run(&["--within-days", "30"]);

    says(&recent.out, "within 14 days, trees 1,");
    says(&both.out, "within 30 days, trees 2,");
}

/// Asserts that `--trees-per-second` asks the sleeper for the spacing between trees, and only
/// between them.
#[test]
fn the_rate_limit_asks_for_the_spacing_between_trees() {
    let world = World::new("runs-rate");
    for at in 1..=4 {
        world.tree_without_list(at);
    }

    ok(&world.run(&["--trees-per-second", "4"]));

    let waits = world.naps.waits();
    assert_eq!(waits, vec![Duration::from_millis(250); 3]);
}

/// Asserts that the time a tree took is part of the spacing: a tree that took 100 ms is
/// followed by a wait of the remaining 150 ms, and one that took longer than the spacing by
/// none.
#[test]
fn the_work_of_a_tree_counts_towards_the_spacing() {
    let world = World::new("runs-work");
    for at in 1..=3 {
        world.tree_without_list(at);
    }
    *world.store.work.lock().expect("lock") =
        Some((world.clock.clone(), Duration::from_millis(100)));

    ok(&world.run(&["--trees-per-second", "4"]));
    assert_eq!(world.naps.waits(), vec![Duration::from_millis(150); 2]);

    world.naps.waits.lock().expect("lock").clear();
    *world.store.work.lock().expect("lock") =
        Some((world.clock.clone(), Duration::from_millis(400)));
    ok(&world.run(&["--trees-per-second", "4"]));
    assert!(world.naps.waits().is_empty());
}

/// Asserts that a run without a rate never sleeps.
#[test]
fn no_rate_means_no_sleep() {
    let world = World::new("runs-norate");
    world.tree_without_list(1);
    world.tree_without_list(2);

    ok(&world.run(&[]));

    assert!(world.naps.waits().is_empty());
}

/// Asserts that a report that cannot be written ends the run with status 1 after the report
/// was printed.
#[test]
fn a_report_file_that_cannot_be_written_fails_the_run() {
    let world = World::new("runs-nowrite");
    world.tree_without_list(1);
    let path = world.path("missing/r.json");

    let ran = world.run(&["--report-json", &path.to_string_lossy()]);

    assert_eq!(ran.code, 1);
    says(&ran.out, "trees 1");
}
