//! The record of the frontend: whole or not at all, and only what is needed.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use kbf_testkit::dir::TestDir;

use super::config::FrontendConfig;
use super::record::{Record, Slot, Trial};
use crate::trust::Trust;

fn slot(commit: &str) -> Slot {
    Slot {
        commit: commit.to_owned(),
        image: format!("sha256:{commit}"),
    }
}

fn trial() -> Trial {
    Trial {
        commit: "n".to_owned(),
        image: "sha256:n".to_owned(),
        from: "o".to_owned(),
        from_image: "sha256:o".to_owned(),
        deadline_unix: Some(7),
        config: FrontendConfig::parse(
            "[frontend]\nname = \"f\"\nrestart = \"unless-stopped\"\napi = \"127.0.0.1:1\"\ndrain_token_file = \"/t\"\ndrain_secs = 180\n",
        )
        .unwrap(),
    }
}

fn rig(name: &str) -> (TestDir, Trust) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
    let uid = fs::metadata(&*dir).unwrap().uid();
    let trust = Trust::owned_by(uid, &dir);
    (dir, trust)
}

/// A host with no file has an empty record; what is saved is read back, in a file only its
/// owner can read, with no half-written file left beside it.
#[test]
fn a_record_is_saved_whole_and_read_back() {
    let (dir, trust) = rig("hi-fe-record-roundtrip");
    let path = dir.join("frontend.json");
    assert_eq!(Record::load(&trust, &path).unwrap(), Record::default());

    let mut record = Record::default();
    record.images.insert("n".to_owned(), "sha256:n".to_owned());
    record.current = Some(slot("o"));
    record.pending = Some(trial());
    record.save(&path).unwrap();
    assert_eq!(Record::load(&trust, &path).unwrap(), record);
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
    assert!(!dir.join("frontend.json.new").exists());

    // A second save replaces the first.
    record.pending = None;
    record.save(&path).unwrap();
    assert_eq!(Record::load(&trust, &path).unwrap().pending, None);
}

/// A file that is not the host's, or not a record, or a path that cannot be written, is an
/// error that names the file.
#[test]
fn a_file_that_is_not_believed_or_cannot_be_written_is_an_error() {
    let (dir, trust) = rig("hi-fe-record-errors");
    let path = dir.join("frontend.json");
    fs::write(&path, "{}").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(
        Record::load(&trust, &path)
            .unwrap_err()
            .contains("frontend.json")
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(Record::load(&trust, &path).unwrap(), Record::default());
    fs::write(&path, "{\"images\": 3}").unwrap();
    assert!(
        Record::load(&trust, &path)
            .unwrap_err()
            .contains("frontend.json")
    );
    fs::write(&path, "{\"unknown\": 3}").unwrap();
    assert!(Record::load(&trust, &path).is_err());

    // A path whose directory is not there, and a path that cannot be looked at.
    let missing = dir.join("gone/frontend.json");
    assert!(
        Record::default()
            .save(&missing)
            .unwrap_err()
            .contains("frontend.json")
    );
    assert_eq!(Record::load(&trust, &missing).unwrap(), Record::default());
    let long = dir.join("x".repeat(300)).join("frontend.json");
    assert!(
        Record::load(&trust, &long)
            .unwrap_err()
            .contains("frontend.json")
    );
}

/// Only the images of the release that runs, the one on trial, the one it replaced and the
/// one just loaded are kept.
#[test]
fn only_the_images_that_are_needed_are_kept() {
    let mut record = Record::default();
    for commit in ["a", "b", "c", "n", "o", "z"] {
        record
            .images
            .insert(commit.to_owned(), format!("sha256:{commit}"));
    }
    record.current = Some(slot("a"));
    record.pending = Some(trial());
    record.prune("z");
    let kept: Vec<&str> = record.images.keys().map(String::as_str).collect();
    assert_eq!(kept, ["a", "n", "o", "z"]);

    record.pending = None;
    record.current = None;
    record.prune("b");
    let kept: Vec<&str> = record.images.keys().map(String::as_str).collect();
    assert!(kept.is_empty(), "{kept:?}");
}

/// What a record is on disk, as the helper wrote it before `group_add` existed: a trial's
/// configuration has no `group_add` key. A host that sets no group writes exactly these bytes
/// (an older helper, which refuses unknown keys, still reads them), and these bytes load.
const PRE_GROUP_ADD_RECORD: &str = r#"{"images":{"n":"sha256:n"},"current":{"commit":"o","image":"sha256:o"},"pending":{"commit":"n","image":"sha256:n","from":"o","from_image":"sha256:o","deadline_unix":7,"config":{"run":{"name":"f","restart":"unless-stopped","network":null,"user":null,"ports":[],"mounts":[],"command":[]},"api":"127.0.0.1:1","drain_token_file":"/t","peers":[],"drain_secs":180,"ready_secs":90}}}"#;

fn pre_group_add_record() -> Record {
    let mut record = Record::default();
    record.images.insert("n".to_owned(), "sha256:n".to_owned());
    record.current = Some(slot("o"));
    record.pending = Some(trial());
    record
}

#[test]
fn a_record_with_no_groups_is_written_without_the_key() {
    let (dir, _trust) = rig("hi-fe-record-nogroups");
    let path = dir.join("frontend.json");
    pre_group_add_record().save(&path).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let expected: serde_json::Value = serde_json::from_str(PRE_GROUP_ADD_RECORD).unwrap();
    assert_eq!(written, expected);
    assert!(!fs::read_to_string(&path).unwrap().contains("group_add"));
}

#[test]
fn a_record_from_before_group_add_loads() {
    let (dir, trust) = rig("hi-fe-record-before");
    let path = dir.join("frontend.json");
    fs::write(&path, PRE_GROUP_ADD_RECORD).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(Record::load(&trust, &path).unwrap(), pre_group_add_record());
}

#[test]
fn a_record_with_groups_keeps_them() {
    let (dir, trust) = rig("hi-fe-record-groups");
    let path = dir.join("frontend.json");
    let mut record = pre_group_add_record();
    record.pending.as_mut().unwrap().config.run.group_add = vec![990];
    record.save(&path).unwrap();
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("\"group_add\":[990]")
    );
    assert_eq!(Record::load(&trust, &path).unwrap(), record);
}
