//! The ops store: its file, its reader, and the order a change reaches memory and the file.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex, OnceLock};

use kbf_testkit::dir::TestDir;
use kbf_updater::wire::{Outcome, Phase, Token, UpdateResult};

use crate::desired::StoreError;
use crate::ops::{
    AUDIT_RING, AuditEntry, Exclusion, Halt, HaltAck, HostOps, OpsError, OpsState, OpsStore,
    ServiceOps, Write,
};
use crate::testutil::version;

fn dir(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

fn entry(action: &str) -> AuditEntry {
    AuditEntry::new("operator", action)
}

/// The `last_update` of a halt of `target`.
fn halt_report(target: &str) -> UpdateResult {
    let mut report = UpdateResult::new(version(target), Outcome::Failed);
    report.phase = Some(Phase::Halt);
    report.reason = Token::parse("tainted");
    report
}

fn exclude(state: &mut OpsState, node: &str) {
    state.excluded.push(Exclusion {
        service: "kbf".to_owned(),
        node_id: node.to_owned(),
        reason: "disk replaced".to_owned(),
    });
}

/// A file holding one of everything the schema has.
const FULL: &str = r#"{
  "schema": 1,
  "hosts": {
    "n1": {
      "labels": {
        "zone": "a"
      }
    }
  },
  "excluded": [
    {
      "service": "kbf",
      "node_id": "n2",
      "reason": "disk replaced"
    }
  ],
  "services": {
    "kbf": {
      "halt": {
        "node_id": "n3",
        "check": "tainted",
        "target": "2.0",
        "report": {
          "target": "2.0",
          "outcome": "failed",
          "phase": "halt",
          "reason": "tainted"
        }
      },
      "halt_acked": [
        {
          "node_id": "n4",
          "target": "1.5",
          "report": {
            "target": "1.5",
            "outcome": "failed",
            "phase": "halt",
            "reason": "tainted"
          }
        }
      ],
      "halt_seen": [
        {
          "node_id": "n5",
          "target": "2.0",
          "report": {
            "target": "2.0",
            "outcome": "failed",
            "phase": "halt",
            "reason": "tainted"
          }
        }
      ],
      "halt_generation": 3,
      "last_good": "1.0",
      "bad": [
        "1.9"
      ]
    }
  },
  "audit": [
    {
      "seq": 7,
      "by": "operator",
      "action": "exclude",
      "service": "kbf",
      "node_id": "n2",
      "reason": "disk replaced"
    }
  ]
}
"#;

/// Pins: the schema of the file. Every field of every table reads back and writes out as
/// shown here (the text a later change reads is this one), and the state the reader makes is
/// the state a test builds by hand.
#[test]
fn the_file_reads_and_writes_every_field_of_the_schema() {
    let dir = dir("ops-full");
    let file = dir.path().join("ops.json");
    fs::write(&file, FULL).expect("written");
    let store = OpsStore::open(&file).expect("read");
    let mut want = OpsState::default();
    want.hosts.insert(
        "n1".to_owned(),
        HostOps {
            labels: [("zone".to_owned(), "a".to_owned())].into(),
        },
    );
    exclude(&mut want, "n2");
    want.services.insert(
        "kbf".to_owned(),
        ServiceOps {
            halt: Some(Halt {
                node_id: "n3".to_owned(),
                check: "tainted".to_owned(),
                target: version("2.0"),
                report: halt_report("2.0"),
            }),
            halt_acked: vec![HaltAck {
                node_id: "n4".to_owned(),
                target: version("1.5"),
                report: halt_report("1.5"),
            }],
            halt_seen: vec![HaltAck {
                node_id: "n5".to_owned(),
                target: version("2.0"),
                report: halt_report("2.0"),
            }],
            halt_generation: 3,
            last_good: Some(version("1.0")),
            bad: vec![version("1.9")],
        },
    );
    let mut seven = entry("exclude");
    seven.seq = 7;
    seven.service = Some("kbf".to_owned());
    seven.node_id = Some("n2".to_owned());
    seven.reason = Some("disk replaced".to_owned());
    want.audit.push(seven);
    assert_eq!(store.state(), want);

    // The next change writes the whole file back: every field survives, the ring continues.
    store
        .change(Write::BeforeMemory, entry("include"), |_| {})
        .expect("changed");
    let text = fs::read_to_string(&file).expect("read");
    let wrote: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let first: serde_json::Value = serde_json::from_str(FULL).expect("JSON");
    for key in ["schema", "hosts", "excluded", "services"] {
        assert_eq!(wrote[key], first[key], "{key}");
    }
    assert_eq!(wrote["audit"][1]["seq"], 8);
    assert!(text.ends_with("}\n"), "{text:?}");
    assert_eq!(
        OpsStore::open(&file).expect("reread").state(),
        store.state()
    );
}

