//! The eviction pass: what it removes, in what order, and why.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use super::LocalCache;
use super::evict::Pass;
use super::layout::name_of;
use super::metrics::{Evicted, read};
use super::testing::{Rig, pattern};
use super::{Disk, SystemDisk, locked};
use crate::Backend;

/// A cache of a thousand bytes with the marks at 90, 50 and 100 percent and the free-space
/// floor at `floor` percent, on a file system that reports itself empty, in which `count`
/// objects of `size` bytes named `cas/b/k0`, `cas/b/k1` and so on were read in that order.
async fn filled(name: &str, floor: u8, count: usize, size: usize) -> Rig {
    let rig = Rig::with(name, |config| {
        config.budget = 1000;
        config.high_water = 90;
        config.low_water = 50;
        config.critical = 100;
        config.free_floor = floor;
    });
    rig.disk.set_space(1000, 1000);
    for n in 0..count {
        let key = format!("cas/b/k{n}");
        rig.put(&key, &pattern(size)).await;
        assert_eq!(rig.get(&key).await.unwrap().len(), size);
    }
    rig
}

/// The names of the objects in the order of their use, the oldest first.
fn order(cache: &LocalCache) -> Vec<String> {
    let index = locked(&cache.inner.index);
    index
        .oldest_first()
        .into_iter()
        .map(|(hex, _)| hex)
        .collect()
}

/// Asserts that a fill that takes the cache past its high water mark evicts, down to the low
/// water mark, the objects used longest ago first, and that an object read again counts as
/// used: `k0` was read again before `k9` was filled, so `k1` to `k5` go and `k0` stays. A
/// defect that evicted in the order of the fills would take `k0`.
#[tokio::test]
async fn a_fill_past_high_water_evicts_the_oldest_down_to_low_water() {
    let rig = filled("tscache-evict-budget", 0, 9, 100).await;
    assert_eq!(rig.cache.held(), (900, 9));
    assert_eq!(rig.cache.evict(), Pass::default());
    let _ = rig.get("cas/b/k0").await.unwrap();

    rig.put("cas/b/k9", &pattern(100)).await;
    let _ = rig.get("cas/b/k9").await.unwrap();
    assert_eq!(rig.cache.held(), (500, 5));
    for gone in 1..=5 {
        assert!(!rig.has_file(&format!("cas/b/k{gone}")), "k{gone}");
    }
    for kept in [0, 6, 7, 8, 9] {
        assert!(rig.has_file(&format!("cas/b/k{kept}")), "k{kept}");
    }
    let counters = rig.cache.counters();
    assert_eq!(counters.evictions(Evicted::Budget), 5);
    assert_eq!(counters.evictions(Evicted::Floor), 0);
}

/// Asserts that a free-space floor evicts when the cache is under its budget, by what brings the
/// free space back to the floor (the free space is injected), and counts it as the floor's.
#[tokio::test]
async fn a_free_space_floor_evicts_below_the_budget() {
    let rig = filled("tscache-evict-floor", 10, 4, 40).await;
    // A hundred and fifty free of a thousand is over the floor of a hundred: nothing to do.
    rig.disk.set_space(1000, 150);
    assert_eq!(rig.cache.evict(), Pass::default());
    // Fifty free: fifty bytes are due, and two objects of forty are the least that frees them.
    rig.disk.set_space(1000, 50);
    let pass = rig.cache.evict();
    assert_eq!(
        pass,
        Pass {
            evicted: 2,
            freed: 80,
            pinned: 0
        }
    );
    assert_eq!(rig.cache.held(), (80, 2));
    let counters = rig.cache.counters();
    assert_eq!(counters.evictions(Evicted::Floor), 2);
    assert_eq!(counters.evictions(Evicted::Budget), 0);
}

/// Asserts that when both are due the first bytes freed are the budget's and the rest the
/// floor's: with 800 bytes held, 300 over the low water mark, and 500 due for the floor, five
/// objects go, three counted for the budget and two for the floor.
#[tokio::test]
async fn the_bytes_freed_are_charged_to_the_budget_first_and_then_to_the_floor() {
    let rig = Rig::with("tscache-evict-both", |config| {
        config.budget = 1000;
        config.high_water = 100;
        config.low_water = 99;
        config.critical = 100;
        config.free_floor = 0;
    });
    rig.disk.set_space(2000, 2000);
    for n in 0..8 {
        let key = format!("cas/b/k{n}");
        rig.put(&key, &pattern(100)).await;
        let _ = rig.get(&key).await.unwrap();
    }
    let strict = rig.reopen_with(|config| {
        config.high_water = 60;
        config.low_water = 50;
        config.free_floor = 30;
    });
    assert_eq!(strict.held(), (800, 8));
    // The floor is 30 percent of 2000, 600 bytes; 100 are free.
    rig.disk.set_space(2000, 100);
    let pass = strict.evict();
    assert_eq!(pass.evicted, 5);
    assert_eq!(pass.freed, 500);
    assert_eq!(strict.counters().evictions(Evicted::Budget), 3);
    assert_eq!(strict.counters().evictions(Evicted::Floor), 2);
    assert_eq!(strict.held(), (300, 3));
}

