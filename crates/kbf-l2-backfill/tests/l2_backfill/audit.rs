//! The live-key sample: its selection, its file and its place among the passes.

use std::num::NonZeroUsize;
use std::sync::atomic::Ordering;

use kbf_l2_backfill::audit::{Selector, file_text, rank};

use crate::support::{World, says};

fn keys(count: usize) -> Vec<String> {
    (0..count).map(|at| format!("cas/p/{at:05}")).collect()
}

fn n(count: usize) -> NonZeroUsize {
    NonZeroUsize::new(count).expect("above zero")
}

fn sample_of(world: &World, more: &[&str]) -> Vec<String> {
    let path = world.path("sample.txt");
    let mut flags = vec!["--audit-keys-out", path.to_str().expect("a path")];
    flags.extend_from_slice(more);
    let ran = world.run(&flags);
    assert_eq!(ran.code, 0, "{}", ran.err);
    let text = std::fs::read_to_string(&path).expect("a sample file");
    text.lines().map(str::to_owned).collect()
}

/// Asserts that the one-pass selection equals sorting every key by rank and taking the first
/// `n`, for several sizes and seeds over a few hundred keys.
#[test]
fn the_bounded_selection_equals_a_sort_of_every_key() {
    let all = keys(317);
    for seed in [0_u64, 1, 99, u64::MAX] {
        for take in [1_usize, 7, 50, 316, 317, 400] {
            let mut selector = Selector::new(seed, n(take));
            for key in &all {
                selector.offer(key);
            }
            let mut ranked: Vec<_> = all
                .iter()
                .map(|key| (rank(seed, key), key.clone()))
                .collect();
            ranked.sort();
            let mut expected: Vec<String> =
                ranked.into_iter().take(take).map(|(_, key)| key).collect();
            expected.sort();
            assert_eq!(selector.into_keys(), expected, "seed {seed}, n {take}");
        }
    }
}

/// Asserts that the selection does not depend on the order the keys are offered in.
#[test]
fn the_selection_does_not_depend_on_the_order_of_the_sweep() {
    let all = keys(100);
    let mut forward = Selector::new(5, n(10));
    let mut backward = Selector::new(5, n(10));
    all.iter().for_each(|key| forward.offer(key));
    all.iter().rev().for_each(|key| backward.offer(key));

    assert_eq!(forward.into_keys(), backward.into_keys());
}

/// Asserts the file's format: one key per line, each ended by a newline, sorted; no keys, an
/// empty file.
#[test]
fn the_file_is_one_key_per_line_each_ended_by_a_newline() {
    assert_eq!(file_text(&["a".to_owned(), "b/c".to_owned()]), "a\nb/c\n");
    assert_eq!(file_text(&[]), "");
}

/// Asserts that a sample alone is exactly `--audit-keys` keys when more are live, all of them
/// when fewer, sorted, one per line, and the same for a seed.
#[test]
fn the_sample_has_n_keys_or_all_of_them_and_is_the_same_for_a_seed() {
    let world = World::new("audit-n");
    for key in keys(300) {
        world.live_pack(&key);
    }

    let ten = sample_of(&world, &["--audit-keys", "10", "--audit-seed", "3"]);
    let again = sample_of(&world, &["--audit-keys", "10", "--audit-seed", "3"]);
    let all = sample_of(&world, &["--audit-keys", "1000", "--audit-seed", "3"]);

    assert_eq!(ten.len(), 10);
    assert_eq!(ten, again);
    let mut sorted = ten.clone();
    sorted.sort();
    assert_eq!(ten, sorted);
    assert_eq!(all, keys(300));
    let text = std::fs::read_to_string(world.path("sample.txt")).expect("a file");
    assert!(text.ends_with('\n'));
    assert_eq!(text.matches('\n').count(), 300);
}