/// Pins: an empty state writes the five keys a reader of the schema looks for, and the
/// marks of a service with nothing set are explicit nulls and empty lists.
#[test]
fn an_empty_state_writes_the_schema_with_every_table() {
    let dir = dir("ops-empty-text");
    let file = dir.path().join("ops.json");
    let store = OpsStore::open(&file).expect("opened");
    store
        .change(Write::BeforeMemory, entry("noop"), |state| {
            state
                .services
                .insert("kbf".to_owned(), ServiceOps::default());
        })
        .expect("changed");
    let wrote: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&file).expect("read")).expect("JSON");
    assert_eq!(
        wrote["services"],
        serde_json::json!({"kbf": {"halt": null, "halt_acked": [], "last_good": null, "bad": []}})
    );
    assert_eq!(wrote["hosts"], serde_json::json!({}));
    assert_eq!(wrote["excluded"], serde_json::json!([]));
    assert_eq!(wrote["schema"], 1);
}

/// Catches: a missing file taken for an error (an upgraded farm would not start), and an
/// open that creates the file (an upgraded farm's files would change before any feature is
/// used). A missing file is the empty state, and none is created until the first change.
#[test]
fn a_missing_file_is_the_empty_state_and_opening_creates_nothing() {
    let dir = dir("ops-missing");
    let file = dir.path().join("ops.json");
    let store = OpsStore::open(&file).expect("a missing file is no state");
    assert_eq!(store.state(), OpsState::default());
    assert!(store.state().is_empty());
    assert!(!file.exists());
    assert!(!dir.path().join("ops.json.new").exists());
    store
        .change(Write::BeforeMemory, entry("exclude"), |s| exclude(s, "n1"))
        .expect("changed");
    assert!(file.exists());
    assert!(!store.state().is_empty());
}

/// Catches: a reader that skips what it does not know: a key unknown to this build, at the
/// top, in a host, an exclusion, a service, a halt or an audit entry, refuses the start, and
/// the error names the file. (A coordinator that skipped it could ignore a halt.)
#[test]
fn an_unknown_key_anywhere_refuses_to_start() {
    let dir = dir("ops-unknown");
    let file = dir.path().join("ops.json");
    let places = [
        r#"{"schema":1,"future":1}"#,
        r#"{"schema":1,"hosts":{"n1":{"labels":{},"cordon":true}}}"#,
        r#"{"schema":1,"excluded":[{"service":"kbf","node_id":"n","reason":"r","until":1}]}"#,
        r#"{"schema":1,"services":{"kbf":{"urgent":true}}}"#,
        r#"{"schema":1,"services":{"kbf":{"halt":{"node_id":"n","check":"c","target":"1.0","at":1}}}}"#,
        r#"{"schema":1,"audit":[{"seq":1,"by":"b","action":"a","extra":1}]}"#,
    ];
    for text in places {
        fs::write(&file, text).expect("written");
        let why = OpsStore::open(&file).err().expect("refused");
        assert!(matches!(why, OpsError::Invalid { .. }), "{text}: {why}");
        assert!(why.to_string().contains("ops.json"), "{why}");
    }
    for text in ["", "[]", "{", r#"{"schema":"one"}"#, r#"{"hosts":{}}"#] {
        fs::write(&file, text).expect("written");
        let why = OpsStore::open(&file).err().expect("refused");
        assert!(matches!(why, OpsError::Invalid { .. }), "{text}: {why}");
    }
}

