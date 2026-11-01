//! A fake farm: the history routes of the operator API, answered from a table.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;

use kbf_estimator_replay::readonly::{CallError, Reply, Transport};
use serde_json::{Value, json};

pub const GIB: u64 = 1 << 30;
/// 2026-10-01T00:00:00Z and 2026-10-03T00:00:00Z, in microseconds.
pub const OCT_1: u64 = 1_790_812_800_000_000;
pub const DAY: u64 = 86_400_000_000;

pub const SINCE: &str = "2026-10-01T00:00:00Z";
pub const UNTIL: &str = "2026-10-03T00:00:00Z";
pub const INVOCATIONS: &str =
    "/v1/invocations?since=2026-10-01T00%3A00%3A00Z&until=2026-10-03T00%3A00%3A00Z&page_size=1000";

pub type Routes = BTreeMap<String, (u16, String)>;

pub fn put(routes: &mut Routes, path: &str, body: &Value) {
    routes.insert(path.to_owned(), (200, body.to_string()));
}

/// An RFC 3339 time `us` microseconds after the epoch, as the API writes one.
pub fn at(us: u64) -> String {
    let (secs, frac) = (us / 1_000_000, us % 1_000_000);
    let t = time::OffsetDateTime::from_unix_timestamp(i64::try_from(secs).unwrap()).unwrap();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{frac:06}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// One run as `GET /v1/runs/{id}` renders it.
pub fn run(id: &str, worker: &str, queued: u64, class: &str, usage: Value) -> Value {
    json!({
        "run_id": id,
        "instance": "main",
        "worker": worker,
        "queued": at(queued),
        "started": at(queued + 1_000_000),
        "ended": at(queued + 61_000_000),
        "class": class,
        "memory_kill": "none",
        "usage": usage,
        "action": format!("{}-100", id.repeat(3)),
        "shape": {"key": "00000000000000aa", "cfg": "c1", "target": null, "test": false},
    })
}

pub fn usage(peak: u64, wall: u64) -> Value {
    json!({"cpu_usage_us": 2_000_000, "wall_us": wall, "memory_peak_bytes": peak})
}

/// Three builds' worth of history over two nodes (see `export.rs`).
pub fn farm() -> Routes {
    let mut r = Routes::new();
    put(
        &mut r,
        "/v1/nodes",
        &json!({"nodes": [
            {"node_id": "n1", "software": {"os_name": "Ubuntu"}},
            {"node_id": "mac1", "software": {"os_name": "macOS"}},
            {"node_id": "old", "software": null},
        ]}),
    );
    put(
        &mut r,
        "/v1/nodes/n1",
        &json!({"capacity": {"cpu_millis": 8000, "memory_bytes": 64 * GIB}}),
    );
    put(
        &mut r,
        "/v1/nodes/mac1",
        &json!({"capacity": {"cpu_millis": 8000, "memory_bytes": 32 * GIB}}),
    );
    put(
        &mut r,
        INVOCATIONS,
        &json!({"invocations": [{"id": "b1"}, {"id": "b/2"}]}),
    );
    // Build b1: two pages of actions.
    let first = "/v1/invocations/b1/actions?page_size=1000";
    let second = "/v1/invocations/b1/actions?page_size=1000&page_token=2";
    put(
        &mut r,
        first,
        &json!({"actions": [
            {"attempts": [{"run_id": "r1"}, {"run_id": "r2"}]},
            {"attempts": [{"queued": "x"}]},
        ], "next_page": second}),
    );
    put(
        &mut r,
        second,
        &json!({"actions": [{"attempts": [{"run_id": "r3"}, {"run_id": "r4"}]}]}),
    );
    // Build b/2 (the id is encoded in the path) repeats r1 and adds r5 (outside the window).
    put(
        &mut r,
        "/v1/invocations/b%2F2/actions?page_size=1000",
        &json!({"actions": [{"attempts": [{"run_id": "r1"}, {"run_id": "r5"}, {"run_id": "r6"}]}]}),
    );
    // r1: a success on n1 at 4 GiB (no booking kept), peak 3 GiB.
    put(
        &mut r,
        "/v1/runs/r1",
        &run(
            "r1",
            "n1",
            OCT_1 + 5_000_000,
            "ok",
            usage(3 * GIB, 60_000_000),
        ),
    );
    // r2: an own-limit kill at 4 GiB raised to 16; the read API wrote 0 for peak and wall.
    let mut killed = run("r2", "n1", OCT_1 + 2_000_000, "memory_killed", usage(0, 0));
    killed["memory_kill"] = "own_limit".into();
    killed["booked_memory_bytes"] = (4 * GIB).into();
    killed["raised_memory_bytes"] = (16 * GIB).into();
    killed["memory_used_bytes"] = 0.into();
    put(&mut r, "/v1/runs/r2", &killed);
    // r3: on a Mac, with the fields a newer server adds.
    let mut mac = run(
        "r3",
        "mac1",
        OCT_1 + DAY + 5,
        "ok",
        json!({
            "cpu_usage_us": 1, "wall_us": 2_000_000, "memory_peak_bytes": GIB,
            "basis": "mac_footprint", "booked_millicpus": 1000, "node_cpu_pressure_centi": 1234,
            "memory_events_max": 2, "memory_events_oom_kill": 1, "memory_swap_peak_bytes": 7,
            "memory_anon_swap_peak_bytes": 8, "cpu_pressure_some_us": 9,
        }),
    );
    // The operation, as the run route writes it (a number under `operation_id`).
    let served = kbf_server::run_json(&kbf_history::records::RunRecord {
        operation_id: Some(9),
        ..Default::default()
    });
    mac["operation_id"] = served["operation_id"].clone();
    put(&mut r, "/v1/runs/r3", &mac);
    // r4: a node the list does not know, no shape, no usage, never started.
    let mut lost = run("r4", "gone", OCT_1 + DAY + 1, "lost", Value::Null);
    lost["shape"] = Value::Null;
    lost["started"] = Value::Null;
    lost["ended"] = Value::Null;
    lost["action"] = Value::Null;
    lost.as_object_mut().unwrap().remove("memory_kill");
    put(&mut r, "/v1/runs/r4", &lost);
    // r5: queued before the window.
    put(
        &mut r,
        "/v1/runs/r5",
        &run("r5", "n1", OCT_1 - 1, "ok", usage(GIB, 5_000_000)),
    );
    // r6: not held (404).
    r
}

/// A transport over `routes` that records every request; a path it has no route for is 404.
pub struct Fake {
    pub routes: Routes,
    pub seen: RefCell<Vec<(String, String)>>,
}

impl Fake {
    pub fn new(routes: Routes) -> Self {
        Self {
            routes,
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl Transport for Fake {
    fn call(&self, method: &str, path: &str) -> Result<Reply, CallError> {
        self.seen
            .borrow_mut()
            .push((method.to_owned(), path.to_owned()));
        let (status, body) = self
            .routes
            .get(path)
            .cloned()
            .unwrap_or((404, String::new()));
        Ok(Reply { status, body })
    }
}

impl Transport for &Fake {
    fn call(&self, method: &str, path: &str) -> Result<Reply, CallError> {
        (**self).call(method, path)
    }
}

// ---- the replay ----

use kbf_estimator::{Basis, Cfg, Instance, Key, PlatformClass, Shape};
use kbf_estimator_replay::replay::trace::{Op, Recorded, Trace};
use kbf_estimator_replay::row::{Capacity, Row, Usage};

pub const MIB: u64 = 1 << 20;
pub const HOUR_S: u64 = 3_600;
pub const DAY_S: u64 = 86_400;

pub fn key(instance: &str, shape: u64, cfg: Option<u64>) -> Key {
    Key::new(
        Instance::new(instance),
        PlatformClass::new("linux"),
        Basis::Cgroup,
        Shape(shape),
        cfg.map(Cfg),
    )
}

/// An operation of `instance` and `shape` arriving `at_s` seconds in, needing `need` bytes,
/// that runs `run_s` seconds when it completes and `kill_s` when it is killed; first
/// booking 4 GiB, one core. The digest is unique to the arrival.
pub fn op(instance: &str, shape: u64, at_s: u64, need: u64, run_s: u64, kill_s: u64) -> Op {
    Op {
        instance: instance.to_owned(),
        digest: format!("d-{instance}-{shape}-{at_s}"),
        key: Some(key(instance, shape, None)),
        arrives_us: at_s * 1_000_000,
        first_booked: 4 * GIB,
        cpu_millis: 1_000,
        need,
        run_us: run_s * 1_000_000,
        kill_us: kill_s * 1_000_000,
        cpu_usage_us: run_s * 500_000,
        recorded_kills: 0,
        recorded_attempts: 1,
    }
}

/// A trace of `ops` on nodes of `(name, cores, GiB)`.
pub fn trace(ops: Vec<Op>, nodes: &[(&str, u64, u64)]) -> Trace {
    Trace {
        ops,
        nodes: nodes
            .iter()
            .map(|(n, cores, gib)| {
                (
                    (*n).to_owned(),
                    Capacity {
                        cpu_millis: cores * 1_000,
                        memory_bytes: gib * GIB,
                    },
                )
            })
            .collect(),
        funnel: Default::default(),
        recorded: Recorded::default(),
    }
}

/// A recorded attempt on node `n1`, queued at `queued_s` seconds after 2026-10-01, that
/// ran `run_s` seconds.
pub fn attempt(digest: &str, queued_s: u64, run_s: u64, outcome: &str, peak: Option<u64>) -> Row {
    let queued_us = OCT_1 + queued_s * 1_000_000;
    Row {
        run_id: format!("run-{digest}-{queued_s}"),
        instance: "main".into(),
        platform: "linux".into(),
        node: "n1".into(),
        shape: "00000000000000aa".into(),
        cfg: "-".into(),
        digest: digest.into(),
        outcome: outcome.into(),
        memory_kill: "none".into(),
        booked_memory_bytes: None,
        raised_memory_bytes: None,
        memory_used_bytes: None,
        cgroup: Some(Usage {
            cpu_usage_us: run_s * 500_000,
            wall_us: Some(run_s * 1_000_000),
            memory_peak_bytes: peak,
            ..Usage::default()
        }),
        queued_us,
        started_us: Some(queued_us + 1_000_000),
        ended_us: Some(queued_us + (run_s + 1) * 1_000_000),
        node_capacity: Some(Capacity {
            cpu_millis: 16_000,
            memory_bytes: 64 * GIB,
        }),
        basis: None,
        operation: None,
        booked_millicpus: None,
        node_cpu_pressure_centi: None,
        invocation_id: None,
        correlated_invocations_id: None,
    }
}

/// An own-limit kill of `digest` booked at `booked`, retried at `raised`.
pub fn kill(digest: &str, queued_s: u64, run_s: u64, booked: u64, raised: Option<u64>) -> Row {
    let mut r = attempt(digest, queued_s, run_s, "memory_killed", None);
    r.memory_kill = "own_limit".into();
    r.booked_memory_bytes = Some(booked);
    r.raised_memory_bytes = raised;
    r.memory_used_bytes = Some(0);
    r.cgroup = None;
    r
}

/// A deterministic synthetic history: 60 operations over four days on three nodes, eight
/// shapes with a memory need each, climbing the 4, 16, 32 GiB ladder as the farm does.
pub fn synthetic() -> Vec<Row> {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move |n: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % n
    };
    // Needs of the eight shapes in MiB: five fit the first rung, one needs the second, one the
    // third, and one needs the whole 64 GiB host.
    let needs_mib: [u64; 8] = [900, 2_048, 3_072, 5_120, 6_000, 14_000, 30_000, 60_000];
    let mut rows = Vec::new();
    for i in 0..60_u64 {
        let shape = next(8) as usize;
        let need = needs_mib[shape] * MIB + next(MIB);
        let digest = format!("{:064x}-{}", i + 1, 100 + i);
        let mut t = 5_760 * i + next(3_000);
        let run_s = 20 + next(200);
        let mut booked = 4 * GIB;
        loop {
            let limit = booked + booked / 2 + 512 * MIB;
            let mut row = if need > limit {
                let raised = [16 * GIB, 32 * GIB, 64 * GIB]
                    .into_iter()
                    .find(|r| *r > booked);
                kill(
                    &digest,
                    t,
                    run_s / 2,
                    booked,
                    raised.filter(|_| booked < 64 * GIB),
                )
            } else {
                attempt(&digest, t, run_s, "ok", Some(need))
            };
            row.shape = format!("{:016x}", shape + 1);
            row.node = ["n1", "n2", "n3"][next(3) as usize].to_owned();
            row.node_capacity = Some(match row.node.as_str() {
                "n1" => Capacity {
                    cpu_millis: 16_000,
                    memory_bytes: 64 * GIB,
                },
                "n2" => Capacity {
                    cpu_millis: 8_000,
                    memory_bytes: 32 * GIB,
                },
                _ => Capacity {
                    cpu_millis: 8_000,
                    memory_bytes: 16 * GIB,
                },
            });
            let killed = row.outcome == "memory_killed";
            rows.push(row);
            if !killed || booked >= 64 * GIB {
                break;
            }
            t += run_s / 2 + 2;
            booked = [16 * GIB, 32 * GIB, 64 * GIB]
                .into_iter()
                .find(|r| *r > booked)
                .unwrap_or(booked);
        }
    }
    rows
}
