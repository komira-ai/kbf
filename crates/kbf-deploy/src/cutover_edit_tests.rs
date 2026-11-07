//! The line edit of `kbf.toml`, its refusals, and the staging, placing and restoring of the
//! file, each in a scratch directory of its own.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use super::*;
use crate::cutover::Target;
use crate::cutover::tests::{ready_config, scratch, target, write};

fn settings(dir: &Path, url: &str, ca: bool) -> Settings {
    let t: Target = target(dir, &dir.join("kbf.toml"), url, ca);
    Settings::of(&t).unwrap()
}

/// Behaviour: the edit puts the switch's lines after the `[executor]` header, takes out the
/// lines of the keys it sets, and keeps every comment, other key and table as it was.
#[test]
fn the_edit_changes_only_the_switchs_keys() {
    let dir = scratch("edit");
    let s = settings(dir.path(), "https://192.0.2.30:8443", true);
    let path = dir.path().join("kbf.toml");
    let new = rewritten(&path, &ready_config(), &s).unwrap();
    let token = dir.path().join("job-service.token").display().to_string();
    let ca = dir.path().join("ca.pem").display().to_string();
    let expected = ready_config().replace(
        "[executor]\n# where actions run\nexecutor = \"kbf-daemon\"\n",
        &format!(
            "[executor]\nexecutor = \"js-agent\"\n\
             executor-job-url = \"https://192.0.2.30:8443\"\n\
             executor-job-token-file = \"{token}\"\nexecutor-job-ca-file = \"{ca}\"\n\
             executor-hosts = 4\nexecutor-host-cpus = 16\nexecutor-host-memory-gib = 64\n\
             executor-host-slots = 8\n# where actions run\n"
        ),
    );
    let expected = expected.replace("executor-host-slots = 8\n\n[packs]", "\n[packs]");
    assert_eq!(new, expected);
}

/// Behaviour: an `http` URL removes a CA file the table had; a file with no `[executor]`
/// table gets one at its end; a key of the same name in another table is left alone.
#[test]
fn an_http_switch_removes_the_ca_and_a_table_is_added_when_there_is_none() {
    let dir = scratch("edit-http");
    let s = settings(dir.path(), "http://127.0.0.1:9443", false);
    let path = dir.path().join("kbf.toml");
    let old = "[executor]\nexecutor-job-ca-file = \"/old/ca.pem\" # the old CA\n\n[other]\nexecutor-hosts = 1\n";
    let new = rewritten(&path, old, &s).unwrap();
    assert!(!new.contains("/old/ca.pem"), "{new}");
    assert!(new.contains("[other]\nexecutor-hosts = 1\n"), "{new}");
    let new = rewritten(&path, "[scheduler]\nx = 1", &s).unwrap();
    assert!(
        new.starts_with("[scheduler]\nx = 1\n\n[executor]\nexecutor = \"js-agent\"\n"),
        "{new}"
    );
    let new = rewritten(&path, "", &s).unwrap();
    assert!(
        new.starts_with("[executor]\nexecutor = \"js-agent\"\n"),
        "{new}"
    );
}

/// Behaviour: a file the line edit cannot rewrite exactly is refused, saying to make the
/// change by hand: a dotted key, a quoted key, a value over several lines, a header with spaces.
#[test]
fn a_file_the_line_edit_cannot_rewrite_is_refused() {
    let dir = scratch("edit-refused");
    let s = settings(dir.path(), "http://127.0.0.1:9443", false);
    let path = dir.path().join("kbf.toml");
    for (old, why) in [
        ("executor.executor-hosts = 2\n", "the result is not TOML"),
        ("[executor]\n\"executor-hosts\" = 2\n", "the result is not"),
        (
            "[executor]\nexecutor-hosts = [\n  2,\n]\n",
            "the result is not TOML",
        ),
        (
            "[ executor ]\nexecutor = \"kbf-daemon\"\n",
            "the result is not TOML",
        ),
        (
            "[executor]\nexecutor-work-dir = \"\"\"\nexecutor-hosts = 3\n\"\"\"\n",
            "the result is not the file with only the switch's keys changed",
        ),
    ] {
        let e = rewritten(&path, old, &s).unwrap_err();
        assert_eq!(e.code(), 2, "{old}");
        assert!(e.to_string().contains(why), "{old}: {e}");
        assert!(e.to_string().ends_with("make the change by hand"), "{e}");
    }
    let e = rewritten(&path, "[x\n", &s).unwrap_err();
    assert!(e.to_string().contains("is not TOML"), "{e}");
    let e = rewritten(&path, "executor = 1\n", &s).unwrap_err();
    assert!(e.to_string().contains("is not a table"), "{e}");
}