/// Catches: a coordinator rolled back below the build that wrote the file starting anyway
/// and ignoring what it holds: a schema newer than this build's is refused, the text names
/// the path and says to move the file aside, and the file is left as it was. A schema of 0
/// is refused too, and a missing one.
#[test]
fn a_newer_schema_refuses_to_start_and_leaves_the_file() {
    let dir = dir("ops-newer");
    let file = dir.path().join("ops.json");
    let text = r#"{"schema":2,"services":{"kbf":{"halt":{"node_id":"n","check":"c","target":"1.0"},"later":1}}}"#;
    fs::write(&file, text).expect("written");
    let why = OpsStore::open(&file).err().expect("refused");
    assert!(matches!(why, OpsError::Newer { found: 2, .. }), "{why}");
    let said = why.to_string();
    assert!(
        said.contains("ops.json") && said.contains("move it aside"),
        "{said}"
    );
    assert_eq!(fs::read_to_string(&file).expect("read"), text);
    fs::write(&file, r#"{"schema":0}"#).expect("written");
    let why = OpsStore::open(&file).err().expect("refused");
    assert!(matches!(why, OpsError::Invalid { .. }), "{why}");
    assert!(why.to_string().contains("schema 0"), "{why}");
}

/// Catches: a path that cannot be read taken for no state (a directory where the file
/// should be): the start is refused with an I/O error naming it.
#[test]
fn a_path_that_cannot_be_read_is_an_io_error() {
    let dir = dir("ops-unreadable");
    let why = OpsStore::open(dir.path()).err().expect("refused");
    assert!(matches!(why, OpsError::Io(StoreError::Io { .. })), "{why}");
}

/// Catches: the file being written in place, so that a crash mid-write leaves a torn store,
/// or a leftover of such a crash being read: a half-written temporary file beside the store
/// is never read (the store is the file, or empty); the next change replaces the leftover
/// and leaves the whole file with no temporary beside it.
#[test]
fn a_partial_temporary_file_is_never_read_as_the_store() {
    let dir = dir("ops-partial");
    let file = dir.path().join("ops.json");
    let temp = dir.path().join("ops.json.new");
    fs::write(&temp, &FULL[..FULL.len() / 2]).expect("written");
    let store = OpsStore::open(&file).expect("the leftover is not the store");
    assert!(store.state().is_empty());
    store
        .change(Write::BeforeMemory, entry("exclude"), |s| exclude(s, "n1"))
        .expect("changed");
    assert!(!temp.exists(), "the leftover is replaced and renamed away");
    let again = OpsStore::open(&file).expect("whole");
    assert_eq!(again.state(), store.state());
    // And with a good file in place, a torn temporary beside it changes nothing read.
    fs::write(&temp, "{\"schema\":1,\"excl").expect("written");
    assert_eq!(
        OpsStore::open(&file).expect("opened").state(),
        store.state()
    );
}

/// Catches: a file created with the process umask or through a link left at the temporary
/// name: the file is owner-only, and a symlink at the temporary name is replaced, not
/// written through.
#[test]
fn the_file_is_owner_only_and_a_link_at_the_temporary_name_is_not_followed() {
    use std::os::unix::fs::symlink;
    let dir = dir("ops-mode");
    let file = dir.path().join("ops.json");
    let victim = dir.path().join("victim");
    fs::write(&victim, "precious").expect("written");
    symlink(&victim, dir.path().join("ops.json.new")).expect("linked");
    let store = OpsStore::open(&file).expect("opened");
    store
        .change(Write::BeforeMemory, entry("exclude"), |s| exclude(s, "n1"))
        .expect("changed");
    assert_eq!(fs::read_to_string(&victim).expect("read"), "precious");
    let mode = fs::metadata(&file).expect("stat").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// Catches: a failed replacement damaging the old file or the memory: with a directory at
/// the temporary name the change fails with an I/O error, the old file is byte for byte as
/// it was, and the state in memory is the old state.
#[test]
fn a_write_that_cannot_happen_leaves_the_old_file_and_state() {
    let dir = dir("ops-blocked");
    let file = dir.path().join("ops.json");
    let store = OpsStore::open(&file).expect("opened");
    store
        .change(Write::BeforeMemory, entry("exclude"), |s| exclude(s, "n1"))
        .expect("changed");
    let before = fs::read(&file).expect("read");
    let state = store.state();
    fs::create_dir(dir.path().join("ops.json.new")).expect("made");
    let why = store
        .change(Write::BeforeMemory, entry("exclude"), |s| exclude(s, "n2"))
        .expect_err("fails");
    assert!(matches!(why, OpsError::Io(StoreError::Io { .. })), "{why}");
    assert_eq!(fs::read(&file).expect("read"), before);
    assert_eq!(store.state(), state);
}

/// A store whose writer notes how many exclusions memory held at each write, then succeeds
/// or fails as told. The writer reads the store through a cell filled once the store exists.
struct Spy {
    store: Arc<OpsStore>,
    /// Per write: (exclusions in memory when it was called, exclusions it was asked to write).
    seen: Arc<Mutex<Vec<(usize, usize)>>>,
}

fn spy(fails: bool) -> Spy {
    let cell: Arc<OnceLock<Arc<OpsStore>>> = Arc::new(OnceLock::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (reads, notes) = (Arc::clone(&cell), Arc::clone(&seen));
    let writer = Box::new(move |state: &OpsState| {
        let held = reads
            .get()
            .expect("the store exists")
            .state()
            .excluded
            .len();
        notes
            .lock()
            .expect("lock")
            .push((held, state.excluded.len()));
        if fails {
            Err(OpsError::Io(StoreError::Io {
                path: "ops.json".into(),
                source: std::io::Error::other("disk full"),
            }))
        } else {
            Ok(())
        }
    });
    let store = Arc::new(OpsStore::with_writer(OpsState::default(), writer));
    assert!(cell.set(Arc::clone(&store)).is_ok());
    Spy { store, seen }
}

/// Catches: a raising change (a halt) written before it is in memory, or dropped when the
/// write fails: memory holds it when the writer is called, and a failed write leaves it
/// raised in memory while the call reports the failure. (The next write carries it.)
#[test]
fn a_raising_change_is_in_memory_before_it_is_written_and_stays_when_the_write_fails() {
    let spy = spy(true);
    let why = spy
        .store
        .change(Write::AfterMemory, entry("halt"), |s| exclude(s, "n1"))
        .expect_err("the write failed");
    assert!(matches!(why, OpsError::Io(_)), "{why}");
    assert_eq!(*spy.seen.lock().expect("lock"), [(1, 1)]);
    assert_eq!(spy.store.state().excluded.len(), 1);
    assert_eq!(spy.store.state().audit.len(), 1);
}

/// Catches: a clearing change entering memory before the file holds it, or surviving a
/// failed write: the writer is called while memory still holds the old state, and a failed
/// write leaves the old state and no audit entry in memory.
#[test]
fn a_clearing_change_is_written_before_it_is_in_memory_and_does_not_happen_when_the_write_fails() {
    let spy = spy(true);
    let why = spy
        .store
        .change(Write::BeforeMemory, entry("include"), |s| exclude(s, "n1"))
        .expect_err("the write failed");
    assert!(matches!(why, OpsError::Io(_)), "{why}");
    assert_eq!(*spy.seen.lock().expect("lock"), [(0, 1)]);
    assert!(spy.store.state().is_empty());
}

/// Both orders, when the write succeeds: the change is in memory afterwards, and the
/// writer was called exactly once, with the state that is now in memory.
#[test]
fn both_orders_end_with_the_change_in_memory_and_one_write() {
    for write in [Write::AfterMemory, Write::BeforeMemory] {
        let spy = spy(false);
        spy.store
            .change(write, entry("exclude"), |s| exclude(s, "n1"))
            .expect("changed");
        assert_eq!(spy.store.state().excluded.len(), 1, "{write:?}");
        assert_eq!(spy.seen.lock().expect("lock").len(), 1, "{write:?}");
    }
}

/// Catches: a ring that grows without bound or is numbered from its length: after more
/// changes than the ring holds it keeps the last 64, numbers continue one by one, and a
/// restart continues from the file's last number.
#[test]
fn the_ring_keeps_the_last_64_and_numbers_continue_after_a_restart() {
    let dir = dir("ops-ring");
    let file = dir.path().join("ops.json");
    let store = OpsStore::open(&file).expect("opened");
    for n in 1..=70_u64 {
        let done = store
            .change(Write::BeforeMemory, entry("include"), |_| {})
            .expect("changed");
        assert_eq!(done.seq, n);
    }
    assert_eq!(store.state().audit.len(), AUDIT_RING);
    let seqs = |state: &OpsState| state.audit.iter().map(|e| e.seq).collect::<Vec<_>>();
    assert_eq!(seqs(&store.state()), (7..=70).collect::<Vec<u64>>());
    let again = OpsStore::open(&file).expect("reopened");
    assert_eq!(seqs(&again.state()), (7..=70).collect::<Vec<u64>>());
    let next = again
        .change(Write::BeforeMemory, entry("include"), |_| {})
        .expect("changed");
    assert_eq!(next.seq, 71);
    assert_eq!(again.state().audit.first().map(|e| e.seq), Some(8));
}

/// A store that keeps nothing still applies and numbers changes.
#[test]
fn a_volatile_store_keeps_changes_in_memory_only() {
    let store = OpsStore::volatile();
    let done = store
        .change(Write::BeforeMemory, entry("exclude"), |s| exclude(s, "n1"))
        .expect("changed");
    assert_eq!(done.seq, 1);
    assert_eq!(store.state().excluded.len(), 1);
}

/// Pins: a refused edit does nothing. With a file behind the store, an edit that refuses
/// returns its own error and leaves the state, the ring, the file's bytes and the next
/// number as they were (the next accepted change is numbered as if the refusal never was),
/// and the writer is not called. This holds with either order of writing.
#[test]
fn a_refused_edit_leaves_the_state_the_ring_and_the_file_untouched() {
    let dir = dir("ops-refused");
    let file = dir.path().join("ops.json");
    let store = OpsStore::open(&file).expect("opened");
    store
        .change(Write::BeforeMemory, entry("exclude"), |s| exclude(s, "n1"))
        .expect("changed");
    let (state, bytes) = (store.state(), fs::read(&file).expect("read"));
    for write in [Write::BeforeMemory, Write::AfterMemory] {
        let refused = store.try_change(write, entry("exclude"), |s| {
            exclude(s, "n2");
            Err(OpsError::Invalid {
                path: "ops.json".into(),
                why: "refused".to_owned(),
            })
        });
        assert!(matches!(refused, Err(OpsError::Invalid { .. })));
        assert_eq!(store.state(), state);
        assert_eq!(fs::read(&file).expect("read"), bytes);
        assert!(!dir.path().join("ops.json.new").exists());
    }
    let next = store
        .change(Write::BeforeMemory, entry("exclude"), |s| exclude(s, "n3"))
        .expect("changed");
    assert_eq!(next.seq, 2);
}
