//! The ops file across the release that keeps halt reports, clears and their generations:
//! this build reads a file the build before it wrote, and the build before it refuses a file
//! that holds what it would ignore (a halt it does not act on, the acknowledgements and the
//! generations of a clear), so that rolling a coordinator back across this release stops at
//! the start, with the path, until the operator moves the file aside on purpose
//! (`docs/deploy/kbf-updater.md`, `--ops-file`). A file of a farm that never halted reads in
//! both.

use kbf_updater::wire::{Outcome, Phase, Token, UpdateResult};

use crate::ops::{AuditEntry, Exclusion, Halt, HaltAck, HostOps, OpsState, OpsStore, Write};
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

    /// Whether the previous build starts on `text`.
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

/// The file this build writes for `state`, as it writes it.
fn written(name: &str, edit: impl FnOnce(&mut OpsState)) -> String {
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), name);
    let path = dir.path().join("ops.json");
    OpsStore::open(&path)
        .expect("a store")
        .change(
            Write::BeforeMemory,
            AuditEntry::new("operator", "seed"),
            edit,
        )
        .expect("written");
    std::fs::read_to_string(&path).expect("the file")
}

/// Catches: a key of this release written for a farm that never halted (an empty
/// `halt_seen`, a generation of 0), which would stop a rollback the file gives no reason
/// to stop: the previous build reads it.
#[test]
fn the_previous_build_reads_a_file_of_a_farm_that_never_halted() {
    let text = written("ops-prev-calm", |state| {
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
    });
    assert_eq!(previous::reads(&text), Ok(()), "{text}");
}

/// Catches: the previous build starting on a file that holds a halt this build raised. It
/// has no halt module: it would go on ordering the halted service. It refuses the file (the
/// halt's `report`), so the operator decides.
#[test]
fn the_previous_build_refuses_a_file_holding_a_halt() {
    let text = written("ops-prev-halt", |state| {
        state.services.entry("kbf".to_owned()).or_default().halt = Some(Halt {
            node_id: "s1".to_owned(),
            check: "tainted".to_owned(),
            target: version("2.0"),
            report: Some(halted_update("2.0")),
        });
    });
    let refused = previous::reads(&text).expect_err("a halt it would ignore");
    assert!(refused.contains("unknown field `report`"), "{refused}");
}

/// Catches: the previous build starting on a file of a cleared halt, where it would forget
/// the acknowledgements (and the generation agents lift on) that keep the cleared report
/// from halting again once this build is back.
#[test]
fn the_previous_build_refuses_a_file_holding_a_clear() {
    let text = written("ops-prev-clear", |state| {
        let marks = state.services.entry("kbf".to_owned()).or_default();
        marks.halt_acked.push(HaltAck {
            node_id: "s1".to_owned(),
            target: version("2.0"),
            report: Some(halted_update("2.0")),
        });
        marks.halt_generation = 1;
    });
    let refused = previous::reads(&text).expect_err("a clear it would forget");
    assert!(refused.contains("unknown field"), "{refused}");
}

/// Pins: a file the previous build wrote, a halt and its acknowledgement included, reads in
/// this build (arrives green; `an_ops_file_from_before_reports_loads_and_its_acks_drop_on_the_first_beat`
/// pins what this build then does with it).
#[test]
fn this_build_reads_a_file_the_previous_build_wrote() {
    let text = r#"{"schema":1,"hosts":{},"excluded":[],"services":{"kbf":{
        "halt":{"node_id":"s1","check":"tainted","target":"2.0"},
        "halt_acked":[{"node_id":"s2","target":"1.5"}],"last_good":"1.0","bad":[]}},
        "audit":[]}"#;
    assert_eq!(previous::reads(text), Ok(()));
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "ops-prev-forward");
    let path = dir.path().join("ops.json");
    std::fs::write(&path, text).expect("written");
    let marks = OpsStore::open(&path).expect("read").marks("kbf");
    assert_eq!(marks.halt.map(|h| h.node_id), Some("s1".to_owned()));
    assert_eq!(marks.halt_acked.len(), 1);
}
