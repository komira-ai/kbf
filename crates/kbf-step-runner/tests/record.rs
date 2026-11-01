//! The step record: durable, versioned, and stopped at the points a crash can come.

mod support;

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kbf_step_runner::record::{HookSel, PARAMS_FILE, Progress, RECORD_FILE, SCHEMA, State};
use kbf_step_runner::{CrashPoint, Loaded, Record, RecordError, RecordStore, Stamp};
use support::{FakeClock, Scratch};
use update_hook::{Class, Phase, Step, Verdict};

fn record() -> Record {
    let clock = FakeClock::new();
    Record {
        schema: SCHEMA,
        service: "svc".to_owned(),
        target_version: "2.0.0".to_owned(),
        artifact_digest: "abc".to_owned(),
        tarball_digest: "0123".to_owned(),
        previous_digest: Some("def".to_owned()),
        generation: 7,
        params: "/state/params.toml".into(),
        step: Step::Verify,
        attempt: 2,
        state: State::Running {
            started: Stamp::now(&*clock),
            deadline_secs: 1600,
        },
        hook: HookSel::Previous,
        progress: Progress {
            committed: true,
            promotion_pending: true,
            then: Some(Verdict::Failed {
                phase: Phase::GateRelease,
                class: Class::Environment,
            }),
            retry: 2,
            verify_started: Some(Stamp::now(&*clock)),
            watching: None,
            reason: Some("remote_down".to_owned()),
        },
    }
}

/// Catches a record that loses a field on the way to disk and back, in any state.
#[test]
fn a_record_survives_the_disk() {
    let dir = Scratch::new("rec-roundtrip");
    let store = RecordStore::new(dir.path());
    assert_eq!(store.load().unwrap(), Loaded::Absent);
    let mut rec = record();
    for state in [
        rec.state.clone(),
        State::Pending,
        State::Done {
            code: Some(75),
            last_line: Some("{}".to_owned()),
        },
        State::Finished {
            verdict: Verdict::Succeeded {
                promotion_pending: false,
            },
            reason: Some("ok".to_owned()),
        },
    ] {
        rec.state = state;
        store.save(&rec).unwrap();
        assert_eq!(
            store.load().unwrap(),
            Loaded::Current(Box::new(rec.clone()))
        );
    }
    assert!(!dir.path().join("step-record.json.tmp").exists());
}

/// Catches a unit name that is not a function of service, step and generation: resume stops
/// the old unit by this name.
#[test]
fn the_unit_name_is_deterministic() {
    assert_eq!(record().unit(Step::Cutover), "updater-svc-cutover-7");
    assert_eq!(record().unit(Step::Rollback), "updater-svc-rollback-7");
}

/// Catches a record written by a newer binary being read as ours (and then rewritten): it is
/// reported as newer, with its schema, and the file is untouched.
#[test]
fn a_newer_schema_is_reported_and_left_alone() {
    let dir = Scratch::new("rec-newer");
    let store = RecordStore::new(dir.path());
    let text = r#"{"schema": 99, "something": "new"}"#;
    fs::write(dir.path().join(RECORD_FILE), text).unwrap();
    assert_eq!(store.load().unwrap(), Loaded::Newer { schema: 99 });
    assert_eq!(
        fs::read_to_string(dir.path().join(RECORD_FILE)).unwrap(),
        text
    );
}

/// Catches a damaged record read as an empty one (which would start an update over a half-done
/// one): garbage and a record with a missing field are `Corrupt`; an unreadable file is I/O.
#[test]
fn a_damaged_record_is_an_error() {
    let dir = Scratch::new("rec-corrupt");
    let store = RecordStore::new(dir.path());
    for text in ["not json", r#"{"schema": 1}"#, r#"{"service": "x"}"#] {
        fs::write(dir.path().join(RECORD_FILE), text).unwrap();
        let err = store.load().unwrap_err();
        assert!(matches!(err, RecordError::Corrupt(_)), "{text}: {err:?}");
        assert!(err.to_string().contains("not readable"));
    }
    fs::remove_file(dir.path().join(RECORD_FILE)).unwrap();
    fs::create_dir(dir.path().join(RECORD_FILE)).unwrap();
    let err = store.load().unwrap_err();
    assert!(matches!(err, RecordError::Io(_)));
    assert!(err.to_string().contains("state directory"));
}

