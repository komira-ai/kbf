//! The stored form, staleness, and the bounds on the map.

use super::*;
use crate::codec::{LEN, checksum};

fn lived_in() -> Summary {
    let mut s = Summaries::default();
    let k = linux(1);
    let mut rng = Rng(7);
    for i in 0..700u32 {
        let run = if rng.below(50) == 0 {
            heavy_kill()
        } else {
            busy(rng.below(6000) + 1, Some(0))
        };
        s.observe(&k, &run, Day(DAY.0 + i / 40), &Bounds::default());
    }
    s.get(&k).unwrap().clone()
}

/// Rule 3.5: a summary round-trips, and the bytes are deterministic and fixed-size.
#[test]
fn a_summary_round_trips() {
    let summary = lived_in();
    assert!(summary.oom_floor() > 0 && summary.samples() == 700);
    let bytes = summary.encode();
    assert_eq!(bytes.len(), LEN);
    assert_eq!(bytes, summary.encode());
    assert_eq!(Summary::decode(&bytes), Some(summary));
    let fresh = Summary::new(Basis::MacFootprint, Day(3));
    assert_eq!(Summary::decode(&fresh.encode()), Some(fresh));
}

fn reseal(mut bytes: Vec<u8>) -> Vec<u8> {
    let sum = checksum(&bytes[..LEN - 8]);
    bytes[LEN - 8..].copy_from_slice(&sum.to_le_bytes());
    bytes
}

/// Rule 3.5: anything that is not exactly one valid record decodes to `None`.
#[test]
fn a_corrupt_row_does_not_decode() {
    let bytes = lived_in().encode();
    assert_eq!(Summary::decode(&[]), None);
    assert_eq!(Summary::decode(&bytes[..LEN - 1]), None);
    assert_eq!(Summary::decode(&[bytes.clone(), vec![0]].concat()), None);
    for at in 0..LEN {
        let mut flipped = bytes.clone();
        flipped[at] ^= 0x01;
        assert_eq!(Summary::decode(&flipped), None, "byte {at}");
    }
    // Damage that keeps the checksum valid is caught by the field checks.
    let with = |at: usize, value: &[u8]| {
        let mut bytes = bytes.clone();
        bytes[at..at + value.len()].copy_from_slice(value);
        reseal(bytes)
    };
    assert_eq!(Summary::decode(&with(0, &[2])), None, "version");
    assert_eq!(Summary::decode(&with(1, &[3])), None, "basis");
    assert_eq!(Summary::decode(&with(1, &[0])), None, "basis 0");
    let n = u64::from_le_bytes(bytes[2..10].try_into().unwrap());
    let beyond = (n + 1).to_le_bytes();
    assert!(
        Summary::decode(&with(2, &0u64.to_le_bytes())).is_none(),
        "anchor and kill after n"
    );
    assert!(
        Summary::decode(&with(30, &beyond)).is_none(),
        "anchor after n"
    );
    assert!(
        Summary::decode(&with(42, &beyond)).is_none(),
        "kill after n"
    );
}

/// A row that claims impossible counters or days is corrupt, so a decoded summary can
/// never overflow a counter or sit beyond staleness.
#[test]
fn absurd_counters_and_days_do_not_decode() {
    let bytes = lived_in().encode();
    let with = |at: usize, value: &[u8]| {
        let mut bytes = bytes.clone();
        bytes[at..at + value.len()].copy_from_slice(value);
        reseal(bytes)
    };
    let huge = u64::MAX.to_le_bytes();
    assert!(Summary::decode(&with(2, &huge)).is_none(), "n_total");
    assert!(Summary::decode(&with(10, &huge)).is_none(), "cpu_n");
    let day = (MAX_DAY + 1).to_le_bytes();
    assert!(Summary::decode(&with(18, &day)).is_none(), "last_seen");
    assert!(Summary::decode(&with(38, &day)).is_none(), "floor_day");
    assert!(Summary::decode(&with(50, &(MAX_DAY + RAISE_ONLY_DAYS + 1).to_le_bytes())).is_none());
    let stamp_at = LEN - 8 - 4 * 32 - 8 * 8 - 4 * 8;
    assert!(
        Summary::decode(&with(stamp_at, &day)).is_none(),
        "day_stamp"
    );
    assert!(Summary::decode(&with(18, &MAX_DAY.to_le_bytes())).is_some());
    // The state a caller cannot push past the bound: far-future days are clamped.
    let mut s = Summaries::default();
    s.observe(&linux(1), &success(GIB), Day(u32::MAX), &Bounds::default());
    assert_eq!(s.get(&linux(1)).unwrap().last_seen(), Day(MAX_DAY));
    assert!(Summary::decode(&s.get(&linux(1)).unwrap().encode()).is_some());
}

