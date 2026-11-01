//! The operation id of a run, from the route that writes it to the rows the replay reads:
//! the fake farm here answers with `kbf_server::run_json`, the run route's own renderer.

mod common;

use common::*;
use kbf_estimator_replay::export::{Window, export};
use kbf_estimator_replay::readonly::ReadOnly;
use kbf_estimator_replay::replay::Params;
use kbf_estimator_replay::replay::trace::build;
use kbf_estimator_replay::row::Row;
use kbf_history::records::{MemoryKill, RunOutcome, RunRecord, Timestamps, WireDigest};
use kbf_history::{RunId, StreamId};
use serde_json::json;

/// A run of `action` on node `n1`, queued `queued_s` seconds after 2026-10-01, as the
/// server holds it.
fn record(seq: u32, action: u8, queued_s: u64, operation: Option<u64>) -> RunRecord {
    let queued_us = OCT_1 + queued_s * 1_000_000;
    let mut r = RunRecord {
        instance: "main".to_owned(),
        node: "n1".to_owned(),
        outcome: RunOutcome::Ok.into(),
        cfg: "-".to_owned(),
        timestamps: Some(Timestamps {
            queued_us,
            ..Timestamps::default()
        }),
        attempt_queued_us: queued_us,
        started_us: queued_us + 1_000_000,
        ended_us: queued_us + 11_000_000,
        action: Some(WireDigest::new([action; 32], 100)),
        operation_id: operation,
        ..RunRecord::default()
    };
    r.set_run_id(RunId::new(1_791_709_200_000, StreamId(7), seq));
    r
}

/// An own-limit kill the ladder retried at 16 GiB.
fn killed(mut r: RunRecord) -> RunRecord {
    r.outcome = RunOutcome::MemoryKilled.into();
    r.memory_kill = MemoryKill::OwnLimit.into();
    r.booked_memory_bytes = 4 * GIB;
    r.raised_memory_bytes = 16 * GIB;
    r
}

/// A farm of one build whose attempts are `records`, each answered as the run route does.
fn served(records: &[RunRecord]) -> Fake {
    let mut r = Routes::new();
    put(&mut r, "/v1/nodes", &json!({"nodes": [{"node_id": "n1"}]}));
    put(
        &mut r,
        "/v1/nodes/n1",
        &json!({"capacity": {"cpu_millis": 8000, "memory_bytes": 64 * GIB}}),
    );
    put(&mut r, INVOCATIONS, &json!({"invocations": [{"id": "b1"}]}));
    let attempts: Vec<_> = records
        .iter()
        .map(|rec| json!({"run_id": rec.run_id().to_string()}))
        .collect();
    put(
        &mut r,
        "/v1/invocations/b1/actions?page_size=1000",
        &json!({"actions": [{"attempts": attempts}]}),
    );
    for rec in records {
        put(
            &mut r,
            &format!("/v1/runs/{}", rec.run_id()),
            &kbf_server::run_json(rec),
        );
    }
    Fake::new(r)
}

fn rows_of(records: &[RunRecord]) -> Vec<Row> {
    let fake = served(records);
    let window = Window::parse(SINCE, UNTIL).unwrap();
    export(&ReadOnly::new(&fake), window).unwrap().rows
}

/// Catches an exporter that reads another key or another type than the run route writes
/// (`operation_id`, a number), so every row from a server has no operation; and an id of 0,
/// a real one, read as absent. A run the server gives no operation stays without one.
#[test]
fn an_export_from_a_server_carries_the_run_operation() {
    let rows = rows_of(&[
        record(1, 1, 100, Some(41)),
        record(2, 2, 200, Some(0)),
        record(3, 3, 300, None),
    ]);
    let ops: Vec<Option<&str>> = rows.iter().map(|r| r.operation.as_deref()).collect();
    assert_eq!(ops, [Some("41"), Some("0"), None]);
}

/// Catches a retry paired with its kill by the time gap alone once the server names the
/// operation: a retry hours after the kill (one operation, same id), and a retry right
/// after it that another operation made (two operations, two ids).
#[test]
fn a_server_retry_is_paired_with_its_kill_by_operation() {
    let far = rows_of(&[
        killed(record(1, 1, 0, Some(7))),
        record(2, 1, 4_000, Some(7)),
    ]);
    assert_eq!(build(far, &Params::default()).ops.len(), 1);

    let near = rows_of(&[killed(record(1, 1, 0, Some(7))), record(2, 1, 12, Some(8))]);
    assert_eq!(build(near, &Params::default()).ops.len(), 2);
}
