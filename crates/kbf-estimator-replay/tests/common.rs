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
    mac["operation"] = "op-9".into();
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
