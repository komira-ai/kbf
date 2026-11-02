//! The record of the writes in its file beside the state file: kept across a restart, dropped
//! (and the generation raised) when the state file holds another state than it describes, and
//! the state file kept in the form earlier builds read.

use std::fs;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};

use crate::desired::{Desired, DesiredStore, KBF, StoreError};
use crate::writes::{Record, Replaced, path_of};

fn dir(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

fn state(version: &str, storage: u32) -> Desired {
    Desired::parse(
        format!(r#"{{"version":"{version}","min_up":{{"storage":{storage},"frontend":1}}}}"#)
            .as_bytes(),
    )
    .expect("a state")
}

fn replaced(version: &str, storage: u32) -> Option<Replaced> {
    let old = state(version, storage);
    Some(Replaced {
        version: old.version,
        min_up: old.min_up,
    })
}

/// Catches: the record kept in memory only, so a restarted coordinator would answer
/// generation 0 and no `replaced` (a restore after a restart would find no baseline, and a
/// generation read before the restart could be reached again by other writes); and the
/// state file written in a form an earlier build cannot read.
#[test]
fn the_record_survives_a_restart_and_the_state_file_keeps_its_form() {
    let dir = dir("writes-restart");
    let path = dir.path().join("desired.json");
    let store = DesiredStore::open(&path).expect("a store");
    store.set(KBF, state("1.0", 2)).expect("set");
    let writer = store.writer();
    let made = writer
        .set(KBF, state("2.0", 1), "run-1".to_owned())
        .expect("set");
    drop(writer);
    let want = Record {
        generation: 2,
        replaced: replaced("1.0", 2),
        write_id: "run-1".to_owned(),
    };
    assert_eq!(made, want);
    let again = DesiredStore::open(&path).expect("reopened");
    assert_eq!(again.read(KBF), (Some(state("2.0", 1)), want));
    assert_eq!(
        Desired::parse(&fs::read(&path).expect("the state file")),
        Ok(state("2.0", 1))
    );
}

/// Catches: a record trusted for a state it does not describe. The state file replaced by
/// another writer (an earlier build, or a stop between the two files' writes) leaves a
/// record whose `replaced` and `write_id` are another state's: they are dropped and the
/// generation is raised, so a write conditional on the old generation is refused. A state
/// with no entry at all (the first start of this build) has the empty record.
#[test]
fn a_record_of_another_state_is_dropped_and_its_generation_raised() {
    let dir = dir("writes-other");
    let path = dir.path().join("desired.json");
    let store = DesiredStore::open(&path).expect("a store");
    store.set(KBF, state("1.0", 1)).expect("set");
    store.set(KBF, state("2.0", 1)).expect("set");
    drop(store);
    fs::write(
        &path,
        br#"{"version":"3.0","min_up":{"storage":1,"frontend":1}}"#,
    )
    .expect("another writer");
    let reopened = DesiredStore::open(&path).expect("reopened");
    assert_eq!(
        reopened.read(KBF),
        (
            Some(state("3.0", 1)),
            Record {
                generation: 3,
                ..Record::default()
            }
        )
    );
    fs::remove_file(path_of(&path)).expect("no record file");
    let fresh = DesiredStore::open(&path).expect("reopened");
    assert_eq!(fresh.read(KBF), (Some(state("3.0", 1)), Record::default()));
}

/// Catches: a record file this build does not understand read as no record (a silent reset
/// of the generation), and a key a later build adds refused (B8: a reader skips what it
/// does not know).
#[test]
fn an_unreadable_record_file_stops_the_start_and_an_unknown_key_is_skipped() {
    let dir = dir("writes-unreadable");
    let path = dir.path().join("desired.json");
    let store = DesiredStore::open(&path).expect("a store");
    store.set(KBF, state("1.0", 1)).expect("set");
    drop(store);
    let records = path_of(&path);
    let mut text: Value =
        serde_json::from_slice(&fs::read(&records).expect("the record file")).expect("JSON");
    text["later"] = json!(true);
    text["services"][KBF]["later"] = json!({"a": 1});
    fs::write(&records, text.to_string()).expect("write");
    let reopened = DesiredStore::open(&path).expect("unknown keys are skipped");
    assert_eq!(reopened.read(KBF).1.generation, 1);

    fs::write(&records, b"not json").expect("write");
    let refused = DesiredStore::open(&path).expect_err("refused");
    assert!(
        matches!(&refused, StoreError::Io { path, .. } if *path == records),
        "{refused}"
    );
    fs::remove_file(&records).expect("remove");
    fs::create_dir(&records).expect("a directory where the file goes");
    let refused = DesiredStore::open(&path).expect_err("refused");
    assert!(
        matches!(&refused, StoreError::Io { path, .. } if *path == records),
        "{refused}"
    );
}

/// Catches: a state set although its record could not be written, which would leave a state
/// with no generation raised for it: the write fails, and the state, the record and the state
/// file are as they were.
#[test]
fn a_record_that_cannot_be_written_sets_nothing() {
    let dir = dir("writes-unwritable");
    let path = dir.path().join("desired.json");
    let store = DesiredStore::open(&path).expect("a store");
    store.set(KBF, state("1.0", 1)).expect("set");
    let mut temp = path_of(&path).into_os_string();
    temp.push(".new");
    fs::create_dir(&temp).expect("a directory where the temporary file goes");
    let failed = store.writer().set(KBF, state("2.0", 1), "x".to_owned());
    assert!(matches!(failed, Err(StoreError::Io { .. })), "{failed:?}");
    let want = Record {
        generation: 1,
        ..Record::default()
    };
    assert_eq!(store.read(KBF), (Some(state("1.0", 1)), want));
    assert_eq!(
        Desired::parse(&fs::read(&path).expect("the state file")),
        Ok(state("1.0", 1))
    );
}

/// Catches: a write by the coordinator itself (its automatic rollback) not counted as a
/// write: it raises the generation, records the version it replaced, and has no id, so a
/// restore conditional on a read before it is refused and does not take it for its own.
#[test]
fn a_write_without_an_id_raises_the_generation_and_clears_the_id() {
    let dir = dir("writes-own");
    let store = DesiredStore::open(&dir.path().join("desired.json")).expect("a store");
    let writer = store.writer();
    writer
        .set(KBF, state("1.0", 1), "first".to_owned())
        .expect("set");
    writer
        .set(KBF, state("2.0", 1), "second".to_owned())
        .expect("set");
    drop(writer);
    store.set(KBF, state("1.0", 1)).expect("rolled back");
    assert_eq!(
        store.read(KBF).1,
        Record {
            generation: 3,
            replaced: replaced("2.0", 1),
            write_id: String::new(),
        }
    );
    assert_eq!(store.read("other"), (None, Record::default()));
}
