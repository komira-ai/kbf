//! The cold store's state, read again: the source the run asks before every pass is the one
//! preflight uses (`mc admin info` and the heal status file), and each read goes to the store
//! again.

mod ops_support;

use std::sync::Arc;

use tierstore_bench::coldstate::{ColdStateSource, OpsColdState};
use tierstore_bench::error::BenchError;

use ops_support::*;

fn source(name: &str) -> (kbf_testkit::dir::TestDir, Arc<FakeHost>, OpsColdState) {
    let (dir, host, config, ops) = base(name);
    let host = Arc::new(host);
    let spec = config
        .store(tierstore_bench::role::Role::Cold)
        .unwrap()
        .clone();
    let source = OpsColdState::new(host.clone(), ops, spec, dir.path().to_owned());
    (dir, host, source)
}

/// Catches a heal state that is read once and kept: the first read finds the heal under way
/// (a healing drive, 6.4 percent), the store is then changed to a healed one, and the second
/// read says so, with the drives, the time, and nothing that names the store. Both reads asked
/// the store (`mc admin info`) and did nothing else to it.
#[test]
fn each_read_asks_the_store_again() {
    let (_dir, host, source) = source("tb-cold-state-again");
    let first = source.read().unwrap().unwrap();
    assert!(first.healing);
    assert_eq!(first.percent_done, Some(6.4));
    assert_eq!(first.drives.len(), 2);
    assert_eq!(first.drives.iter().filter(|d| d.healing).count(), 1);

    host.answer(|_| ok(&info_json("RELEASE.2025-mark", false)));
    host.file("/private-mark/heal.json", r#"{"percent_done": 100.0}"#);
    let second = source.read().unwrap().unwrap();
    assert!(!second.healing);
    assert_eq!(second.percent_done, Some(100.0));
    assert!(second.drives.iter().all(|d| !d.healing));
    assert!(second.read_unix_ms >= first.read_unix_ms);

    let asked: Vec<String> = host.ran().iter().map(|c| c.args.join(" ")).collect();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(
        asked.iter().all(|a| a.ends_with("admin info tbops")),
        "{asked:?}"
    );
    let text = serde_json::to_string(&(first, second)).unwrap();
    for private in PRIVATE {
        assert!(!text.contains(private), "{private} in {text}");
    }
}

/// Catches a read that fails when only one of its two sources answers: the status file alone
/// is a reading (no drives), the admin info alone is a reading (no percent), and neither is an
/// error, as is a status file that is not one.
#[test]
fn one_source_is_enough_and_none_is_an_error() {
    let (_dir, host, source) = source("tb-cold-state-sources");
    host.answer(|_| failed(1, ""));
    let by_file = source.read().unwrap().unwrap();
    assert!(by_file.healing);
    assert!(by_file.drives.is_empty());

    host.answer(|_| ok(&info_json("v", true)));
    host.files
        .lock()
        .unwrap()
        .remove(std::path::Path::new("/private-mark/heal.json"));
    assert!(
        matches!(source.read(), Err(BenchError::Ops(_))),
        "an unreadable status file"
    );
    host.file("/private-mark/heal.json", "[]");
    let err = source.read().unwrap_err();
    assert!(err.to_string().starts_with("cold state:"), "{err}");
}

/// Catches a store that cannot be asked and has no status file being read as one that is not
/// healing.
#[test]
fn a_store_that_cannot_be_asked_and_no_status_file_is_an_error() {
    let (dir, host, config, mut ops) = base("tb-cold-state-neither");
    ops.cold_heal_status_file = None;
    host.answer(|_| failed(1, ""));
    let spec = config
        .store(tierstore_bench::role::Role::Cold)
        .unwrap()
        .clone();
    let source = OpsColdState::new(Arc::new(host), ops, spec, dir.path().to_owned());
    let err = source.read().unwrap_err();
    assert!(
        err.to_string()
            .contains("neither the admin info nor a status file"),
        "{err}"
    );
}