/// Catches a write that reports success while the state directory is gone, for the record and
/// for the params file.
#[test]
fn a_write_to_a_missing_directory_fails() {
    let dir = Scratch::new("rec-gone");
    let store = RecordStore::new(&dir.path().join("absent"));
    assert!(matches!(store.save(&record()), Err(RecordError::Io(_))));
    assert!(matches!(
        store.save_params("a = 1\n"),
        Err(RecordError::Io(_))
    ));
}

/// Catches a gate that does not stop where it says: writes are numbered from 1, the gate sees
/// the point before and after each, and `true` makes that write fail as a crash would (before
/// the write it leaves the old file, after it the new).
#[test]
fn the_crash_gate_stops_before_and_after_a_write() {
    let dir = Scratch::new("rec-gate");
    let seen = Arc::new(AtomicU64::new(0));
    let counter = seen.clone();
    let store = RecordStore::new(dir.path()).with_crash_gate(Arc::new(move |p| {
        counter.fetch_add(1, Ordering::SeqCst);
        p == CrashPoint::BeforeWrite(2) || p == CrashPoint::AfterWrite(3)
    }));
    let mut rec = record();
    store.save(&rec).unwrap();
    rec.attempt = 9;
    let err = store.save(&rec).unwrap_err();
    assert!(matches!(
        err,
        RecordError::Crashed(CrashPoint::BeforeWrite(2))
    ));
    assert!(err.to_string().contains("BeforeWrite(2)"));
    let Loaded::Current(on_disk) = store.load().unwrap() else {
        panic!("a record")
    };
    assert_eq!(on_disk.attempt, 2);
    let err = store.save(&rec).unwrap_err();
    assert!(matches!(
        err,
        RecordError::Crashed(CrashPoint::AfterWrite(3))
    ));
    let Loaded::Current(on_disk) = store.load().unwrap() else {
        panic!("a record")
    };
    assert_eq!(on_disk.attempt, 9);
    assert_eq!(seen.load(Ordering::SeqCst), 5);
}

/// Catches a params file written anywhere but beside the record, or not whole.
#[test]
fn the_params_file_is_written_where_the_store_says() {
    let dir = Scratch::new("rec-params");
    let store = RecordStore::new(dir.path());
    store.save_params("a = 1\n").unwrap();
    assert_eq!(store.params_path(), dir.path().join(PARAMS_FILE));
    assert_eq!(fs::read_to_string(store.params_path()).unwrap(), "a = 1\n");
}

/// Catches a params file left behind or a removal that hides an error: removing one that is
/// there removes it, one that is not there is fine, and one that cannot be removed is an error.
#[test]
fn removing_the_params_file_reports_only_real_errors() {
    let dir = Scratch::new("rec-rm-params");
    let store = RecordStore::new(dir.path());
    store.save_params("a = 1\n").unwrap();
    store.remove_params().unwrap();
    assert!(!store.params_path().exists());
    store.remove_params().unwrap();
    fs::create_dir(store.params_path()).unwrap();
    assert!(matches!(store.remove_params(), Err(RecordError::Io(_))));
}

/// Catches a private-directory check that passes a file, a link or a directory others can enter.
#[test]
fn only_a_closed_directory_of_the_user_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Scratch::new("rec-private");
    let closed = dir.path().join("closed");
    fs::create_dir(&closed).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    RecordStore::new(&closed).check_private().unwrap();
    let file = dir.path().join("file");
    fs::write(&file, "").unwrap();
    assert!(RecordStore::new(&file).check_private().is_err());
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&closed, &link).unwrap();
    assert!(RecordStore::new(&link).check_private().is_err());
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o770)).unwrap();
    let err = RecordStore::new(&closed).check_private().unwrap_err();
    assert!(err.to_string().contains("private directory"));
    assert!(
        RecordStore::new(&dir.path().join("absent"))
            .check_private()
            .is_err()
    );
}

