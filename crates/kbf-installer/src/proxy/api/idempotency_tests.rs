use super::idempotency::{Idempotency, KEEP, write_atomic};
use crate::testkit::{A, B, c};

#[test]
fn an_entry_is_on_disk_when_insert_returns_and_ids_never_repeat() {
    let dir = kbf_host::testkit::scratch("u8a-idem-disk");
    let mut map = Idempotency::open(&dir);
    assert!(!map.lost());
    let first = map.insert("r1", &c(A), true, false).unwrap();
    assert!(!first.started);
    let reopened = Idempotency::open(&dir);
    assert_eq!(reopened.get("r1"), Some(&first));
    let mut reopened = reopened;
    let second = reopened.insert("r2", &c(B), false, true).unwrap();
    assert_ne!(first.update_id, second.update_id);
    assert_eq!(Idempotency::open(&dir).get("r2"), Some(&second));
    assert_eq!(Idempotency::open(&dir).get("r3"), None);
}

#[test]
fn started_is_remembered() {
    let dir = kbf_host::testkit::scratch("u8a-idem-started");
    let mut map = Idempotency::open(&dir);
    map.insert("r1", &c(A), true, false).unwrap();
    map.mark_started("r1").unwrap();
    map.mark_started("unknown").unwrap();
    assert!(Idempotency::open(&dir).get("r1").unwrap().started);
}

#[test]
fn only_the_newest_entries_are_kept_but_ids_keep_counting() {
    let dir = kbf_host::testkit::scratch("u8a-idem-keep");
    let mut map = Idempotency::open(&dir);
    for i in 0..KEEP + 6 {
        map.insert(&format!("r{i}"), &c(A), true, false).unwrap();
    }
    let map = Idempotency::open(&dir);
    assert_eq!(map.get("r0"), None);
    assert!(map.get("r5").is_none() && map.get("r6").is_some());
    assert_eq!(
        map.get(&format!("r{}", KEEP + 5)).unwrap().update_id.0,
        format!("upd-{}", KEEP + 6)
    );
}

#[test]
fn a_file_that_does_not_parse_is_lost_not_empty() {
    for (i, content) in [
        &b"{"[..],
        b"{\"schema\":2,\"next_seq\":1,\"entries\":[]}",
        b"{\"schema\":1,\"next_seq\":1,\"entries\":[],\"extra\":1}",
    ]
    .into_iter()
    .enumerate()
    {
        let dir = kbf_host::testkit::scratch(&format!("u8a-idem-lost-{i}"));
        std::fs::write(dir.join("requests.json"), content).unwrap();
        assert!(Idempotency::open(&dir).lost());
    }
    // A directory where the file should be is unreadable too.
    let dir = kbf_host::testkit::scratch("u8a-idem-lost-dir");
    std::fs::create_dir(dir.join("requests.json")).unwrap();
    assert!(Idempotency::open(&dir).lost());
}

#[test]
fn a_temporary_file_from_a_crash_is_ignored_and_replaced() {
    let dir = kbf_host::testkit::scratch("u8a-idem-tmp");
    let mut map = Idempotency::open(&dir);
    map.insert("r1", &c(A), true, false).unwrap();
    std::fs::write(dir.join(".requests.json.tmp"), b"half a wri").unwrap();
    let mut map = Idempotency::open(&dir);
    assert!(!map.lost() && map.get("r1").is_some());
    map.insert("r2", &c(B), true, false).unwrap();
    assert!(Idempotency::open(&dir).get("r2").is_some());
}

#[test]
fn write_atomic_leaves_only_the_file() {
    let dir = kbf_host::testkit::scratch("u8a-atomic");
    write_atomic(&dir, "f", b"one").unwrap();
    write_atomic(&dir, "f", b"two").unwrap();
    assert_eq!(std::fs::read(dir.join("f")).unwrap(), b"two");
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("f")]);
    assert!(write_atomic(&dir.join("missing"), "f", b"x").is_err());
}