/// Behaviour: staging keeps the original byte for byte with the file's mode, and the
/// staged text with it too; placing renames the staged file over the file and keeps the
/// backup; restoring puts the original back and the backup is gone.
#[test]
fn stage_place_and_restore_keep_the_original_byte_for_byte() {
    let dir = scratch("stage");
    let file = write(dir.path(), "kbf.toml", "old\n");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
    let staged = Staged::write(&file, "old\n", "new\n").unwrap();
    for (path, text) in [(&staged.backup, "old\n"), (&staged.staged, "new\n")] {
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o7777, 0o640);
    }
    let backup = staged.backup.clone();
    staged.put_in_place().unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");
    assert!(!beside(&file, STAGED_SUFFIX).exists());
    let e = require_no_backup(&file).unwrap_err();
    assert!(e.to_string().contains("is there already"), "{e}");
    let (found, text) = read_backup(&file).unwrap();
    assert_eq!(
        (found.as_path(), text.as_str()),
        (backup.as_path(), "old\n")
    );
    restore(&file, &found).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
    assert!(!backup.exists());
    let e = read_backup(&file).unwrap_err();
    assert!(
        e.to_string().contains("there is no switch to back out"),
        "{e}"
    );
}

/// Behaviour: a staging that is dropped before it is placed leaves no file of its own; a
/// staged file that is there already is refused and left alone.
#[test]
fn a_dropped_staging_removes_its_files_and_a_stale_one_is_refused() {
    let dir = scratch("stage-drop");
    let file = write(dir.path(), "kbf.toml", "old\n");
    drop(Staged::write(&file, "old\n", "new\n").unwrap());
    assert!(!beside(&file, BACKUP_SUFFIX).exists());
    assert!(!beside(&file, STAGED_SUFFIX).exists());
    let stale = write(dir.path(), "kbf.toml.job-service-new", "theirs\n");
    let e = Staged::write(&file, "old\n", "new\n").unwrap_err();
    assert_eq!(e.code(), 2);
    assert_eq!(std::fs::read_to_string(&stale).unwrap(), "theirs\n");
    assert!(!beside(&file, BACKUP_SUFFIX).exists());
}

/// Behaviour: a missing file, a place that cannot be renamed over, and a backup that is not a
/// regular file are each said, and change nothing else.
#[test]
fn files_that_cannot_be_read_or_renamed_are_reported() {
    let dir = scratch("stage-fail");
    let gone = dir.path().join("gone.toml");
    assert_eq!(Staged::write(&gone, "", "").unwrap_err().code(), 2);
    assert_eq!(canonical(&gone).unwrap_err().code(), 2);
    let file = write(dir.path(), "kbf.toml", "old\n");
    let staged = Staged::write(&file, "old\n", "new\n").unwrap();
    std::fs::remove_file(&staged.staged).unwrap();
    assert!(
        staged
            .put_in_place()
            .unwrap_err()
            .to_string()
            .starts_with("cannot rename ")
    );
    std::fs::create_dir(beside(&file, BACKUP_SUFFIX)).unwrap();
    assert!(
        read_backup(&file)
            .unwrap_err()
            .to_string()
            .contains("not a regular file")
    );
    let e = restore(&file, &dir.path().join("none")).unwrap_err();
    assert_eq!(e.code(), 1);
    let unreadable = write(dir.path(), "b.toml", "x");
    std::fs::write(beside(&unreadable, BACKUP_SUFFIX), [0xff, 0xfe]).unwrap();
    assert_eq!(read_backup(&unreadable).unwrap_err().code(), 2);
}

/// Behaviour: a file that cannot be given its owner is removed, not left half made. As
/// root every owner can be given, so the case cannot be made there and is skipped, saying so.
#[test]
fn a_file_that_cannot_be_given_its_owner_is_removed() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: as root every owner can be given");
        return;
    }
    let dir = scratch("create-fail");
    let path = dir.path().join("f");
    assert!(create(&path, b"x", 0o600, 0, 0).is_err());
    assert!(!path.exists());
}

/// Behaviour: a staging in a directory it cannot write fails, names the file, and leaves
/// nothing. As root every directory can be written, so the case is skipped there, saying so.
#[test]
fn a_staging_that_cannot_write_fails_and_leaves_nothing() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: as root every directory can be written");
        return;
    }
    let dir = scratch("stage-read-only");
    let file = write(dir.path(), "kbf.toml", "old\n");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let e = Staged::write(&file, "old\n", "new\n").unwrap_err();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(e.code(), 1);
    assert!(e.to_string().starts_with("cannot write "), "{e}");
    assert!(!beside(&file, BACKUP_SUFFIX).exists());
}