/// Rule 3.5: a stored row that does not decode, or is another basis, leaves the key
/// cold; a good row restores the same estimate, in the table its state belongs in.
#[test]
fn a_corrupt_row_leaves_the_key_cold() {
    let mut original = Summaries::default();
    let k = linux(1);
    original.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    feed(&mut original, &k, &success(GIB), 10);
    let bytes = original.get(&k.l2()).unwrap().encode();

    let mut restored = Summaries::default();
    let mut bad = bytes.clone();
    bad[40] ^= 0xff;
    assert_eq!(
        restored.insert_encoded(&k.l2(), &bad),
        Err(Fallback::Corrupt)
    );
    assert!(restored.is_empty());
    assert_eq!(estimate(&restored, &k.l2()), Err(Fallback::Missing));
    let mac = key(Basis::MacFootprint, None);
    assert_eq!(
        restored.insert_encoded(&mac, &bytes),
        Err(Fallback::Corrupt)
    );
    assert_eq!(restored.insert_encoded(&k.l2(), &bytes), Ok(()));
    assert_eq!(restored.len(), 1);
    assert_eq!(floor(&restored, &k), 16 * GIB);
    assert_eq!(estimate(&restored, &k.l2()), estimate(&original, &k.l2()));
    // Restoring over a key already held replaces it, wherever it was held.
    let clean = Summary::new(Basis::Cgroup, DAY).encode();
    assert_eq!(restored.insert_encoded(&k.l2(), &clean), Ok(()));
    assert_eq!(restored.len(), 1);
    assert_eq!(floor(&restored, &k), 0);
    assert_eq!(restored.insert_encoded(&k.l2(), &bytes), Ok(()));
    assert_eq!(restored.len(), 1);
}

// ---- staleness and the bounds on the map -------------------------------------------

/// Rule 4.3: idle more than 60 days is stale (no estimate, no floor); expire drops it.
#[test]
fn a_stale_summary_is_no_estimate() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    assert_eq!(estimate(&s, &k), Err(Fallback::Missing));
    feed(&mut s, &k, &success(GIB), 5);
    s.observe(&k, &heavy_kill(), DAY, &b);
    let at = |days: u32| s.estimate(&k, Day(DAY.0 + days), &b, &Hint::default());
    assert!(at(STALE_DAYS).is_ok());
    assert_eq!(at(STALE_DAYS + 1), Err(Fallback::Stale));
    assert_eq!(s.key_floor(&k, &b), 16 * GIB);
    // A clock that reads before the last sighting is not stale.
    assert!(s.estimate(&k, Day(DAY.0 - 5), &b, &Hint::default()).is_ok());
    s.expire(Day(DAY.0 + STALE_DAYS));
    assert_eq!(s.len(), 2);
    // A key with no safety state goes the same way when idle.
    feed(&mut s, &linux(2), &success(GIB), 5);
    assert_eq!(s.len(), 3);
    s.expire(Day(DAY.0 + STALE_DAYS + 1));
    assert_eq!(s.len(), 2, "only the keys holding a floor remain");
}

/// A floor is safety state: it does not expire by idleness. After 60 idle days the
/// estimate is gone but the floor is not, and `expire` keeps it and drops the samples;
/// it then decays only by 256 new runs and 7 days.
#[test]
fn idleness_does_not_expire_a_floor() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    feed(&mut s, &k, &success(GIB), 10);
    s.observe(&k, &heavy_kill(), DAY, &b);
    let idle = Day(DAY.0 + 400);
    assert_eq!(
        s.estimate(&k, idle, &b, &Hint::default()),
        Err(Fallback::Stale)
    );
    assert_eq!(s.key_floor(&k, &b), 16 * GIB);
    s.expire(idle);
    assert_eq!(s.len(), 2);
    assert_eq!(s.key_floor(&k, &b), 16 * GIB);
    let kept = s.get(&k).unwrap().clone();
    assert_eq!((kept.samples(), kept.oom_floor()), (0, 16 * GIB));
    // Stripping is idempotent, and a new run revives the key under its floor.
    s.expire(idle);
    assert_eq!(s.get(&k).unwrap(), &kept);
    s.observe(&k, &success(GIB), idle, &b);
    assert_eq!(s.key_floor(&k, &b), 16 * GIB);
    assert!(
        s.estimate(&k, idle, &b, &Hint::default())
            .unwrap()
            .memory_bytes
            >= 16 * GIB
    );
}