/// Asserts that different seeds give different samples.
#[test]
fn different_seeds_give_different_samples() {
    let world = World::new("audit-seeds");
    for key in keys(300) {
        world.live_pack(&key);
    }

    let one = sample_of(&world, &["--audit-keys", "10", "--audit-seed", "1"]);
    let two = sample_of(&world, &["--audit-keys", "10", "--audit-seed", "2"]);

    assert_ne!(one, two);
}

/// Asserts that only live packs are sampled, even when the sample could hold all.
#[test]
fn only_live_packs_are_in_the_sample() {
    let world = World::new("audit-live");
    world.live_pack("cas/p/live-a");
    world.live_pack("cas/p/live-b");
    world.dead_pack("cas/p/dead");

    let sample = sample_of(&world, &[]);

    assert_eq!(
        sample,
        vec!["cas/p/live-a".to_owned(), "cas/p/live-b".to_owned()]
    );
}

/// Asserts that the sweep goes through every page of the index and the report counts the live
/// packs, and that a seed not given is the clock's, and said.
#[test]
fn the_sweep_pages_through_the_index_and_the_report_says_the_seed() {
    let world = World::new("audit-pages");
    for key in keys(2300) {
        world.live_pack(&key);
    }

    let sample = sample_of(
        &world,
        &[
            "--audit-keys",
            "20",
            "--report-json",
            &world.path("r.json").to_string_lossy(),
        ],
    );

    assert_eq!(sample.len(), 20);
    let report = std::fs::read_to_string(world.path("r.json")).expect("a report");
    let expected = r#"{
  "mode": "dry-run",
  "backfill": null,
  "audit": {
    "seed": 7,
    "requested": 20,
    "live_packs": 2300,
    "written": 20
  }
}"#;
    assert_eq!(report, expected);
}

/// Asserts that an index with no live pack gives an empty file.
#[test]
fn no_live_pack_gives_an_empty_file() {
    let world = World::new("audit-empty");

    let sample = sample_of(&world, &[]);

    assert!(sample.is_empty());
    assert_eq!(
        std::fs::read(world.path("sample.txt")).expect("a file"),
        b""
    );
}

/// Asserts that a dry run with a sample takes the sample alone: no tree is looked at.
#[test]
fn a_dry_run_with_a_sample_does_no_backfill() {
    let world = World::new("audit-alone");
    world.tree_without_list(1);
    world.live_pack("cas/p/x");

    sample_of(&world, &[]);

    assert_eq!(world.store.calls(), (0, 0, 0));
    assert_eq!(world.unlisted().len(), 1);
}

/// Asserts that an apply with a sample backfills first and then samples.
#[test]
fn an_apply_with_a_sample_backfills_and_samples() {
    let world = World::new("audit-both");
    world.tree_without_list(1);
    world.live_pack("cas/p/x");
    let path = world.path("sample.txt");

    let ran = world.run(&[
        "--mode",
        "apply",
        "--audit-keys-out",
        &path.to_string_lossy(),
    ]);

    assert_eq!(ran.code, 0, "{}", ran.err);
    says(&ran.out, "list gone 1");
    says(
        &ran.out,
        "audit sample: seed 7, 1 keys written of 1000 asked, 1 live packs",
    );
    assert!(world.unlisted().is_empty());
}

/// Asserts that a sweep that fails, and a sample file that cannot be written, end with status
/// 1 and no sample file.
#[test]
fn a_failed_sweep_or_file_fails_the_run() {
    let world = World::new("audit-fail");
    world.live_pack("cas/p/x");
    world.index.fail_live.store(true, Ordering::SeqCst);
    let path = world.path("sample.txt");

    let ran = world.run(&["--audit-keys-out", &path.to_string_lossy()]);
    assert_eq!(ran.code, 1);
    says(&ran.err, "sweeping the live packs");
    assert!(!path.exists());

    world.index.fail_live.store(false, Ordering::SeqCst);
    let nowhere = world.path("missing/sample.txt");
    let ran = world.run(&["--audit-keys-out", &nowhere.to_string_lossy()]);
    assert_eq!(ran.code, 1);
}
