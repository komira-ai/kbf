use std::fs;

use kbf_testkit::dir::TestDir;
use kbf_types::HostRole;

use crate::desired::{Desired, DesiredError, DesiredStore, MAX_COUNT, StoreError, dir_of};

const FULL: &str = r#"{"version":"0.1.9+gabcdefabcdef-2",
    "max_updating":{"frontend":1,"storage":2,"daemon":3,"mac-daemon":4},
    "min_up":{"storage":3,"frontend":2}}"#;

fn dir(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

/// Catches: a desired state read with other values than it was written with (each limit is
/// per role and `mac-daemon` is spelled with the hyphen), and limits left out not falling
/// back to the defaults (1, 1, 2, 2).
#[test]
fn a_desired_state_reads_every_limit_and_defaults_the_rest() {
    let d = Desired::parse(FULL.as_bytes()).expect("parsed");
    let got = HostRole::ALL.map(|r| d.max_updating.of(r));
    assert_eq!(got, [1, 2, 3, 4]);
    assert_eq!((d.min_up.storage, d.min_up.frontend), (3, 2));
    assert_eq!(d.version.as_str(), "0.1.9+gabcdefabcdef-2");
    let short = r#"{"version":"1.0","min_up":{"storage":1,"frontend":1}}"#;
    let d = Desired::parse(short.as_bytes()).expect("parsed");
    assert_eq!(HostRole::ALL.map(|r| d.max_updating.of(r)), [1, 1, 2, 2]);
    let partial =
        r#"{"version":"1.0","max_updating":{"daemon":7},"min_up":{"storage":1,"frontend":1}}"#;
    let d = Desired::parse(partial.as_bytes()).expect("parsed");
    assert_eq!(HostRole::ALL.map(|r| d.max_updating.of(r)), [1, 1, 7, 2]);
}

/// Catches: a desired state accepted that the host-side rule or the schema refuses: a
/// version with a shell word or an option, a missing minimum, an unknown field (a URL for the
/// release, say), a count over the cap, a negative count, and text that is no JSON.
#[test]
fn a_desired_state_that_is_not_one_is_refused() {
    let over = format!(
        r#"{{"version":"1.0","min_up":{{"storage":{},"frontend":1}}}}"#,
        MAX_COUNT + 1
    );
    let at_cap = format!(r#"{{"version":"1.0","min_up":{{"storage":{MAX_COUNT},"frontend":1}}}}"#);
    assert!(Desired::parse(at_cap.as_bytes()).is_ok());
    assert_eq!(Desired::parse(over.as_bytes()), Err(DesiredError::Count));
    let max_over = format!(
        r#"{{"version":"1.0","max_updating":{{"mac-daemon":{}}},"min_up":{{"storage":1,"frontend":1}}}}"#,
        MAX_COUNT + 1
    );
    assert_eq!(
        Desired::parse(max_over.as_bytes()),
        Err(DesiredError::Count)
    );
    for bad in [
        r#"{"version":"-1","min_up":{"storage":1,"frontend":1}}"#,
        r#"{"version":"1.0;id","min_up":{"storage":1,"frontend":1}}"#,
        r#"{"version":"1.0"}"#,
        r#"{"min_up":{"storage":1,"frontend":1}}"#,
        r#"{"version":"1.0","min_up":{"storage":1}}"#,
        r#"{"version":"1.0","min_up":{"storage":1,"frontend":1},"url":"https://x.example"}"#,
        r#"{"version":"1.0","max_updating":{"other":1},"min_up":{"storage":1,"frontend":1}}"#,
        r#"{"version":"1.0","min_up":{"storage":-1,"frontend":1}}"#,
        "not json",
        "",
    ] {
        assert!(
            matches!(
                Desired::parse(bad.as_bytes()),
                Err(DesiredError::Malformed(_))
            ),
            "{bad}"
        );
    }
    assert!(DesiredError::Count.to_string().contains("10000"));
}

/// Catches: the desired state lost by a restart: a state set is read back, equal, by a store
/// opened on the same file, and a second set replaces the first; a store on no file has none.
#[test]
fn the_desired_state_survives_a_restart() {
    let dir = dir("desired-restart");
    let file = dir.path().join("desired.json");
    let store = DesiredStore::open(&file).expect("opened");
    assert_eq!(store.get(), None);
    let first = Desired::parse(FULL.as_bytes()).expect("parsed");
    store.set(first.clone()).expect("stored");
    assert_eq!(store.get(), Some(first.clone()));
    drop(store);
    let again = DesiredStore::open(&file).expect("reopened");
    assert_eq!(again.get(), Some(first));
    let second = Desired::parse(br#"{"version":"1.0","min_up":{"storage":1,"frontend":1}}"#)
        .expect("parsed");
    again.set(second.clone()).expect("stored");
    drop(again);
    assert_eq!(
        DesiredStore::open(&file).expect("reopened").get(),
        Some(second)
    );
}

/// Catches: a state file written in place (a crash mid-write would leave half a state): the
/// file is replaced whole, and no temporary file is left beside it.
#[test]
fn the_state_file_is_replaced_whole_and_leaves_nothing_behind() {
    let dir = dir("desired-atomic");
    let file = dir.path().join("desired.json");
    let store = DesiredStore::open(&file).expect("opened");
    store
        .set(Desired::parse(FULL.as_bytes()).expect("parsed"))
        .expect("stored");
    let names: Vec<_> = fs::read_dir(dir.path())
        .expect("listed")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, ["desired.json"]);
    let text = fs::read_to_string(&file).expect("read");
    assert!(text.ends_with("}\n"), "{text:?}");
    assert_eq!(
        Desired::parse(text.as_bytes()).expect("whole"),
        store.get().expect("set")
    );
}

/// Catches: a state file that cannot be written changing the state in memory (the answer to
/// CI would say the state was set while a restart would lose it), and the failure not being
/// an error.
#[test]
fn a_state_that_cannot_be_written_is_not_set() {
    let dir = dir("desired-unwritable");
    let file = dir.path().join("no-such-dir").join("desired.json");
    let store = DesiredStore::open(&file).expect("a missing file is no state");
    let d = Desired::parse(FULL.as_bytes()).expect("parsed");
    assert!(matches!(store.set(d), Err(StoreError::Io { .. })));
    assert_eq!(store.get(), None);
}

/// Catches: a state file the coordinator cannot make sense of being taken for no state (it
/// would then order nothing, and a later set would overwrite what an operator wrote): start
/// refuses, and the error names the file.
#[test]
fn a_state_file_that_cannot_be_read_back_refuses_to_start() {
    let dir = dir("desired-bad");
    let file = dir.path().join("desired.json");
    fs::write(&file, "{}").expect("written");
    let why = DesiredStore::open(&file).expect_err("refused");
    assert!(matches!(why, StoreError::Invalid { .. }));
    assert!(why.to_string().contains("desired.json"), "{why}");
    let unreadable = dir.path().join("a-directory");
    fs::create_dir(&unreadable).expect("made");
    let why = DesiredStore::open(&unreadable).expect_err("refused");
    assert!(matches!(why, StoreError::Io { .. }));
}

/// Catches: the directory fsync of a state file with a bare name aimed at the empty path
/// (which is no directory), and a path with a parent aimed anywhere but at the parent.
#[test]
fn the_directory_of_a_bare_file_name_is_the_working_directory() {
    use std::path::Path;
    assert_eq!(dir_of(Path::new("desired.json")), Path::new("."));
    assert_eq!(dir_of(Path::new("state/desired.json")), Path::new("state"));
    assert_eq!(dir_of(Path::new("/desired.json")), Path::new("/"));
}
