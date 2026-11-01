//! The exporter's walk over a fake farm: what it asks, what rows it makes, what it counts,
//! and that it asks with GET only.

mod common;

use common::*;
use kbf_estimator_replay::export::{ExportError, Window, export};
use kbf_estimator_replay::readonly::{CallError, ReadOnly, Reply, Transport};
use kbf_estimator_replay::row::{Capacity, Row, Usage};
use serde_json::json;

fn window() -> Window {
    Window::parse(SINCE, UNTIL).unwrap()
}

/// Catches: a server that now writes the lease's grant on every attempt (`booked_memory_bytes`
/// on an accepted run, and `booked_millicpus` beside `usage`) failing the walk, either key
/// ignored, or a run without them given a made-up booking.
#[test]
fn a_run_that_names_its_grant_keeps_both_axes() {
    let mut routes = farm();
    let mut granted = run(
        "r1",
        "n1",
        OCT_1 + 5_000_000,
        "ok",
        usage(3 * GIB, 60_000_000),
    );
    granted["booked_memory_bytes"] = (6 * GIB).into();
    granted["booked_millicpus"] = 2500.into();
    put(&mut routes, "/v1/runs/r1", &granted);
    let fake = Fake::new(routes);
    let done = export(&ReadOnly::new(&fake), window()).unwrap();
    let r1 = done.rows.iter().find(|r| r.run_id == "r1").unwrap();
    assert_eq!(
        (r1.booked_memory_bytes, r1.booked_millicpus),
        (Some(6 * GIB), Some(2500))
    );
    let mac = done.rows.iter().find(|r| r.run_id == "r3").unwrap();
    assert_eq!(
        mac.booked_millicpus,
        Some(1000),
        "the usage block's key still reads"
    );
}

/// Catches: a walk that stops after the first page of an invocation's actions (r3, r4 lost),
/// one that does not encode an id into the path (`b/2` would reach another route), one that
/// reads a run twice because two builds list it, one that drops a run another route does
/// not hold instead of counting it, a window that keeps r5 (queued before it), a node that
/// is not looked up by id (platform, capacity), a read API zero kept as a measurement, and
/// rows out of queue order. Also: every request is a GET.
#[test]
fn a_window_becomes_its_attempts_in_queue_order() {
    let fake = Fake::new(farm());
    let api = ReadOnly::new(&fake);
    let done = export(&api, window()).unwrap();

    let ids: Vec<&str> = done.rows.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(ids, ["r2", "r1", "r4", "r3"], "by queue time");
    let report = &done.report;
    assert_eq!(
        (
            report.invocations,
            report.attempts,
            report.without_run_id,
            report.duplicate_run_ids,
            report.run_gone,
            report.outside_window,
            report.rows
        ),
        (2, 8, 1, 1, 1, 1, 4)
    );

    let r1 = &done.rows[1];
    assert_eq!(
        *r1,
        Row {
            run_id: "r1".into(),
            instance: "main".into(),
            platform: "linux".into(),
            node: "n1".into(),
            shape: "00000000000000aa".into(),
            cfg: "c1".into(),
            digest: format!("{}-100", "r1".repeat(3)),
            outcome: "ok".into(),
            memory_kill: "none".into(),
            booked_memory_bytes: None,
            raised_memory_bytes: None,
            memory_used_bytes: None,
            cgroup: Some(Usage {
                cpu_usage_us: 2_000_000,
                wall_us: Some(60_000_000),
                memory_peak_bytes: Some(3 * GIB),
                ..Usage::default()
            }),
            queued_us: OCT_1 + 5_000_000,
            started_us: Some(OCT_1 + 6_000_000),
            ended_us: Some(OCT_1 + 66_000_000),
            node_capacity: Some(Capacity {
                cpu_millis: 8000,
                memory_bytes: 64 * GIB
            }),
            basis: None,
            operation: None,
            booked_millicpus: None,
            node_cpu_pressure_centi: None,
            invocation_id: None,
            correlated_invocations_id: None,
            phase_times: None,
            fetch_us: None,
            execution_us: None,
            pack_us: None,
        }
    );

    // A kill: the booking and the raise are kept; the zero peak and wall are absent.
    let r2 = &done.rows[0];
    assert_eq!(r2.memory_kill, "own_limit");
    assert_eq!(r2.booked_memory_bytes, Some(4 * GIB));
    assert_eq!(r2.raised_memory_bytes, Some(16 * GIB));
    assert_eq!(r2.memory_used_bytes, Some(0));
    let usage = r2.cgroup.as_ref().unwrap();
    assert_eq!((usage.wall_us, usage.memory_peak_bytes), (None, None));
    assert_eq!(usage.cpu_usage_us, 2_000_000);

    // A node the list does not know, a run with no shape, usage, start or end.
    let r4 = &done.rows[2];
    assert_eq!((r4.platform.as_str(), r4.node.as_str()), ("-", "gone"));
    assert_eq!(
        (r4.shape.as_str(), r4.cfg.as_str(), r4.digest.as_str()),
        ("-", "-", "-")
    );
    assert_eq!(r4.memory_kill, "none", "absent means none");
    assert_eq!(
        (r4.cgroup.as_ref(), r4.started_us, r4.ended_us),
        (None, None, None)
    );
    assert_eq!(r4.node_capacity, None);

    // Every field a newer server adds is read when present.
    let r3 = &done.rows[3];
    assert_eq!(r3.platform, "macos");
    assert_eq!(r3.basis.as_deref(), Some("mac_footprint"));
    assert_eq!(r3.operation.as_deref(), Some("op-9"));
    assert_eq!(r3.booked_millicpus, Some(1000));
    assert_eq!(r3.node_cpu_pressure_centi, Some(1234));
    let u = r3.cgroup.as_ref().unwrap();
    assert_eq!(
        (
            u.memory_events_max,
            u.memory_events_oom_kill,
            u.memory_swap_peak_bytes,
            u.memory_anon_swap_peak_bytes,
            u.cpu_pressure_some_us
        ),
        (Some(2), Some(1), Some(7), Some(8), Some(9))
    );

    let seen = fake.seen.borrow();
    assert!(seen.iter().all(|(m, _)| m == "GET"), "{seen:?}");
    assert!(
        seen.iter()
            .any(|(_, p)| p == "/v1/invocations/b1/actions?page_size=1000&page_token=2"),
        "the second page is followed"
    );
    assert!(
        seen.iter()
            .any(|(_, p)| p == "/v1/invocations/b%2F2/actions?page_size=1000")
    );
    assert_eq!(
        seen.iter().filter(|(_, p)| p == "/v1/runs/r1").count(),
        1,
        "a run two builds list is read once"
    );
}

