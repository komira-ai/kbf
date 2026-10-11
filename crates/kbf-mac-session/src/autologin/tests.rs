use std::os::unix::fs::PermissionsExt as _;

use super::*;
use crate::testing::{FakeHost, scratch};

fn decode(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .zip(KC_KEY.iter().cycle())
        .map(|(byte, key)| byte ^ key)
        .collect()
}

/// Catches: a `/etc/kcpassword` the login window cannot read back: the key not
/// repeated past its 11 bytes, no NUL after the password, or a length that is not a
/// whole number of 12-byte blocks (a password of exactly 12 bytes needs a second block,
/// or it has no terminator).
#[test]
fn kcpassword_is_the_password_and_a_nul_xored_in_whole_blocks() {
    assert_eq!(
        kcpassword(b"a"),
        [
            b'a' ^ 0x7D,
            0x89,
            0x52,
            0x23,
            0xD2,
            0xBC,
            0xDD,
            0xEA,
            0xA3,
            0xB9,
            0x1F,
            0x7D
        ]
    );
    for len in [0, 1, 11, 12, 13, 23, 24, 64] {
        let password: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
        let encoded = kcpassword(&password);
        assert_eq!(encoded.len() % KC_BLOCK, 0, "length {len}");
        assert!(encoded.len() > len, "length {len}: no room for the NUL");
        assert!(
            encoded.len() <= len + KC_BLOCK,
            "length {len}: a block too many"
        );
        let plain = decode(&encoded);
        assert_eq!(&plain[..len], &password[..], "length {len}");
        assert!(plain[len..].iter().all(|&b| b == 0), "length {len}");
    }
    assert_eq!(kcpassword(&[b'x'; 12]).len(), 24);
}

/// Catches: an intent record that is read through a link, taken although it is torn
/// or holds something else, or left readable by others.
#[test]
fn the_intent_record_names_one_lease_and_is_private() {
    let dir = scratch("intent");
    let path = dir.join("session-intent");
    assert_eq!(read_intent(&path), Ok(None));
    write_intent(&path, LeaseId::new(3, 7)).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "3.7\n");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(read_intent(&path), Ok(Some(LeaseId::new(3, 7))));
    // Written again over the old record, whole.
    write_intent(&path, LeaseId::new(3, 8)).unwrap();
    assert_eq!(read_intent(&path), Ok(Some(LeaseId::new(3, 8))));
    for bad in ["3.7", "3.7\n4.1\n", "03.7\n", "\n", "kbf-lease-3-7\n"] {
        std::fs::write(&path, bad).unwrap();
        assert!(read_intent(&path).is_err(), "{bad:?} was taken");
    }
    assert!(remove(&path).unwrap());
    assert!(!remove(&path).unwrap());
    std::fs::write(dir.join("elsewhere"), "3.7\n").unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere"), &path).unwrap();
    assert!(read_intent(&path).is_err(), "read through a link");
}

/// Catches: `/etc/kcpassword` written through a link planted at its path (root would
/// write the password where the link points), left readable by others, or a stale
/// temporary file that stops the write.
#[test]
fn kcpassword_replaces_a_planted_link_and_is_private() {
    let dir = scratch("kcpassword");
    let path = dir.join("kcpassword");
    let target = dir.join("target");
    std::fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    std::fs::write(dir.join("kcpassword.new"), "stale").unwrap();
    write_kcpassword(&path, b"secret").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    let meta = std::fs::symlink_metadata(&path).unwrap();
    assert!(meta.file_type().is_file());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(&path).unwrap(), kcpassword(b"secret"));
    assert!(!dir.join("kcpassword.new").exists());
}

fn answer(host: &FakeHost, text: &str, success: bool, stdout: &str) {
    host.state().answers.insert(
        text.to_owned(),
        CommandOutput {
            success,
            stdout: stdout.to_owned(),
            stderr: "said no".to_owned(),
        },
    );
}

