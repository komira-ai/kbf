//! The run as `GET /v1/runs/{id}` renders it. Each test names the defect it catches.

use kbf_history::records::{Cgroup, MemoryKill, RunOutcome, RunRecord, Timestamps, WireDigest};
use kbf_history::{RunId, StreamId};
use serde_json::{Value, json};

use super::{parse, render};

fn wire(n: u8) -> WireDigest {
    WireDigest::new([n; 32], u64::from(n))
}

fn text(n: u8) -> String {
    format!("{}-{n}", hex::encode([n; 32]))
}

fn base() -> RunRecord {
    let mut r = RunRecord {
        instance: "main".to_owned(),
        node: "node-a".to_owned(),
        lease_epoch: 3,
        lease_seq: 11,
        outcome: RunOutcome::Ok.into(),
        cfg: "-".to_owned(),
        timestamps: Some(Timestamps {
            queued_us: 1_000_000,
            granted_us: 2_000_000,
            ..Timestamps::default()
        }),
        ..RunRecord::default()
    };
    r.set_run_id(RunId::new(1_791_709_200_000, StreamId(7), 4));
    r
}

/// Catches: an id that is not the 24 digits the record carries, a link that names
/// another route, a lease not shown as `term.seq`, an attempt time left out or made
/// the epoch (`started` and `ended` that were never set are `null`), the attempt's own
/// queue time replaced by the first submission, and a key the record has no value for
/// written as 0 (a memory booking on a run that was not killed, an exit code on a
/// lost one).
#[test]
fn a_run_says_what_the_record_has_and_leaves_out_what_it_has_not() {
    let id = "0001a2b3c4d0000007000004";
    let mut r = base();
    r.set_run_id(id.parse().unwrap());
    let v = render(&r);
    assert_eq!(v["run_id"], id);
    assert_eq!(v["href"], format!("/v1/runs/{id}"));
    assert_eq!(v["lease"], "3.11");
    assert_eq!(v["worker"], "node-a");
    assert_eq!(v["instance"], "main");
    assert_eq!(v["queued"], "1970-01-01T00:00:01.000000Z");
    assert_eq!(v["granted"], "1970-01-01T00:00:02.000000Z");
    assert_eq!((&v["started"], &v["ended"]), (&Value::Null, &Value::Null));
    assert_eq!(v["class"], "ok");
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["memory_kill"], "none");
    assert_eq!(
        v["reapi_times"],
        json!({"queued": "1970-01-01T00:00:01.000000Z"})
    );
    for absent in [
        "reason",
        "booked_memory_bytes",
        "cap_bytes",
        "raised_memory_bytes",
        "memory_used_bytes",
        "memory_limit_bytes",
        "farm_runs",
        "farm_budget",
        "wait_reason",
        "partition",
        "presubmit_us",
        "usage",
    ] {
        assert!(v.get(absent).is_none(), "{absent}: {v}");
    }
    for null in ["action", "command", "result", "stdout", "stderr", "shape"] {
        assert_eq!(v[null], Value::Null, "{null}");
    }

    r.attempt_queued_us = 1_500_000;
    r.started_us = 2_500_000;
    r.ended_us = 3_000_000;
    r.outcome = RunOutcome::Lost.into();
    let v = render(&r);
    assert_eq!(v["queued"], "1970-01-01T00:00:01.500000Z");
    assert_eq!(v["started"], "1970-01-01T00:00:02.500000Z");
    assert_eq!(v["ended"], "1970-01-01T00:00:03.000000Z");
    assert_eq!(v["class"], "lost");
    assert!(v.get("exit_code").is_none(), "a lost run has no exit code");
}

