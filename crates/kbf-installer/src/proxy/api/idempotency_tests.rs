use super::idempotency::{Idempotency, KEEP, Waiting, write_atomic};
use crate::proxy::model::{Locator, Verb};
use crate::testkit::{A, B, c};

fn waiting(commit: &str, deadline_s: u64) -> Waiting {
    Waiting {
        from: Some(c(A)),
        order: Locator {
            order_id: 101,
            epoch: 1,
            generation: 1,
            verb: Verb::FrontendUpdate,
            commit: c(commit),
            window_s: 900,
        },
        since_s: deadline_s - 900,
        deadline_s,
    }
}

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
fn started_is_remembered_with_what_the_update_waits_for() {
    let dir = kbf_host::testkit::scratch("u8a-idem-started");
    let mut map = Idempotency::open(&dir);
    map.insert("r1", &c(A), true, false).unwrap();
    assert!(
        map.waiting().is_none(),
        "an accepted request waits for nothing"
    );
    map.mark_started("r1", waiting(B, 5_000)).unwrap();
    map.mark_started("unknown", waiting(B, 5_000)).unwrap();
    let reopened = Idempotency::open(&dir);
    let entry = reopened.get("r1").unwrap();
    assert!(entry.started);
    assert_eq!(entry.waiting, Some(waiting(B, 5_000)));
    let (found, w) = reopened.waiting().expect("the update that waits");
    assert_eq!((found.request_id.as_str(), w.deadline_s), ("r1", 5_000));
}

#[test]
fn an_update_stops_waiting_durably_and_only_when_it_did() {
    let dir = kbf_host::testkit::scratch("u8a-idem-finish");
    let state = dir.join("state");
    std::fs::create_dir(&state).unwrap();
    let mut map = Idempotency::open(&state);
    map.insert("r1", &c(A), true, false).unwrap();
    map.mark_started("r1", waiting(B, 5_000)).unwrap();
    map.finish_waiting("unknown").unwrap();
    assert!(
        Idempotency::open(&state).waiting().is_some(),
        "not r1's end"
    );
    map.finish_waiting("r1").unwrap();
    let reopened = Idempotency::open(&state);
    assert!(reopened.waiting().is_none());
    assert!(reopened.get("r1").unwrap().started, "started stays");
    // Nothing waits, so nothing is written: with the directory gone a save would fail.
    std::fs::rename(&state, dir.join("moved")).unwrap();
    map.finish_waiting("r1").unwrap();
}

#[test]
fn a_new_wait_ends_the_wait_that_could_not_be_saved_as_over() {
    let dir = kbf_host::testkit::scratch("u8a-idem-one-wait");
    let mut map = Idempotency::open(&dir);
    map.insert("r1", &c(A), true, false).unwrap();
    map.insert("r2", &c(B), true, false).unwrap();
    map.mark_started("r1", waiting(A, 5_000)).unwrap();
    map.mark_started("r2", waiting(B, 9_000)).unwrap();
    let reopened = Idempotency::open(&dir);
    assert_eq!(reopened.get("r1").unwrap().waiting, None);
    let (found, w) = reopened.waiting().expect("r2 waits");
    assert_eq!((found.request_id.as_str(), w.deadline_s), ("r2", 9_000));
}

#[test]
fn a_file_written_before_waiting_existed_reads_as_nothing_waiting() {
    let dir = kbf_host::testkit::scratch("u8a-idem-old-file");
    let entry = format!(
        "{{\"request_id\":\"r1\",\"update_id\":\"upd-1\",\"commit\":\"{}\",\"nodes\":true,\
         \"rollback\":false,\"started\":true}}",
        c(A).as_str()
    );
    let file = format!("{{\"schema\":1,\"next_seq\":2,\"entries\":[{entry}]}}");
    std::fs::write(dir.join("requests.json"), file).unwrap();
    let map = Idempotency::open(&dir);
    assert!(!map.lost());
    assert!(map.get("r1").unwrap().started);
    assert!(map.waiting().is_none());
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
