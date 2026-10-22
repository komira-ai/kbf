use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use kbf_testkit::dir::TestDir;
use serde_json::Value;

use super::{Audit, Entry, MAX_RAW_CHARS, now, truncate_raw};
use crate::trust::Trust;

fn entry(outcome: &'static str) -> Entry {
    Entry {
        time: "2026-10-22T00:00:00Z".into(),
        verb: Some("install"),
        version: Some("1.0-1".into()),
        channel: Some("candidate".into()),
        outcome,
        reason: None,
        command: None,
    }
}

fn setup(name: &str) -> (TestDir, Trust) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let trust = Trust::owned_by(fs::metadata(&*dir).unwrap().uid(), &dir);
    (dir, trust)
}

/// The file is created 0600, each append is one line, a second open appends after the
/// first (nothing is truncated), and the fields are the ones the log promises.
#[test]
fn lines_are_appended_to_a_private_file() {
    let (dir, trust) = setup("audit-append");
    let path = dir.join("log");
    let mut audit = Audit::open(&trust, &path).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    audit.append(&entry("installed")).unwrap();
    drop(audit);
    let mut again = Audit::open(&trust, &path).unwrap();
    let mut refused = entry("refused");
    refused.reason = Some("bad_command");
    refused.verb = None;
    refused.version = None;
    refused.channel = None;
    refused.command = Some("rm -rf \"\n/\"".into());
    again.append(&refused).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text:?}");
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(
        first,
        serde_json::json!({
            "time": "2026-10-22T00:00:00Z", "verb": "install", "version": "1.0-1",
            "channel": "candidate", "outcome": "installed", "reason": null
        })
    );
    let second: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(
        second["command"], "rm -rf \"\n/\"",
        "escaped on the line, exact once parsed"
    );
    assert_eq!(second["reason"], "bad_command");
    assert_eq!(second["verb"], Value::Null);
}

/// Catches a log that follows a link, or is used though others can read or write it, or
/// though the trusted user does not own it.
#[test]
fn an_unsafe_log_is_refused() {
    let (dir, trust) = setup("audit-unsafe");
    let real = dir.join("real");
    fs::write(&real, "").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.join("link");
    symlink(&real, &link).unwrap();
    assert!(
        Audit::open(&trust, &link).is_err(),
        "a link is not followed"
    );
    for mode in [0o644, 0o660, 0o606, 0o640, 0o604, 0o666] {
        fs::set_permissions(&real, fs::Permissions::from_mode(mode)).unwrap();
        assert!(Audit::open(&trust, &real).is_err(), "mode {mode:o}");
    }
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(Audit::open(&trust, &real).is_ok());
    let stranger = Trust::owned_by(trust_uid(&dir) + 1, &dir);
    assert!(
        Audit::open(&stranger, &real).is_err(),
        "not the trusted user's"
    );
    assert!(
        Audit::open(&trust, &dir).is_err(),
        "a directory is not a log"
    );
    assert!(Audit::open(&trust, &dir.join("no-such-dir").join("log")).is_err());
}

fn trust_uid(dir: &TestDir) -> u32 {
    fs::metadata(&**dir).unwrap().uid()
}

/// The time is RFC 3339 in UTC; the raw command is cut by characters, not bytes.
#[test]
fn time_and_truncation() {
    let t = now();
    assert!(
        t.ends_with('Z') && t.len() >= 20 && t.as_bytes()[10] == b'T',
        "{t}"
    );
    assert!(t.starts_with("20"), "{t}");
    assert_eq!(truncate_raw("short"), "short");
    let long = "\u{e9}".repeat(MAX_RAW_CHARS + 50);
    assert_eq!(truncate_raw(&long).chars().count(), MAX_RAW_CHARS);
    let exact = "a".repeat(MAX_RAW_CHARS);
    assert_eq!(truncate_raw(&exact), exact);
}