fn export_with(routes: Routes) -> Result<kbf_estimator_replay::export::Export, ExportError> {
    export(&ReadOnly::new(Fake::new(routes)), window())
}

fn base() -> Routes {
    let mut r = farm();
    put(&mut r, INVOCATIONS, &json!({"invocations": [{"id": "b1"}]}));
    r.insert(
        "/v1/invocations/b1/actions?page_size=1000".into(),
        (200, json!({"actions": []}).to_string()),
    );
    r
}

/// Catches an answer shape the walk reads wrongly and passes on as an empty export: each
/// of the shapes below must be an error naming the path, not zero rows.
#[test]
fn an_answer_the_walk_cannot_read_is_an_error_naming_the_path() {
    type Case = (&'static str, Box<dyn Fn(&mut Routes)>, &'static str);
    let cases: Vec<Case> = vec![
        (
            "invocation without id",
            Box::new(|r| put(r, INVOCATIONS, &json!({"invocations": [{}]}))),
            "an invocation has no id",
        ),
        (
            "invocations not an array",
            Box::new(|r| put(r, INVOCATIONS, &json!({"invocations": 3}))),
            "no `invocations` array",
        ),
        (
            "list is 404",
            Box::new(|r| {
                r.remove(INVOCATIONS);
            }),
            "no such list",
        ),
        (
            "node without id",
            Box::new(|r| put(r, "/v1/nodes", &json!({"nodes": [{}]}))),
            "a node has no id",
        ),
        (
            "nodes 404",
            Box::new(|r| {
                r.remove("/v1/nodes");
            }),
            "404",
        ),
        (
            "run without queued",
            Box::new(|r| {
                let mut run = run("r1", "n1", OCT_1, "ok", usage(1, 1));
                run.as_object_mut().unwrap().remove("queued");
                put(r, "/v1/runs/r1", &run);
                put(
                    r,
                    "/v1/invocations/b1/actions?page_size=1000",
                    &json!({"actions": [{"attempts": [{"run_id": "r1"}]}]}),
                );
            }),
            "no `queued`",
        ),
        (
            "run with a bad time",
            Box::new(|r| {
                let mut run = run("r1", "n1", OCT_1, "ok", usage(1, 1));
                run["started"] = "yesterday".into();
                put(r, "/v1/runs/r1", &run);
                put(
                    r,
                    "/v1/invocations/b1/actions?page_size=1000",
                    &json!({"actions": [{"attempts": [{"run_id": "r1"}]}]}),
                );
            }),
            "yesterday",
        ),
        (
            "run before the epoch",
            Box::new(|r| {
                let mut run = run("r1", "n1", OCT_1, "ok", usage(1, 1));
                run["queued"] = "1960-01-01T00:00:00Z".into();
                put(r, "/v1/runs/r1", &run);
                put(
                    r,
                    "/v1/invocations/b1/actions?page_size=1000",
                    &json!({"actions": [{"attempts": [{"run_id": "r1"}]}]}),
                );
            }),
            "before the epoch",
        ),
        (
            "not json",
            Box::new(|r| {
                r.insert("/v1/nodes".into(), (200, "<html>".into()));
            }),
            "not JSON",
        ),
    ];
    for (name, change, want) in cases {
        let mut routes = base();
        change(&mut routes);
        let err = export_with(routes).expect_err(name);
        assert!(matches!(err, ExportError::Shape { .. }), "{name}: {err:?}");
        assert!(err.to_string().contains(want), "{name}: {err}");
    }
}