/// Catches a change to the persisted schema: the record, its states and the verdicts are files on
/// disk that an older or newer updater reads, so their JSON is pinned as written here, and read
/// back to the same values.
#[test]
fn the_record_on_disk_has_this_shape() {
    let stamp = || Stamp {
        boot_id: "b".to_owned(),
        mono_secs: 7,
    };
    let verdicts = [
        (
            Verdict::Succeeded {
                promotion_pending: true,
            },
            r#"{"Succeeded":{"promotion_pending":true}}"#,
        ),
        (
            Verdict::Failed {
                phase: Phase::AfterCommit,
                class: Class::Environment,
            },
            r#"{"Failed":{"phase":"after_commit","class":"environment"}}"#,
        ),
        (
            Verdict::Deferred {
                retry_after_secs: Some(5),
            },
            r#"{"Deferred":{"retry_after_secs":5}}"#,
        ),
        (
            Verdict::Deferred {
                retry_after_secs: None,
            },
            r#"{"Deferred":{"retry_after_secs":null}}"#,
        ),
        (Verdict::Halt, r#""Halt""#),
    ];
    for (verdict, json) in verdicts {
        assert_eq!(serde_json::to_string(&verdict).unwrap(), json);
        assert_eq!(serde_json::from_str::<Verdict>(json).unwrap(), verdict);
    }
    let states = [
        (State::Pending, r#"{"state":"pending"}"#),
        (
            State::Running {
                started: stamp(),
                deadline_secs: 9,
            },
            r#"{"state":"running","started":{"boot_id":"b","mono_secs":7},"deadline_secs":9}"#,
        ),
        (
            State::Done {
                code: Some(75),
                last_line: Some("{}".to_owned()),
            },
            r#"{"state":"done","code":75,"last_line":"{}"}"#,
        ),
        (
            State::Finished {
                verdict: Verdict::Halt,
                reason: Some("x".to_owned()),
            },
            r#"{"state":"finished","verdict":"Halt","reason":"x"}"#,
        ),
    ];
    for (state, json) in states {
        assert_eq!(serde_json::to_string(&state).unwrap(), json);
        assert_eq!(serde_json::from_str::<State>(json).unwrap(), state);
    }
    let rec = Record {
        schema: SCHEMA,
        service: "svc".to_owned(),
        target_version: "2.0.0".to_owned(),
        artifact_digest: "abc".to_owned(),
        tarball_digest: "0123".to_owned(),
        previous_digest: Some("def".to_owned()),
        generation: 7,
        params: "/state/params.toml".into(),
        step: Step::Verify,
        attempt: 2,
        state: State::Pending,
        hook: HookSel::Previous,
        progress: Progress {
            committed: true,
            promotion_pending: false,
            then: Some(Verdict::Deferred {
                retry_after_secs: None,
            }),
            retry: 2,
            verify_started: Some(stamp()),
            watching: None,
            reason: Some("remote_down".to_owned()),
        },
    };
    let json = r#"{"schema":1,"service":"svc","target_version":"2.0.0","artifact_digest":"abc","tarball_digest":"0123","previous_digest":"def","generation":7,"params":"/state/params.toml","step":"verify","attempt":2,"state":{"state":"pending"},"hook":"previous","progress":{"committed":true,"promotion_pending":false,"then":{"Deferred":{"retry_after_secs":null}},"retry":2,"verify_started":{"boot_id":"b","mono_secs":7},"watching":null,"reason":"remote_down"}}"#;
    assert_eq!(serde_json::to_string(&rec).unwrap(), json);
    assert_eq!(serde_json::from_str::<Record>(json).unwrap(), rec);
}

/// Catches a renamed variant of what the record holds besides its states and verdicts (those are
/// pinned with the whole record above): every step, every hook choice, every phase and class of
/// a failure is written under this name and read back from it, so that a rename (a lost
/// `rename_all`, a new spelling) fails here and not in the updater of the next release.
#[test]
fn every_variant_name_in_the_record_is_pinned() {
    fn pinned<T>(pairs: &[(T, &str)])
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        for (value, name) in pairs {
            let written = serde_json::to_value(value).unwrap();
            assert_eq!(written, *name, "{value:?}");
            let read: T = serde_json::from_value(serde_json::Value::from(*name)).unwrap();
            assert_eq!(&read, value, "{name}");
        }
    }
    pinned(&[
        (Step::Prepare, "prepare"),
        (Step::Cutover, "cutover"),
        (Step::Verify, "verify"),
        (Step::Rollback, "rollback"),
        (Step::Cleanup, "cleanup"),
    ]);
    pinned(&[(HookSel::New, "new"), (HookSel::Previous, "previous")]);
    pinned(&[
        (Phase::BeforeCommit, "before_commit"),
        (Phase::GateRelease, "gate_release"),
        (Phase::GateEnvironment, "gate_environment"),
        (Phase::AfterCommit, "after_commit"),
        (Phase::Halt, "halt"),
    ]);
    pinned(&[
        (Class::Release, "release"),
        (Class::Environment, "environment"),
    ]);
}