fn shape_key(instance: &str, shape: u64) -> Key {
    Key::new(
        Instance::new(instance),
        PlatformClass::new("p"),
        Basis::Cgroup,
        Shape(shape),
        None,
    )
}

/// Rule 3.2: at most `max_keys` keys; the least recently seen go, ties by key, and the
/// key just observed is never the one dropped.
#[test]
fn the_oldest_keys_are_evicted() {
    let b = Bounds::default();
    let mut s = Summaries::with_limits(4, 4, 4);
    let none = |shape| shape_key("main", shape);
    for (shape, day) in [(5, 10), (6, 11), (7, 12), (8, 13)] {
        s.observe(&none(shape), &success(GIB), Day(day), &b);
    }
    assert_eq!(s.len(), 4);
    s.observe(&none(9), &success(GIB), Day(14), &b);
    assert_eq!(s.len(), 4);
    assert!(s.get(&none(5)).is_none() && s.get(&none(9)).is_some());
    // A new key that is older than every other is still kept; the oldest other goes.
    s.observe(&none(1), &success(GIB), Day(1), &b);
    assert!(s.get(&none(1)).is_some() && s.get(&none(6)).is_none());
    // Equal days: the smaller key goes.
    let mut tie = Summaries::with_limits(2, 2, 2);
    for shape in [3, 2, 4] {
        tie.observe(&none(shape), &success(GIB), Day(1), &b);
    }
    assert!(
        tie.get(&none(2)).is_none() && tie.get(&none(3)).is_some() && tie.get(&none(4)).is_some()
    );
    // A key with a configuration needs two slots; a limit under two is raised to two.
    let mut two = Summaries::with_limits(0, 0, 0);
    two.observe(&linux(1), &success(GIB), DAY, &b);
    assert_eq!(two.len(), 2);
    two.observe(&linux(2), &success(GIB), Day(DAY.0 + 1), &b);
    assert_eq!(two.len(), 2);
    assert!(two.get(&linux(2)).is_some() && two.get(&linux(2).l2()).is_some());
}

/// The documented bound: 65,536 keys, and no more than a quarter of them one
/// instance's.
#[test]
fn the_map_is_bounded_at_65536_keys() {
    assert_eq!((MAX_KEYS, MAX_KEYS_PER_INSTANCE), (65_536, 16_384));
    let b = Bounds::default();
    let mut s = Summaries::default();
    for instance in 0..8 {
        for shape in 0..8192 {
            let key = shape_key(&format!("i{instance}"), shape);
            s.observe(&key, &success(GIB), DAY, &b);
        }
    }
    assert_eq!(s.len(), MAX_KEYS);
    s.observe(&shape_key("i9", 0), &success(GIB), DAY, &b);
    assert_eq!(s.len(), MAX_KEYS);
    assert!(
        s.get(&shape_key("i0", 0)).is_none(),
        "the oldest, by key, went"
    );
    assert!(s.get(&shape_key("i9", 0)).is_some());
}

/// S13: a key holding a floor lives in a separate table that the least-recently-seen
/// eviction never touches.
#[test]
fn a_key_with_a_floor_outlives_eviction() {
    let b = Bounds::default();
    let mut s = Summaries::with_limits(3, 3, 4);
    let first = shape_key("main", 1);
    s.observe(&first, &heavy_kill(), Day(1), &b);
    for shape in 2..12 {
        s.observe(
            &shape_key("main", shape),
            &success(GIB),
            Day(1 + shape as u32),
            &b,
        );
    }
    assert_eq!(s.len(), 4, "three evictable keys and the one with a floor");
    assert_eq!(s.key_floor(&first, &b), 16 * GIB);
}