/// Catches a refusal or an outage read as an empty history: a non-2xx other than 404 is
/// an error carrying the status and the start of the body.
#[test]
fn a_refusal_stops_the_export() {
    let mut routes = base();
    routes.insert(INVOCATIONS.into(), (403, "x".repeat(500)));
    let err = export_with(routes).expect_err("403");
    let ExportError::Status { status, body, path } = &err else {
        panic!("{err:?}");
    };
    assert_eq!((*status, body.len()), (403, 200));
    assert_eq!(path, INVOCATIONS);
    assert_eq!(err.to_string(), format!("GET {path}: status 403: {body}"));
}

/// Catches a walk that follows `next_page` for ever: a server whose list never ends is an
/// error.
#[test]
fn a_list_that_never_ends_is_an_error() {
    let mut routes = base();
    put(
        &mut routes,
        INVOCATIONS,
        &json!({"invocations": [], "next_page": INVOCATIONS}),
    );
    let err = export_with(routes).expect_err("endless");
    assert!(matches!(err, ExportError::Endless { .. }), "{err:?}");
    assert!(err.to_string().contains("more than"), "{err}");
}

/// Catches a transport failure reported as success, and a node whose capacity cannot be
/// read (a 404 for its detail) failing the export instead of leaving the capacity out.
#[test]
fn a_failed_call_is_an_error_and_a_missing_node_detail_is_not() {
    struct Down;
    impl Transport for Down {
        fn call(&self, _: &str, _: &str) -> Result<Reply, CallError> {
            Err(CallError::Transport("connection refused".into()))
        }
    }
    let err = export(&ReadOnly::new(Down), window()).expect_err("down");
    assert_eq!(err.to_string(), "connection refused");

    let mut routes = farm();
    routes.remove("/v1/nodes/n1");
    put(
        &mut routes,
        "/v1/nodes/mac1",
        &json!({"capacity": {"cpu_millis": 1}}),
    );
    let done = export_with(routes).unwrap();
    assert_eq!(done.rows.len(), 4);
    assert!(done.rows.iter().all(|r| r.node_capacity.is_none()));
    assert_eq!(
        done.rows[1].platform, "linux",
        "the platform still comes from the list"
    );
}

/// Catches a window that accepts what it cannot read or an end that is not after its start.
#[test]
fn a_window_is_two_times_in_order() {
    assert!(
        Window::parse("soon", UNTIL)
            .unwrap_err()
            .to_string()
            .contains("soon")
    );
    assert!(Window::parse(SINCE, "later").is_err());
    assert!(Window::parse("1960-01-01T00:00:00Z", UNTIL).is_err());
    let same = Window::parse(SINCE, SINCE).unwrap_err();
    assert!(same.to_string().contains("is not after"), "{same}");
    assert!(Window::parse(UNTIL, SINCE).is_err());
}

/// Catches a `next_page` from the server put in the request line unchecked: one holding a
/// line break (a second request), a space, or not under /v1/ is refused before anything is
/// sent for it.
#[test]
fn a_next_page_that_is_not_a_plain_api_path_is_refused() {
    for bad in [
        "/v1/x\r\nPOST /v1/nodes/n1:drain HTTP/1.1",
        "/v1/x\n",
        "/v1/a b",
        "http://elsewhere/v1/x",
        "/other",
    ] {
        let mut routes = base();
        put(
            &mut routes,
            INVOCATIONS,
            &json!({"invocations": [], "next_page": bad}),
        );
        let fake = Fake::new(routes);
        let err = export(&ReadOnly::new(&fake), window()).expect_err(bad);
        assert!(matches!(err, ExportError::Shape { .. }), "{bad:?}: {err:?}");
        assert!(err.to_string().contains("next_page"), "{err}");
        assert!(
            fake.seen
                .borrow()
                .iter()
                .all(|(m, p)| m == "GET" && p != bad),
            "{bad:?}"
        );
    }
}