/// Asserts that an object with another name for its file (a link count over one) is skipped and
/// counted, that the next oldest goes in its place, and that it stays in the cache and in its
/// budget; the other name still reads.
#[tokio::test]
async fn an_object_with_another_link_is_skipped_and_counted() {
    let rig = filled("tscache-evict-pinned", 0, 9, 100).await;
    let elsewhere = rig.dir.path().join("a-job-tree-file");
    std::fs::hard_link(rig.file("cas/b/k0"), &elsewhere).unwrap();
    rig.put("cas/b/k9", &pattern(100)).await;
    let _ = rig.get("cas/b/k9").await.unwrap();
    // 500 bytes were due: the oldest, k0, is linked, so k1 to k5 went.
    assert!(rig.has_file("cas/b/k0"));
    assert!(!rig.has_file("cas/b/k5"));
    assert!(rig.has_file("cas/b/k6"));
    assert_eq!(rig.cache.held(), (500, 5));
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.pinned_skips), 1);
    assert_eq!(counters.evictions(Evicted::Budget), 5);
    assert_eq!(std::fs::read(&elsewhere).unwrap(), pattern(100));
}

/// Asserts that an entry whose file is gone is forgotten by a pass without counting an eviction,
/// and that a pass does nothing while another is running.
#[tokio::test]
async fn a_pass_forgets_a_vanished_file_and_does_not_run_twice() {
    let rig = filled("tscache-evict-vanished", 0, 9, 100).await;
    std::fs::remove_file(rig.file("cas/b/k0")).unwrap();
    rig.put("cas/b/k9", &pattern(100)).await;
    let running = &rig.cache.inner.evicting;
    running.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(rig.cache.evict(), Pass::default());
    running.store(false, std::sync::atomic::Ordering::SeqCst);
    let _ = rig.get("cas/b/k9").await.unwrap();
    // 500 bytes were due: k0 was found gone and forgotten, which frees nothing, then k1 to k5.
    assert_eq!(rig.cache.counters().evictions(Evicted::Budget), 5);
    assert_eq!(rig.cache.held(), (400, 4));
    assert!(!running.load(std::sync::atomic::Ordering::SeqCst));
}

/// Asserts that a use is written to the file once per pass however often the object was used,
/// with the time of the last use, and not again by the next pass.
#[tokio::test]
async fn uses_are_written_back_once_per_object_per_pass() {
    let rig = filled("tscache-evict-touch", 0, 2, 10).await;
    // The reads that filled them are uses too; write those back first.
    let _ = rig.cache.evict();
    rig.disk.touched.lock().unwrap().clear();
    for time in [5_000, 6_000, 7_000] {
        rig.clock.set(time);
        let _ = rig.get("cas/b/k0").await.unwrap();
    }
    let _ = rig.cache.evict();
    let touched = rig.disk.touched.lock().unwrap().clone();
    assert_eq!(touched, vec![(rig.file("cas/b/k0"), 7_000)]);
    let _ = rig.cache.evict();
    assert_eq!(rig.disk.touched.lock().unwrap().len(), 1);
    let modified = std::fs::metadata(rig.file("cas/b/k0"))
        .unwrap()
        .modified()
        .unwrap();
    let since = modified.duration_since(std::time::UNIX_EPOCH).unwrap();
    assert_eq!(since, Duration::from_millis(7_000));
}

/// Asserts that the order of use survives a restart: the times of the files are the seed, and a
/// use that a pass wrote back moves an object to the end of the order.
#[tokio::test]
async fn the_order_of_use_is_rebuilt_from_the_files_at_start() {
    let rig = filled("tscache-evict-restart", 0, 3, 10).await;
    let names: Vec<String> = (0..3)
        .map(|n| name_of(&format!("cas/b/k{n}")).hex)
        .collect();
    for (n, time) in [(0, 1_000), (1, 2_000), (2, 3_000)] {
        SystemDisk
            .touch(&rig.file(&format!("cas/b/k{n}")), time)
            .unwrap();
    }
    let again = rig.reopen();
    assert_eq!(order(&again), names);

    rig.clock.set(5_000);
    let _ = again.get("cas/b/k0", None).await.unwrap();
    let _ = again.evict();
    let third = rig.reopen();
    let expected = vec![names[1].clone(), names[2].clone(), names[0].clone()];
    assert_eq!(order(&third), expected);
}

/// Asserts that the loop runs a pass when its interval has passed and stops when it is halted,
/// writing the record of use before it returns. The runtime's clock is paused and moved by the
/// test, and the shortage of free space is injected, so no real time is waited for or asserted.
#[tokio::test(start_paused = true)]
async fn the_loop_runs_passes_until_it_is_halted_and_then_writes_the_uses() {
    let rig = filled("tscache-evict-loop", 10, 4, 40).await;
    rig.disk.set_space(1000, 50);
    let halt = Arc::new(Notify::new());
    let cache = rig.cache.clone();
    let stop = Arc::clone(&halt);
    let looping = tokio::spawn(async move { cache.maintain(Duration::from_secs(10), stop).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    while rig.cache.counters().evictions(Evicted::Floor) < 2 {
        tokio::task::yield_now().await;
    }
    rig.clock.set(9_000);
    let _ = rig.get("cas/b/k3").await.unwrap();
    halt.notify_one();
    looping.await.unwrap();
    let touched = rig.disk.touched.lock().unwrap().clone();
    assert!(touched.contains(&(rig.file("cas/b/k3"), 9_000)));
}
