//! The record on disk: written whole and atomically, read back as written, refused when it is
//! not ours, and locked for one step at a time.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::record::{FILE, Record, SCHEMA, Store, StoreError, Was};
use crate::ports::Placed;
use crate::testutil::scratch;

fn store(name: &str) -> (Store, std::path::PathBuf) {
    let dir = scratch(name);
    (Store::new(dir.clone(), None), dir)
}

/// A record written is read back as it was, with every mark, and no temporary file is left
/// beside it; the next write replaces it whole.
///
/// Catches: a field lost on the way to disk; a temporary file left in the record directory.
#[test]
fn a_record_is_read_back_as_written() {
    let (store, dir) = store("hook-record-roundtrip");
    assert_eq!(store.load().unwrap(), None);
    let mut record = Record::new("2.0.0", "sha256:00");
    record.placement = Some(Was::In);
    record.drain_begun = true;
    record.deadline_unix = Some(10_600);
    store.save(&record).unwrap();
    assert_eq!(store.load().unwrap(), Some(record.clone()));
    record.rolled_back = true;
    store.save(&record).unwrap();
    assert_eq!(store.load().unwrap(), Some(record));
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [FILE]);
}

/// A record missing fields (one written by an older hook) reads with the missing marks unset;
/// one that is not JSON, or has the wrong types, is refused as not ours; one of a newer schema
/// is refused and not touched.
///
/// Catches: an older record refused for a field it never had; a newer one overwritten.
#[test]
fn a_record_that_is_not_ours_is_refused() {
    let (store, dir) = store("hook-record-refused");
    std::fs::write(
        dir.join(FILE),
        format!("{{\"schema\": {SCHEMA}, \"target\": \"2.0.0\", \"digest\": \"d\"}}"),
    )
    .unwrap();
    let older = store.load().unwrap().unwrap();
    assert_eq!(older, Record::new("2.0.0", "d"));
    for text in ["", "null", "{\"schema\": \"one\"}", "{\"prepared\": 3}"] {
        std::fs::write(dir.join(FILE), text).unwrap();
        let error = store.load().unwrap_err();
        assert!(
            matches!(error, StoreError::Corrupt { .. }),
            "{text:?}: {error}"
        );
        assert!(error.to_string().contains(FILE));
    }
    std::fs::write(dir.join(FILE), "{\"schema\": 99}").unwrap();
    let error = store.load().unwrap_err();
    assert!(
        matches!(error, StoreError::Newer { found: 99, .. }),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(FILE)).unwrap(),
        "{\"schema\": 99}"
    );
}

/// A directory the record cannot be read from or written to says so with its path; a failed
/// write leaves the old record the record.
///
/// Catches: a failed write that loses the record, or reports success.
#[test]
fn a_record_that_cannot_be_read_or_written_says_where() {
    let (store, dir) = store("hook-record-io");
    let record = Record::new("2.0.0", "d");
    store.save(&record).unwrap();
    // The record's own name is a directory: it cannot be read as a file.
    std::fs::remove_file(dir.join(FILE)).unwrap();
    std::fs::create_dir(dir.join(FILE)).unwrap();
    let error = store.load().unwrap_err();
    assert!(
        error.to_string().contains("cannot read the record in"),
        "{error}"
    );
    std::fs::remove_dir(dir.join(FILE)).unwrap();
    store.save(&record).unwrap();
    // The temporary name is taken by a directory: the write fails, the record stays.
    std::fs::create_dir(dir.join(format!("{FILE}.tmp"))).unwrap();
    let error = store.save(&Record::new("3.0.0", "d")).unwrap_err();
    assert!(
        error.to_string().contains("cannot write the record in"),
        "{error}"
    );
    assert_eq!(store.load().unwrap(), Some(record));
}

/// The gate is called before every write and not otherwise.
///
/// Catches: a write that cannot be stopped by a test, or a gate called on a read.
#[test]
fn the_gate_is_called_before_each_write() {
    let dir = scratch("hook-record-gate");
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let store = Store::new(
        dir,
        Some(Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        })),
    );
    store.load().unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
    store.save(&Record::default()).unwrap();
    store.save(&Record::default()).unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

/// The lock on the directory is exclusive for as long as the file lives, a second taker is told
/// it is busy, and a directory that is not there is another error.
///
/// Catches: two steps in one record directory.
#[test]
fn the_record_directory_is_locked_for_one_step_at_a_time() {
    let (store, dir) = store("hook-record-lock");
    let held = store.lock().unwrap();
    let error = store.lock().unwrap_err();
    assert!(matches!(error, StoreError::Busy), "{error}");
    assert!(
        error
            .to_string()
            .contains("another copy of this step is running")
    );
    drop(held);
    drop(store.lock().unwrap());
    let gone = Store::new(dir.join("absent"), None);
    let error = gone.lock().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot open the record directory"),
        "{error}"
    );
}

/// The marks of a record: the placement spells as it is written, an update is over when it was
/// rolled back or confirmed and given back, and is of the target and digest it names.
///
/// Catches: a record taken for another update's, or an unfinished update for a finished one.
#[test]
fn a_record_knows_whose_it_is_and_when_it_is_over() {
    assert_eq!(Was::from(Placed::In), Was::In);
    assert_eq!(Was::from(Placed::Out), Was::Out);
    assert_eq!(serde_json::to_string(&Was::Out).unwrap(), "\"out\"");
    let mut record = Record::new("2.0.0", "d");
    assert!(record.is_of("2.0.0", "d"));
    assert!(!record.is_of("2.0.1", "d") && !record.is_of("2.0.0", "e"));
    assert!(!record.is_over());
    record.confirmed = true;
    assert!(
        !record.is_over(),
        "confirmed, but the node is not yet given back"
    );
    record.given_back = true;
    assert!(record.is_over());
    let mut rolled = Record::new("2.0.0", "d");
    rolled.rolled_back = true;
    assert!(rolled.is_over());
}
