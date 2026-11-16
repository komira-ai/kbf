//! `--history-credentials-file`: where an `s3://` history store's key pair comes from.

use std::cell::Cell;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kbf_state::ManualClock;
use kbf_state::fake::{Fake, Flaw};

use super::{HistoryError, HistoryFlags};
use crate::StateStore;
use crate::farm::EpochPolicy;
use crate::stores::StoreUri;
use crate::test_dir::scratch_dir;

const SECRET: &str = "history-file-secret-5d20";

fn state() -> StateStore {
    StateStore {
        store: Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        )),
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }
}

fn s3_flags(file: Option<PathBuf>) -> HistoryFlags {
    HistoryFlags {
        history_objects: Some(
            "s3://bucket/farm?endpoint=http://127.0.0.1:1"
                .parse::<StoreUri>()
                .expect("an s3 uri"),
        ),
        history_credentials_file: file,
        history_index_root: None,
    }
}

fn key_file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode set");
    path
}

fn pair() -> String {
    format!(r#"{{"access_key_id": "HISTKEYID", "secret_access_key": "{SECRET}"}}"#)
}

/// A key pair in the file opens the store with no variable in the environment: the file is
/// read, and the environment is not asked. Catches a flag that is parsed and then ignored.
#[test]
fn the_key_pair_is_read_from_the_file() {
    let dir = scratch_dir("history-key-file-read");
    let path = key_file(dir.path(), "keys.json", &pair(), 0o600);
    let asked = Cell::new(false);
    let env = |_: &str| {
        asked.set(true);
        None
    };
    let opened = s3_flags(Some(path))
        .open_with(Some(&state()), &env)
        .expect("opens");
    assert!(opened.is_some());
    assert!(!asked.get(), "the environment was read");
}

/// A file others can read is refused at start with the flag, the path and the mode, and
/// never a byte of the file. Catches a key file accepted at 0644.
#[test]
fn a_key_file_others_can_read_is_refused_with_its_mode_named() {
    let dir = scratch_dir("history-key-file-mode");
    let path = key_file(dir.path(), "keys.json", &pair(), 0o644);
    let error = s3_flags(Some(path.clone()))
        .open_with(Some(&state()), &|_| None)
        .map(|_| ())
        .expect_err("refused");
    let said = error.to_string();
    assert_eq!(
        said,
        format!(
            "--history-credentials-file {}: mode 0644: it must be 0600 or 0400",
            path.display()
        )
    );
    assert!(!said.contains(SECRET), "{said}");
    assert!(!format!("{error:?}").contains(SECRET));
}

/// With a file and both variables set, the file's pair is the one used: a file that is
/// refused is an error, and the environment is not a way round it. Catches a flag that
/// loses to the environment, or falls back to it when the file is bad.
#[test]
fn the_file_beats_the_environment_and_does_not_fall_back_to_it() {
    let dir = scratch_dir("history-key-file-beats-env");
    let env = |name: &str| Some(format!("{name}-value"));
    let good = key_file(dir.path(), "good.json", &pair(), 0o400);
    let asked = Cell::new(false);
    let watched = |name: &str| {
        asked.set(true);
        env(name)
    };
    assert!(
        s3_flags(Some(good))
            .open_with(Some(&state()), &watched)
            .expect("opens")
            .is_some()
    );
    assert!(!asked.get(), "the environment was read");
    let bad = key_file(dir.path(), "bad.json", "not json", 0o600);
    let refused = s3_flags(Some(bad))
        .open_with(Some(&state()), &env)
        .map(|_| ())
        .expect_err("refused");
    assert!(matches!(refused, HistoryError::KeyFile { .. }), "{refused}");
}

/// Without the flag the environment's pair still opens the store, and a missing variable
/// is still named. Catches the additive flag changing the path that existed.
#[test]
fn without_the_flag_the_environment_still_gives_the_pair() {
    let env = |name: &str| Some(format!("{name}-value"));
    let opened = s3_flags(None)
        .open_with(Some(&state()), &env)
        .expect("opens");
    assert!(opened.is_some());
    let refused = s3_flags(None)
        .open_with(Some(&state()), &|_| None)
        .map(|_| ())
        .expect_err("no key");
    assert!(
        refused.to_string().contains("AWS_ACCESS_KEY_ID"),
        "{refused}"
    );
}

/// A directory has no keys: the flag with a `file://` store is refused rather than
/// ignored.
#[test]
fn a_key_file_with_a_file_store_is_refused() {
    let dir = scratch_dir("history-key-file-on-dir");
    let path = key_file(dir.path(), "keys.json", &pair(), 0o600);
    let flags = HistoryFlags {
        history_objects: Some(StoreUri::File(dir.path().join("objects"))),
        history_credentials_file: Some(path),
        history_index_root: None,
    };
    let error = flags
        .open_with(Some(&state()), &|_| None)
        .map(|_| ())
        .expect_err("refused");
    assert!(matches!(error, HistoryError::KeyFileOnFileStore), "{error}");
}
