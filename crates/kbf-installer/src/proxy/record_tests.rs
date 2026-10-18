//! `update.json` and the adoption rule, on a scratch directory.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::fakes::plan;
use super::model::*;
use super::ports::Recorder;
use super::record::{FileRecorder, LoadError};

fn dir(name: &str) -> PathBuf {
    kbf_host::testkit::scratch(&format!("proxy-{name}"))
}

fn intent(phase: Phase) -> Intent {
    let mut i = Intent::begin(&plan(Mode::Frontend), None, 1_700_000_000);
    i.phase = phase;
    i
}

#[test]
fn a_record_written_is_the_record_read_back() {
    let d = dir("roundtrip");
    let r = FileRecorder::new(&d);
    let mut i = intent(Phase::Baking);
    i.bake_started = Some(1_700_000_005);
    i.nodes_posted = true;
    r.record(&i).expect("write");
    assert_eq!(FileRecorder::load(&d).expect("read"), Some(i));
}

#[test]
fn a_later_record_replaces_the_earlier_and_leaves_no_temp_file() {
    let d = dir("replace");
    let r = FileRecorder::new(&d);
    r.record(&intent(Phase::Starting)).expect("write");
    r.record(&intent(Phase::Switched)).expect("write");
    let loaded = FileRecorder::load(&d).expect("read").expect("present");
    assert_eq!(loaded.phase, Phase::Switched);
    let names: Vec<_> = fs::read_dir(&d)
        .expect("ls")
        .map(|e| e.expect("e").file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("update.json")]);
}

#[test]
fn the_record_is_private_to_the_proxy_user() {
    let d = dir("mode");
    FileRecorder::new(&d)
        .record(&intent(Phase::Staged))
        .expect("write");
    let mode = fs::metadata(d.join("update.json"))
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_missing_record_is_none_and_a_corrupt_one_is_refused() {
    let d = dir("missing");
    assert!(matches!(FileRecorder::load(&d), Ok(None)));
    fs::write(d.join("update.json"), b"{\"update_id\": ").expect("seed");
    assert!(matches!(FileRecorder::load(&d), Err(LoadError::Corrupt(_))));
}

#[test]
fn an_unwritable_directory_stops_the_swap_before_it_acts() {
    let r = FileRecorder::new(&dir("gone").join("no/such/dir"));
    assert!(matches!(
        r.record(&intent(Phase::Staged)),
        Err(Halt::Port(_))
    ));
}

fn ident(pid: u32, start: u64, exe: &str) -> ProcIdent {
    ProcIdent {
        pid,
        start_time: start,
        exe: exe.into(),
    }
}

#[test]
fn adoption_needs_pid_start_time_and_executable_to_agree() {
    let recorded = ident(7, 100, "/r/a/kbf");
    assert_eq!(verify_ident(&recorded, &ident(7, 100, "/r/a/kbf")), Ok(()));
    let recycled = Err(AdoptError::Recycled { pid: 7 });
    assert_eq!(
        verify_ident(&recorded, &ident(7, 101, "/r/a/kbf")),
        recycled
    );
    assert_eq!(
        verify_ident(&recorded, &ident(7, 100, "/r/b/kbf")),
        recycled
    );
    assert_eq!(
        verify_ident(&recorded, &ident(8, 100, "/r/a/kbf")),
        recycled
    );
}