/// Catches: a failed `defaults delete` taken as cleared while the key is still set, or
/// a missing key (which `defaults delete` reports as a failure) taken as an error.
#[test]
fn clearing_auto_login_checks_a_failed_delete_with_a_read() {
    let dir = scratch("clear");
    let paths = SessionPaths::under(&dir);
    let domain = paths.loginwindow.clone();
    let host = FakeHost::default();
    std::fs::write(&paths.kcpassword, "x").unwrap();
    assert_eq!(clear_auto_login(&host, &paths), Ok(true));
    assert!(!paths.kcpassword.exists());
    // Not set, nothing left: a failed delete, and a read that finds nothing.
    answer(
        &host,
        &format!("{DEFAULTS} delete {domain} autoLoginUser"),
        false,
        "",
    );
    answer(
        &host,
        &format!("{DEFAULTS} read {domain} autoLoginUser"),
        false,
        "",
    );
    assert_eq!(clear_auto_login(&host, &paths), Ok(false));
    // Still set after a failed delete.
    answer(
        &host,
        &format!("{DEFAULTS} read {domain} autoLoginUser"),
        true,
        "kbf-lease-1-1\n",
    );
    let error = clear_auto_login(&host, &paths).unwrap_err();
    assert!(error.contains("said no"), "{error}");
    host.state().fail.insert("command");
    let error = clear_auto_login(&host, &paths).unwrap_err();
    assert!(error.contains("running /usr/bin/defaults"), "{error}");
}

/// Catches: a session switch on a Mac with FileVault on (it would restart to the
/// unlock screen and never log in), or an unreadable answer taken as off.
#[test]
fn filevault_must_say_it_is_off() {
    let host = FakeHost::default();
    assert_eq!(require_filevault_off(&host), Ok(()));
    answer(&host, "/usr/bin/fdesetup isactive", true, "true\n");
    let error = require_filevault_off(&host).unwrap_err();
    assert!(error.contains("FileVault is on"), "{error}");
    answer(&host, "/usr/bin/fdesetup isactive", false, "");
    let error = require_filevault_off(&host).unwrap_err();
    assert!(error.contains("not true or false"), "{error}");
}

/// Catches: a failed `defaults write` or restart reported as done.
#[test]
fn a_failed_write_or_restart_is_an_error() {
    let dir = scratch("write-fail");
    let paths = SessionPaths::under(&dir);
    let host = FakeHost::default();
    assert_eq!(set_auto_login(&host, &paths, "kbf-lease-1-1"), Ok(()));
    assert_eq!(restart_userspace(&host), Ok(()));
    let write = format!(
        "{DEFAULTS} write {} autoLoginUser -string kbf-lease-1-1",
        paths.loginwindow
    );
    answer(&host, &write, false, "");
    answer(&host, "/bin/launchctl reboot userspace", false, "");
    assert!(set_auto_login(&host, &paths, "kbf-lease-1-1").is_err());
    let error = restart_userspace(&host).unwrap_err();
    assert!(
        error.contains("launchctl reboot userspace failed: said no"),
        "{error}"
    );
}

/// The real runner: output, exit status, and an empty environment (no `HOME` or
/// `PATH` from the helper reaches `defaults` or `launchctl`).
#[test]
fn run_command_reports_output_and_clears_the_environment() {
    let output = run_command(
        "/bin/sh",
        &["-c", "echo out; echo err >&2; echo ${HOME-unset}; exit 3"],
    )
    .unwrap();
    assert_eq!(
        output,
        CommandOutput {
            success: false,
            stdout: "out\nunset\n".to_owned(),
            stderr: "err\n".to_owned(),
        }
    );
    assert!(run_command("/bin/sh", &["-c", "true"]).unwrap().success);
    assert!(run_command("/nonexistent/program", &[]).is_err());
}

/// Catches: a failure to read, write or remove one of the switch's files taken as
/// success (each path here is a directory, which no file operation may treat as the
/// record), and a relative path's directory not synced.
#[test]
fn file_errors_are_reported() {
    let dir = scratch("file-errors");
    let taken = dir.join("taken");
    std::fs::create_dir_all(taken.join("inside")).unwrap();
    assert!(read_intent(&taken).is_err(), "a directory read as a record");
    assert!(remove(&taken).is_err(), "a directory removed as a file");
    std::fs::create_dir_all(dir.join("record.new/inside")).unwrap();
    assert!(write_intent(&dir.join("record"), LeaseId::new(1, 1)).is_err());
    let paths = SessionPaths {
        kcpassword: taken.clone(),
        ..SessionPaths::under(&dir)
    };
    let error = clear_auto_login(&FakeHost::default(), &paths).unwrap_err();
    assert!(error.contains("removing"), "{error}");
    // A bare file name: its directory is the current one.
    assert!(!remove(Path::new("kbf-mac-session-no-such-file")).unwrap());
    assert!(sync_parent(Path::new("kbf-mac-session-no-such-file")).is_ok());
}