/// S13: the safety table is bounded, and when it is full a new key with a floor is
/// refused a place there (counted), never dropped: it stays in the main table with
/// its floor and every pinned key keeps its own.
#[test]
fn a_full_safety_table_refuses_and_counts_never_drops() {
    let b = Bounds::default();
    let mut s = Summaries::with_limits(10, 10, 8);
    for shape in 1..=3 {
        s.observe(
            &shape_key("main", shape),
            &heavy_kill(),
            Day(shape as u32),
            &b,
        );
    }
    assert_eq!(s.unpinned(), 1);
    assert_eq!(s.len(), 3);
    for shape in 1..=3 {
        assert_eq!(s.key_floor(&shape_key("main", shape), &b), 16 * GIB);
    }
    // Observing a pinned key again is not a refusal.
    s.observe(&shape_key("main", 1), &success(GIB), Day(9), &b);
    assert_eq!(s.unpinned(), 1);
    assert_eq!(s.len(), 3);
}

/// S13: one instance cannot flush another's keys: it is held to its quota, and the
/// global limit is reached only through other instances' idle keys.
#[test]
fn an_instance_cannot_flush_another() {
    let b = Bounds::default();
    let mut s = Summaries::with_limits(6, 3, 4);
    s.observe(&shape_key("b", 1), &success(GIB), Day(1), &b);
    s.observe(&shape_key("b", 2), &success(GIB), Day(2), &b);
    for shape in 0..20 {
        s.observe(
            &shape_key("a", shape),
            &success(GIB),
            Day(3 + shape as u32),
            &b,
        );
    }
    assert!(s.get(&shape_key("b", 1)).is_some() && s.get(&shape_key("b", 2)).is_some());
    let held = (0..20)
        .filter(|shape| s.get(&shape_key("a", *shape)).is_some())
        .count();
    assert_eq!(held, 3);
    assert_eq!(s.len(), 5);
}

/// One tenant cannot fill the safety table: an instance holds at most a quarter of its
/// slots, so another instance's new floor is still pinned (and survives eviction).
#[test]
fn an_instance_cannot_fill_the_safety_table() {
    let b = Bounds::default();
    let mut s = Summaries::with_limits(3, 3, 8);
    for shape in 0..8 {
        s.observe(
            &shape_key("a", shape),
            &heavy_kill(),
            Day(1 + shape as u32),
            &b,
        );
    }
    assert_eq!(s.unpinned(), 6, "a holds two of the eight slots");
    let theirs = shape_key("b", 1);
    s.observe(&theirs, &heavy_kill(), Day(20), &b);
    assert_eq!(s.unpinned(), 6, "b's floor was pinned");
    for shape in 0..10 {
        s.observe(
            &shape_key("c", shape),
            &success(GIB),
            Day(30 + shape as u32),
            &b,
        );
    }
    assert_eq!(s.key_floor(&theirs, &b), 16 * GIB);
}

/// When every slot is taken, by several instances within their shares, one more
/// instance's floor is refused and counted, not dropped.
#[test]
fn a_full_table_refuses_another_instance() {
    let b = Bounds::default();
    let mut s = Summaries::with_limits(10, 10, 8);
    for (instance, shape) in [
        ("a", 0),
        ("a", 1),
        ("b", 0),
        ("b", 1),
        ("c", 0),
        ("c", 1),
        ("d", 0),
        ("d", 1),
    ] {
        s.observe(&shape_key(instance, shape), &heavy_kill(), DAY, &b);
    }
    assert_eq!(s.unpinned(), 0);
    let late = shape_key("e", 0);
    s.observe(&late, &heavy_kill(), DAY, &b);
    assert_eq!((s.unpinned(), s.len()), (1, 9));
    assert_eq!(s.key_floor(&late, &b), 16 * GIB);
}

/// Catches: a key's parts unreadable or swapped (a caller that stores a summary under a
/// name made of its key cannot rebuild the key on restart, or rebuilds another key).
#[test]
fn a_key_names_its_parts() {
    let key = Key::new(
        Instance::new("main"),
        PlatformClass::new("linux/x86_64"),
        Basis::MacFootprint,
        Shape(7),
        Some(Cfg(9)),
    );
    assert_eq!(key.instance().as_str(), "main");
    assert_eq!(key.platform().as_str(), "linux/x86_64");
    assert_eq!(key.basis(), Basis::MacFootprint);
    assert_eq!((key.shape(), key.cfg()), (Shape(7), Some(Cfg(9))));
    assert_eq!(key.l2().cfg(), None);
}