/// Catches: a memory kill, its booking, cap and raised booking, the memory the run used
/// and its limit, the farm reruns, the wait reason, the partition and the presubmit
/// time dropped; a class given another name; and the digests, shape and target
/// misspelled.
#[test]
fn a_killed_run_says_how_and_a_shape_what_it_names() {
    let mut r = base();
    r.outcome = RunOutcome::MemoryKilled.into();
    r.memory_kill = MemoryKill::OwnLimit.into();
    r.booked_memory_bytes = 10;
    r.cap_bytes = 11;
    r.raised_memory_bytes = 12;
    r.memory_used_bytes = 13;
    r.memory_limit_bytes = 14;
    r.farm_runs = 1;
    r.farm_budget = 2;
    r.wait_reason = "no node".to_owned();
    r.partition = "linux/large".to_owned();
    r.presubmit_us = 15;
    r.reason = "killed".to_owned();
    r.action = Some(wire(1));
    r.command = Some(wire(2));
    r.result = Some(wire(3));
    r.stdout = Some(wire(4));
    r.stderr = Some(wire(5));
    r.shape = 0x00ab;
    r.cfg = "cfg1".to_owned();
    r.target = "root//a:b".to_owned();
    r.target_kind = 2;
    r.test = true;
    let v = render(&r);
    assert_eq!(v["class"], "memory_killed");
    assert_eq!(v["memory_kill"], "own_limit");
    for (key, want) in [
        ("booked_memory_bytes", 10),
        ("cap_bytes", 11),
        ("raised_memory_bytes", 12),
        ("memory_used_bytes", 13),
        ("memory_limit_bytes", 14),
        ("farm_runs", 1),
        ("farm_budget", 2),
        ("presubmit_us", 15),
    ] {
        assert_eq!(v[key], want, "{key}");
    }
    assert_eq!(v["wait_reason"], "no node");
    assert_eq!(v["partition"], "linux/large");
    assert_eq!(v["reason"], "killed");
    assert_eq!(v["action"], text(1));
    assert_eq!(v["command"], text(2));
    assert_eq!(v["result"], text(3));
    assert_eq!(v["stdout"], text(4));
    assert_eq!(v["stderr"], text(5));
    assert_eq!(
        v["shape"],
        json!({"key": "00000000000000ab", "cfg": "cfg1", "test": true,
               "target": {"label": "root//a:b", "kind": "anon"}})
    );
    r.memory_kill = MemoryKill::NodePressure.into();
    assert_eq!(render(&r)["memory_kill"], "node_pressure");
    // Every class has its name, and a shape's target its kind.
    for (outcome, name) in [
        (RunOutcome::Unspecified, "unspecified"),
        (RunOutcome::Failed, "failed"),
        (RunOutcome::TimedOut, "timed_out"),
        (RunOutcome::Infra, "infra"),
        (RunOutcome::Cancelled, "cancelled"),
    ] {
        r.outcome = outcome.into();
        assert_eq!(render(&r)["class"], name);
    }
    for (kind, name) in [(1, json!("target")), (3, json!("bxl"))] {
        r.target_kind = kind;
        assert_eq!(render(&r)["shape"]["target"]["kind"], name);
    }
    r.target_kind = 0;
    assert_eq!(render(&r)["shape"]["target"], Value::Null);
    r.target_kind = 1;
    r.target = String::new();
    assert_eq!(render(&r)["shape"]["target"], Value::Null);
}

/// Catches: the reapi times written when they were not reported (a time of 0 shown as
/// 1970), the cgroup counters dropped or the parallelism computed with no wall time (a
/// division by zero shown as infinity), NUMA nodes of 0 bytes listed.
#[test]
fn the_times_and_the_cgroup_block_are_what_the_daemon_reported() {
    let mut r = base();
    r.timestamps = Some(Timestamps {
        queued_us: 1_000_000,
        worker_start_us: 3_000_000,
        worker_completed_us: 4_000_000,
        ..Timestamps::default()
    });
    r.cgroup = Some(Cgroup {
        cpu_usage_us: 8,
        cpu_user_us: 5,
        cpu_system_us: 3,
        periods: 2,
        throttled_periods: 1,
        throttled_us: 9,
        memory_peak_bytes: 77,
        numa_bytes: vec![10, 0, 20],
        wall_us: 4,
    });
    let v = render(&r);
    assert_eq!(
        v["reapi_times"],
        json!({"queued": "1970-01-01T00:00:01.000000Z",
               "worker_start": "1970-01-01T00:00:03.000000Z",
               "worker_completed": "1970-01-01T00:00:04.000000Z"})
    );
    assert_eq!(
        v["usage"],
        json!({"wall_us": 4, "cpu_usage_us": 8, "cpu_user_us": 5, "cpu_system_us": 3,
               "cpu_periods": 2, "cpu_throttled": 1, "throttled_us": 9,
               "memory_peak_bytes": 77, "parallelism": 2.0,
               "numa_bytes": [{"node": 0, "bytes": 10}, {"node": 2, "bytes": 20}]})
    );
    r.cgroup = Some(Cgroup {
        cpu_usage_us: 8,
        ..Cgroup::default()
    });
    let usage = &render(&r)["usage"];
    assert!(usage.get("parallelism").is_none());
    assert!(usage.get("numa_bytes").is_none());
    r.timestamps = None;
    assert_eq!(render(&r)["reapi_times"], json!({}));
}

/// Catches: a run id taken in upper case (the same run under two links) or of another
/// length, or one with a sign, and so an id that is not the route's.
#[test]
fn only_the_lowercase_24_digits_are_a_run_id() {
    assert!(parse("0001a2b3c4d0000007000004").is_some());
    for bad in [
        "0001A2B3C4D00000070000000004",
        "0001a2b3c4d0000007000000000",
        "0001a2b3c4d00000070000044",
        "",
        "+001a2b3c4d00000070000000004",
        "../nodes",
    ] {
        assert!(parse(bad).is_none(), "{bad:?}");
    }
}
