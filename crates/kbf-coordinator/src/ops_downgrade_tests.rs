//! The ops store across the halt release (B8): the release before reads the ops file this
//! one writes, whatever halts, clears and generations it holds, and this one reads the
//! release before's file. What the halt release adds is in the halts file beside it
//! ([`crate::ops_file`]), which the release before never opens, and which is joined back on
//! reading by host and target. Its reader skips what it does not know. How a change commits
//! both files is tested in `ops_commit_tests`.

use std::fs;
use std::path::{Path, PathBuf};

use kbf_testkit::dir::TestDir;
use kbf_updater::wire::{Outcome, Phase, Token, UpdateResult};

use crate::ops::{
    AuditEntry, Exclusion, Halt, HaltAck, HostOps, OpsError, OpsState, OpsStore, ServiceOps, Write,
};
use crate::ops_file::{halts_path, staged_path};
use crate::testutil::version;

/// The reader of the build before this one: its schema types as they were, strict as they
/// were (`deny_unknown_fields`). Only reading matters, so only `Deserialize` is kept.
#[allow(dead_code)] // Fields are read by serde only.
mod previous {
    use std::collections::BTreeMap;

    use kbf_updater::version::Version;
    use kbf_updater::wire::UpdateResult;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct HostOps {
        #[serde(default)]
        labels: BTreeMap<String, String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Exclusion {
        service: String,
        node_id: String,
        reason: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Halt {
        node_id: String,
        check: String,
        target: Version,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct HaltAck {
        node_id: String,
        target: Version,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FailureAck {
        node_id: String,
        report: UpdateResult,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ServiceOps {
        #[serde(default)]
        halt: Option<Halt>,
        #[serde(default)]
        halt_acked: Vec<HaltAck>,
        #[serde(default)]
        last_good: Option<Version>,
        #[serde(default)]
        bad: Vec<Version>,
        #[serde(default)]
        failure_acked: Vec<FailureAck>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct AuditEntry {
        seq: u64,
        by: String,
        action: String,
        #[serde(default)]
        service: Option<String>,
        #[serde(default)]
        node_id: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OpsState {
        schema: u32,
        #[serde(default)]
        hosts: BTreeMap<String, HostOps>,
        #[serde(default)]
        excluded: Vec<Exclusion>,
        #[serde(default)]
        services: BTreeMap<String, ServiceOps>,
        #[serde(default)]
        audit: Vec<AuditEntry>,
    }

    /// Whether the previous build starts on `text`, its ops file.
    pub(super) fn reads(text: &str) -> Result<(), String> {
        serde_json::from_str::<OpsState>(text)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn halted_update(target: &str) -> UpdateResult {
    let mut report = UpdateResult::new(version(target), Outcome::Failed);
    report.phase = Some(Phase::Halt);
    report.reason = Token::parse("tainted");
    report
}

fn ack(node: &str, target: &str) -> HaltAck {
    HaltAck {
        node_id: node.to_owned(),
        target: version(target),
        report: Some(halted_update(target)),
    }
}

/// A service halted by `s1` on 2.0, with `s2`'s report kept while it stands, `s3`'s cleared
/// report of 1.5 acknowledged, and two clears counted.
fn halted() -> ServiceOps {
    ServiceOps {
        halt: Some(Halt {
            node_id: "s1".to_owned(),
            check: "tainted".to_owned(),
            target: version("2.0"),
            report: Some(halted_update("2.0")),
        }),
        halt_acked: vec![ack("s3", "1.5")],
        halt_seen: vec![ack("s2", "2.0")],
        halt_generation: 2,
        ..ServiceOps::default()
    }
}

fn calm(state: &mut OpsState) {
    state.hosts.insert(
        "s1".to_owned(),
        HostOps {
            labels: [("zone".to_owned(), "a".to_owned())].into(),
        },
    );
    state.excluded.push(Exclusion {
        service: "kbf".to_owned(),
        node_id: "s2".to_owned(),
        reason: "disk replaced".to_owned(),
    });
    let marks = state.services.entry("kbf".to_owned()).or_default();
    marks.last_good = Some(version("1.0"));
    marks.bad.push(version("2.0"));
}

/// A directory with the ops file this build wrote for the state `edit` makes.
fn written(name: &str, edit: impl FnOnce(&mut OpsState)) -> (TestDir, PathBuf, OpsState) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let path = dir.path().join("ops.json");
    let store = OpsStore::open(&path).expect("a store");
    store
        .change(
            Write::BeforeMemory,
            AuditEntry::new("operator", "seed"),
            edit,
        )
        .expect("written");
    let state = store.state();
    (dir, path, state)
}

fn text(path: &Path) -> String {
    fs::read_to_string(path).expect("the file")
}

/// Catches: a farm that never halted being given a halts file, or a key of the halt release
/// written to its ops file: the release before reads the file, and there is no halts file.
#[test]
fn the_release_before_reads_the_ops_file_of_a_farm_that_never_halted() {
    let (_dir, path, _) = written("ops-prev-calm", calm);
    assert_eq!(previous::reads(&text(&path)), Ok(()), "{}", text(&path));
    assert!(!halts_path(&path).exists());
}

/// Catches: a halt's report, an acknowledgement's report, the reports kept while halted or a
/// clear generation written to the ops file, where the release before, which refuses a key it
/// does not know, would not start (B8: the release before on this release's data). It reads
/// the ops file of a halted farm with clears behind it; the rest is in the halts file.
#[test]
fn the_release_before_reads_the_ops_file_of_a_halted_and_cleared_farm() {
    let (_dir, path, _) = written("ops-prev-halted", |state| {
        calm(state);
        state.services.insert("kbf".to_owned(), halted());
    });
    let ops = text(&path);
    assert_eq!(previous::reads(&ops), Ok(()), "{ops}");
    let halts: serde_json::Value =
        serde_json::from_str(&text(&halts_path(&path))).expect("the halts file");
    assert_eq!(halts["services"]["kbf"]["halt_generation"], 2);
    assert_eq!(halts["services"]["kbf"]["halt_seen"][0]["node_id"], "s2");
}

/// Catches: anything of a halt lost between a write and the next start: the state read from
/// the two files is the state written, reports, kept reports and generation included.
#[test]
fn this_release_reads_back_everything_it_wrote() {
    let (_dir, path, state) = written("ops-prev-round", |state| {
        state.services.insert("kbf".to_owned(), halted());
    });
    assert_eq!(state.services["kbf"], halted());
    assert_eq!(OpsStore::open(&path).expect("read").state(), state);
}

/// Pins: an ops file the release before wrote, a halt and its acknowledgement included,
/// reads in this release with no halts file (arrives green;
/// `an_ops_file_from_before_reports_loads_and_its_acks_drop_on_the_first_beat` pins what this
/// release then does with it).
#[test]
fn this_release_reads_an_ops_file_the_release_before_wrote() {
    let text = r#"{"schema":1,"hosts":{},"excluded":[],"services":{"kbf":{
        "halt":{"node_id":"s1","check":"tainted","target":"2.0"},
        "halt_acked":[{"node_id":"s2","target":"1.5"}],"last_good":"1.0","bad":[]}},
        "audit":[]}"#;
    assert_eq!(previous::reads(text), Ok(()));
    let dir = TestDir::under(std::env::temp_dir(), "ops-prev-forward");
    let path = dir.path().join("ops.json");
    fs::write(&path, text).expect("written");
    let marks = OpsStore::open(&path).expect("read").marks("kbf");
    assert_eq!(
        marks.halt.map(|h| (h.node_id, h.report)),
        Some(("s1".to_owned(), None))
    );
    assert_eq!(marks.halt_acked.len(), 1);
}

/// Catches: the halts file thrown away when the release before rewrote the ops file around it
/// (it keeps a halt and its acknowledgements as it read them, and changes the rest): after a
/// rollback and a roll forward again, every report joins back to its halt and acknowledgement.
#[test]
fn the_halts_file_joins_back_after_the_release_before_rewrote_the_ops_file() {
    let (_dir, path, state) = written("ops-prev-rewrite", |state| {
        state.services.insert("kbf".to_owned(), halted());
    });
    let mut ops: serde_json::Value = serde_json::from_str(&text(&path)).expect("json");
    ops["excluded"] = serde_json::json!([{"service": "kbf", "node_id": "s9", "reason": "gone"}]);
    let rewritten = ops.to_string();
    assert_eq!(previous::reads(&rewritten), Ok(()));
    fs::write(&path, rewritten).expect("rewritten");
    let read = OpsStore::open(&path).expect("read").state();
    assert_eq!(read.services["kbf"], state.services["kbf"]);
    assert_eq!(read.excluded.len(), 1);
}

/// Catches: a report joined to a halt or an acknowledgement it was not made for. When the ops
/// file names another halt and another acknowledgement than the halts file, their reports and
/// the reports kept while the old halt stood are dropped (they read as a file from before
/// reports, whose acknowledgements end on the host's next beat, so the service is only halted
/// again); the generation is kept.
#[test]
fn a_halts_file_record_that_matches_nothing_in_the_ops_file_is_dropped() {
    let (_dir, path, _) = written("ops-prev-stale", |state| {
        state.services.insert("kbf".to_owned(), halted());
    });
    let mut ops: serde_json::Value = serde_json::from_str(&text(&path)).expect("json");
    ops["services"]["kbf"]["halt"]["node_id"] = "s7".into();
    ops["services"]["kbf"]["halt_acked"][0]["target"] = "1.6".into();
    fs::write(&path, ops.to_string()).expect("rewritten");
    let marks = OpsStore::open(&path).expect("read").marks("kbf");
    assert_eq!(
        marks.halt.map(|h| (h.node_id, h.report)),
        Some(("s7".to_owned(), None))
    );
    assert_eq!(marks.halt_acked[0].report, None);
    assert_eq!(marks.halt_seen, []);
    assert_eq!(marks.halt_generation, 2);
}

/// Catches: a halts file a later release wrote refusing to start this one (B8: this release
/// on a later release's data): a key it does not know, at any level, is skipped.
#[test]
fn a_halts_file_key_this_release_does_not_know_is_skipped() {
    let (_dir, path, state) = written("ops-prev-unknown", |state| {
        state.services.insert("kbf".to_owned(), halted());
    });
    let halts = halts_path(&path);
    let mut value: serde_json::Value = serde_json::from_str(&text(&halts)).expect("json");
    value["later"] = 1.into();
    value["services"]["kbf"]["later"] = 1.into();
    value["services"]["kbf"]["halt"]["later"] = 1.into();
    value["services"]["kbf"]["halt_seen"][0]["later"] = 1.into();
    fs::write(&halts, value.to_string()).expect("rewritten");
    assert_eq!(OpsStore::open(&path).expect("read").state(), state);
}

/// Catches: a halts file that cannot be parsed read as an empty one, which would forget every
/// generation and acknowledgement it held: the start is refused, naming the file.
#[test]
fn a_halts_file_that_cannot_be_parsed_refuses_to_start() {
    let (_dir, path, _) = written("ops-prev-garbled", |state| {
        state.services.insert("kbf".to_owned(), halted());
    });
    let halts = halts_path(&path);
    fs::write(&halts, "{\"services\": [").expect("garbled");
    match OpsStore::open(&path) {
        Err(OpsError::Invalid { path: named, .. }) => assert_eq!(named, halts),
        other => panic!(
            "expected the halts file refused, got {:?}",
            other.map(|s| s.state())
        ),
    }
}

/// Catches: a clear whose halts file cannot be staged lifting the halt on disk all the same:
/// the clear fails, it is not in memory, the ops file is untouched, and what a restart reads is
/// the halt standing, its reports and the old generation.
#[test]
fn a_clear_whose_halts_file_cannot_be_written_leaves_the_halt_on_disk() {
    let (_dir, path, _) = written("ops-prev-torn", |state| {
        state.services.insert("kbf".to_owned(), halted());
    });
    let ops_before = text(&path);
    let store = OpsStore::open(&path).expect("read");
    let mut blocked = staged_path(&path).into_os_string();
    blocked.push(".new");
    fs::create_dir(&blocked).expect("blocked");
    let cleared = store.change(
        Write::BeforeMemory,
        AuditEntry::new("operator", "clear_halt"),
        |state| {
            let marks = state.services.get_mut("kbf").expect("kbf");
            let halt = marks.halt.take().expect("halted");
            marks.halt_acked.push(HaltAck::from(halt));
            marks.halt_generation += 1;
        },
    );
    assert!(matches!(cleared, Err(OpsError::Io(_))), "{cleared:?}");
    assert_eq!(store.marks("kbf"), halted(), "the clear is not in memory");
    assert_eq!(text(&path), ops_before, "the ops file is untouched");
    assert_eq!(OpsStore::open(&path).expect("read").marks("kbf"), halted());
}

/// Catches: the first write of a store whose halts file cannot be staged leaving an ops file
/// behind (the change failed, so there was none): none is written.
#[test]
fn a_first_write_whose_halts_file_cannot_be_written_leaves_no_ops_file() {
    let dir = TestDir::under(std::env::temp_dir(), "ops-file-first");
    let path = dir.path().join("ops.json");
    let mut blocked = staged_path(&path).into_os_string();
    blocked.push(".new");
    fs::create_dir(&blocked).expect("blocked");
    let store = OpsStore::open(&path).expect("opened");
    let raised = store.change(
        Write::BeforeMemory,
        AuditEntry::new("operator", "seed"),
        |state| {
            state.services.insert("kbf".to_owned(), halted());
        },
    );
    assert!(raised.is_err());
    assert!(!path.exists());
}

/// Catches: a change answered as made, or its halts file renamed in, when the ops file cannot
/// be replaced or read back (here a directory at its name): the change fails, no halts file is
/// published, and the staged one is left for a start to settle by what the ops file holds.
#[test]
fn a_change_whose_ops_file_cannot_be_replaced_publishes_no_halts_file() {
    let dir = TestDir::under(std::env::temp_dir(), "ops-file-dir");
    let path = dir.path().join("ops.json");
    let store = OpsStore::open(&path).expect("opened");
    fs::create_dir(&path).expect("a directory");
    let changed = store.change(
        Write::BeforeMemory,
        AuditEntry::new("operator", "seed"),
        |state| {
            state.services.insert("kbf".to_owned(), halted());
        },
    );
    match changed {
        Err(why @ OpsError::Io(_)) => assert!(why.to_string().contains("ops.json"), "{why}"),
        other => panic!("expected an I/O error, got {other:?}"),
    }
    assert!(!halts_path(&path).exists());
    assert!(staged_path(&path).exists());
}

/// Catches: the generation of a service the ops file no longer lists dropped with the
/// service's other records (it counts on if the service comes back), and a service the halts
/// file lists with nothing that joins added to the state: the one is kept, the other is not.
#[test]
fn a_generation_is_kept_for_a_service_the_ops_file_no_longer_lists() {
    let dir = TestDir::under(std::env::temp_dir(), "ops-file-gone");
    let path = dir.path().join("ops.json");
    fs::write(&path, r#"{"schema":1}"#).expect("written");
    fs::write(
        halts_path(&path),
        r#"{"services":{"back":{"halt_generation":4},"gone":{"halt_seen":[]}}}"#,
    )
    .expect("written");
    let store = OpsStore::open(&path).expect("read");
    let names: Vec<String> = store.state().services.into_keys().collect();
    assert_eq!(names, ["back"]);
    assert_eq!(store.marks("back").halt_generation, 4);
}

/// Catches: a halts file that cannot be read (here a directory at its name) read as none: the
/// start is refused with an I/O error naming it.
#[test]
fn a_halts_file_that_cannot_be_read_refuses_to_start() {
    let dir = TestDir::under(std::env::temp_dir(), "ops-file-unreadable");
    let path = dir.path().join("ops.json");
    fs::create_dir(halts_path(&path)).expect("a directory");
    match OpsStore::open(&path) {
        Err(why @ OpsError::Io(_)) => {
            assert!(why.to_string().contains("ops.json.halts"), "{why}");
        }
        other => panic!("expected an I/O error, got {:?}", other.map(|s| s.state())),
    }
}

/// Catches: a halts file left as it was once it has nothing to keep, so a later start joins
/// what the state no longer holds: once there is a halts file, every change writes it.
#[test]
fn a_halts_file_once_written_is_written_on_every_change() {
    let (_dir, path, _) = written("ops-file-every", |state| {
        state.services.insert("kbf".to_owned(), halted());
    });
    let store = OpsStore::open(&path).expect("read");
    store
        .change(
            Write::BeforeMemory,
            AuditEntry::new("operator", "seed"),
            |state| {
                state.services.remove("kbf");
            },
        )
        .expect("changed");
    let halts: serde_json::Value =
        serde_json::from_str(&text(&halts_path(&path))).expect("the halts file");
    assert_eq!(halts["services"], serde_json::json!({}));
}
