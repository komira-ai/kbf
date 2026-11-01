//! The wire shapes against the server's documented bodies in `tests/golden`: the client's
//! types lose nothing the server sends, send exactly what the server reads, and the fake
//! answers in the same fields.

mod support;

use std::time::Duration;

use kbf_lease_client::{
    AcquireRequest, Granted, Host, HostSource, HostState, Isolation, Lease, LeaseId, LeaseState,
    Mount, Outcome, PendingLease, ReleaseState, Released, Renewed, Resources, Run, RunAccepted,
    RunFile, RunId, RunLimits, RunSpec, RunState,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use support::{fake, golden, raw_json, shape, whole_host};

/// Reads a golden body as `T` and writes it back: the same JSON means `T` has a field for
/// everything the server sent and sends nothing the server did not.
fn round_trip<T: Serialize + DeserializeOwned>(name: &str) -> T {
    let body = golden(name);
    let typed: T = serde_json::from_value(body.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(serde_json::to_value(&typed).unwrap(), body, "{name}");
    typed
}

/// Asserts that every body the client sends is, field for field, a body in the server's
/// documented form, and that the builders make exactly those. It catches a renamed field, a
/// field sent when it should be left out (`gpu_class: null`), and a builder that sets a field
/// the fixture does not have.
#[test]
fn what_the_client_sends_is_what_the_server_reads() {
    let minimal: AcquireRequest = round_trip("acquire_request_minimal");
    let resources = Resources {
        cpu_millis: 16_000,
        memory_bytes: 64 << 30,
        ..Resources::default()
    };
    let built = AcquireRequest::new("kbf-linux", resources, Duration::from_secs(21_600));
    assert_eq!(built, minimal);

    let full: AcquireRequest = round_trip("acquire_request_full");
    let mut built = built
        .label("kbf-frontend", "fe-1")
        .label("kbf-pool", "main");
    built.resources.gpus = 1;
    built.resources.gpu_class = Some("a100".to_owned());
    built
        .capabilities
        .insert("os".to_owned(), "linux".to_owned());
    built
        .capabilities
        .insert("arch".to_owned(), "x86_64".to_owned());
    built.validation_run = Some("vr-7".to_owned());
    assert_eq!(built.ttl(Duration::from_secs(600)), full);

    let minimal: RunSpec = round_trip("run_request_minimal");
    let limits = RunLimits {
        cpu_millis: 4000,
        memory_bytes: 8 << 30,
        pids: None,
        wall_ms: None,
        disk_bytes: None,
    };
    assert_eq!(RunSpec::new(support::IMAGE, limits), minimal);
    let full: RunSpec = round_trip("run_request_full");
    assert_eq!(full.isolation, Isolation::Host);
    assert_eq!(full.limits.wall_ms, Some(1_800_000));
    assert_eq!(full.limits.disk_bytes, Some(10 << 30));
    assert_eq!(
        full.mounts,
        [Mount {
            root_name: "cas".to_owned(),
            rel: "b/b1".to_owned(),
            ro: true
        }]
    );
}

/// Asserts that a run's disk is optional on the wire in both directions: a body without
/// `disk_bytes` (the minimal golden, what an older client sends) reads as no disk and is
/// written back without the field, and the full golden carries it as the number the server
/// reads. It catches a client that sends `"disk_bytes": null` to a server that does not take
/// it, and one that drops the field from a body that has it.
#[test]
fn a_runs_disk_is_optional_on_the_wire() {
    let without: RunSpec = round_trip("run_request_minimal");
    assert_eq!(without.limits.disk_bytes, None);
    assert!(
        !serde_json::to_string(&without)
            .unwrap()
            .contains("disk_bytes")
    );
    let with: RunSpec = round_trip("run_request_full");
    assert_eq!(with.limits.disk_bytes, Some(10_737_418_240));
}

/// Asserts that the server's request bodies refuse an unknown field and so do the client's
/// types for them, at every level the server's types do. A field added to the client's
/// request that the server does not have would be a 400 on the farm; the types refuse it here.
#[test]
fn a_request_with_a_field_the_server_does_not_know_is_refused() {
    let cases: [(&str, &dyn Fn(Value) -> bool); 2] = [
        ("acquire_request_full", &|v| {
            serde_json::from_value::<AcquireRequest>(v).is_err()
        }),
        ("run_request_full", &|v| {
            serde_json::from_value::<RunSpec>(v).is_err()
        }),
    ];
    for (name, refused) in cases {
        let base = golden(name);
        let mut top = base.clone();
        top["unknown"] = json!(1);
        assert!(refused(top), "{name}: top level");
        let nested = if name.starts_with("acquire") {
            "resources"
        } else {
            "limits"
        };
        let mut inner = base.clone();
        inner[nested]["unknown"] = json!(1);
        assert!(refused(inner), "{name}: {nested}");
    }
}

/// Asserts the reading side of the formats rule: every response body round-trips, and a body
/// from a newer server (an extra field anywhere, a state or outcome nobody has heard of)
/// still reads, the unknown value as `Unknown`. It catches `deny_unknown_fields` on a response
/// type, which would fail every call the day the server adds a field, and a lost
/// `#[serde(other)]`.
#[test]
fn what_the_server_sends_reads_now_and_after_it_grows() {
    let granted: Granted = round_trip("granted");
    assert_eq!(granted.id, LeaseId::new(4, 17));
    let pending: PendingLease = round_trip("pending");
    assert_eq!(pending.state, LeaseState::Pending);
    let renewed: Renewed = round_trip("renewed");
    assert!(!renewed.host_reserved);
    let reserved: Renewed = round_trip("renewed_reserved");
    assert!(reserved.host_reserved);
    let freed: Released = round_trip("released_freed");
    assert_eq!(freed.state, ReleaseState::Freed);
    let releasing: Released = round_trip("released_releasing");
    assert_eq!(releasing.state, ReleaseState::Releasing);
    let _: RunAccepted = round_trip("run_accepted");
    let stopping: RunAccepted = round_trip("run_stopping");
    assert_eq!(stopping.state, RunState::Stopping);
    let exited: Run = round_trip("run_exited");
    assert_eq!(
        (exited.outcome, exited.exit_code),
        (Some(Outcome::Succeeded), Some(0))
    );
    let failed: Run = round_trip("run_failed_to_start");
    assert_eq!(failed.outcome, Some(Outcome::FailedToStart));
    assert_eq!(failed.detail.as_deref(), Some("image-pull-failed"));
    assert!(exited.labels.is_empty(), "a body with no labels has none");
    let labelled: Run = round_trip("run_labelled");
    assert_eq!(labelled.labels["kbf-descriptor"], "d-7f3a");

    // The wrapped lists are crate-private types; the elements are the public ones.
    let leases = golden("leases");
    let held: Lease = serde_json::from_value(leases["leases"][0].clone()).unwrap();
    assert_eq!(held.state, LeaseState::Held);
    assert_eq!(held.host.as_deref(), Some("node3"));
    let awaiting: Lease = serde_json::from_value(leases["leases"][1].clone()).unwrap();
    assert_eq!((awaiting.state, awaiting.host), (LeaseState::Pending, None));
    let hosts = golden("hosts_all_fields");
    let first: Host = serde_json::from_value(hosts["hosts"][0].clone()).unwrap();
    assert_eq!(first.source, HostSource::Static);
    assert!(!first.is_lost() && !first.is_tainted());
    let second: Host = serde_json::from_value(hosts["hosts"][1].clone()).unwrap();
    assert!(second.is_lost() && second.is_tainted());
    assert_eq!(second.state, Some(HostState::Lost));
    assert_eq!(
        second.taint.as_ref().unwrap().check.as_deref(),
        Some("scratch-clean")
    );
    // The server may send either field alone, and a live host's `lost: false` with
    // `state: "live"` is not lost; the two readings each carry the answer.
    let only_flag: Host = serde_json::from_value(hosts["hosts"][2].clone()).unwrap();
    assert!(only_flag.state.is_none() && only_flag.is_lost());
    let only_state: Host = serde_json::from_value(hosts["hosts"][3].clone()).unwrap();
    assert!(only_state.lost.is_none() && only_state.is_lost());
    let live: Host = serde_json::from_value(hosts["hosts"][4].clone()).unwrap();
    assert!(!live.is_lost());
    // The load is optional: the host with none reads as `None`, and the host with one keeps
    // every figure in the unit its name states.
    assert!(first.load.is_none() && second.load.is_none());
    let load = live.load.as_ref().expect("the live host lists a load");
    assert_eq!(
        (load.seq, load.mono_usec, load.cpu_usage_usec),
        (5, 61_000_000, 125_000_000)
    );
    assert_eq!(
        (load.memory_working_set_bytes, load.memory_active_file_bytes),
        (7_000_000_000, 250)
    );
    assert!(load.actions_cpu_limited);
    assert_eq!(load.covered, ["3-7"]);
    assert_eq!(load.lease_clocks[0].started_mono_usec, 500_000);
    assert_eq!(load.minutes[0].cpu_peak_window_millis, 2000);
    // A server that grows the load (a field this client has not heard of) is still read.
    let mut grown = hosts["hosts"][4].clone();
    grown["load"]["new_figure"] = json!(1);
    let grown: Host = serde_json::from_value(grown).unwrap();
    assert_eq!(grown.load.as_ref(), Some(load));
    // A load with only the figures that are always sent reads, its lists empty.
    let mut bare = hosts["hosts"][4].clone();
    for list in ["covered", "lease_clocks", "minutes"] {
        bare["load"].as_object_mut().unwrap().remove(list);
    }
    let bare: Host = serde_json::from_value(bare).unwrap();
    assert!(
        bare.load
            .is_some_and(|l| l.covered.is_empty() && l.minutes.is_empty())
    );
    for element in hosts["hosts"].as_array().unwrap() {
        let host: Host = serde_json::from_value(element.clone()).unwrap();
        assert_eq!(&serde_json::to_value(&host).unwrap(), element);
    }
    for element in leases["leases"].as_array().unwrap() {
        let lease: Lease = serde_json::from_value(element.clone()).unwrap();
        assert_eq!(&serde_json::to_value(&lease).unwrap(), element);
    }
    for element in golden("runs")["runs"].as_array().unwrap() {
        let run: Run = serde_json::from_value(element.clone()).unwrap();
        assert_eq!(&serde_json::to_value(&run).unwrap(), element);
    }

    // A server that grew.
    let mut newer = golden("granted");
    newer["priority"] = json!("high");
    assert!(serde_json::from_value::<Granted>(newer).is_ok());
    let mut newer = golden("run_exited");
    newer["state"] = json!("paused");
    newer["outcome"] = json!("evicted");
    newer["cgroup"] = json!({ "oom": true });
    let run: Run = serde_json::from_value(newer).unwrap();
    assert_eq!(
        (run.state, run.outcome),
        (RunState::Unknown, Some(Outcome::Unknown))
    );
    let mut newer = hosts["hosts"][1].clone();
    newer["source"] = json!("cloud");
    newer["state"] = json!("dozing");
    newer["gen"] = json!(3);
    let host: Host = serde_json::from_value(newer).unwrap();
    assert_eq!(host.source, HostSource::Unknown);
    assert_eq!(host.state, Some(HostState::Unknown));
    let mut newer = leases["leases"][0].clone();
    newer["state"] = json!("zombie");
    let lease: Lease = serde_json::from_value(newer).unwrap();
    assert_eq!(lease.state, LeaseState::Unknown);
    let released: Released =
        serde_json::from_value(json!({ "state": "queued-for-later" })).unwrap();
    assert_eq!(released.state, ReleaseState::Unknown);
}

/// Asserts that the fake answers each route with the fields of the server's documented body,
/// by fields and kinds. A fake that answers in a shape the server does not would make every
/// test above it pass against a contract that does not exist.
#[tokio::test]
async fn the_fake_answers_in_the_shape_of_the_golden_bodies() {
    let (server, client) = fake().await;
    let lease_body = serde_json::to_string(&whole_host().label("kbf-frontend", "fe-1")).unwrap();
    let (status, granted) = raw_json(&server, "POST", "/v1/leases", &lease_body).await;
    assert_eq!(status, 201);
    assert_eq!(shape(&granted), shape(&golden("granted")));
    let id = granted["id"].as_str().unwrap().to_owned();

    let (_, renewed) = raw_json(&server, "POST", &format!("/v1/leases/{id}/renew"), "{}").await;
    assert_eq!(shape(&renewed), shape(&golden("renewed")));
    let (status, bodyless) = raw_json(&server, "POST", &format!("/v1/leases/{id}/renew"), "").await;
    assert_eq!(status, 200, "the renew body may be left out");
    assert_eq!(shape(&bodyless), shape(&golden("renewed")));
    let ttl = golden("renew_request_ttl").to_string();
    let (status, with_ttl) =
        raw_json(&server, "POST", &format!("/v1/leases/{id}/renew"), &ttl).await;
    assert_eq!(status, 200, "the documented renew body is read");
    assert_eq!(shape(&with_ttl), shape(&golden("renewed")));
    let (_, leases) = raw_json(&server, "GET", "/v1/leases", "").await;
    assert_eq!(
        shape(&leases["leases"][0]),
        shape(&golden("leases")["leases"][0])
    );
    let (_, hosts) = raw_json(&server, "GET", "/v1/hosts", "").await;
    assert_eq!(
        shape(&hosts["hosts"][1]),
        shape(&golden("hosts_free")["hosts"][0])
    );

    let spec = serde_json::to_string(&support::run_of(4000, 8)).unwrap();
    let path = format!("/v1/leases/{id}/runs");
    let (status, queued) = raw_json(&server, "POST", &path, &spec).await;
    assert_eq!(status, 202);
    assert_eq!(shape(&queued), shape(&golden("run_accepted")));
    let run_id: RunId = queued["run_id"].as_str().unwrap().parse().unwrap();
    let (_, one) = raw_json(&server, "GET", &format!("{path}/{run_id}"), "").await;
    assert_eq!(shape(&one), shape(&golden("run_queued")));
    assert!(server.run_started(&run_id));
    assert!(server.finish_run(&run_id, Outcome::Succeeded, Some(0), None));
    let (_, exited) = raw_json(&server, "GET", &format!("{path}/{run_id}"), "").await;
    assert_eq!(shape(&exited), shape(&golden("run_exited")));
    let (_, list) = raw_json(&server, "GET", &path, "").await;
    assert_eq!(shape(&list["runs"][0]), shape(&golden("run_exited")));
    let (status, stop) = raw_json(&server, "DELETE", &format!("{path}/{run_id}"), "").await;
    assert_eq!(status, 202);
    assert_eq!(shape(&stop), shape(&golden("run_stopping")));

    let (status, releasing) = raw_json(&server, "DELETE", &format!("/v1/leases/{id}"), "").await;
    assert_eq!(status, 202);
    assert_eq!(releasing, golden("released_releasing"));
    assert!(server.end_run(&run_id));
    let (status, freed) = raw_json(&server, "DELETE", &format!("/v1/leases/{id}"), "").await;
    assert_eq!((status, freed), (404, golden("error_unknown_lease")));
    drop(client);
}

/// Asserts the error bodies of the fake: `{"error": code}`, a `reason` for a 400 that does not
/// repeat the body, 405 with no body for a known route with another method. A client that
/// reads errors by their code is only as good as the fake's codes.
#[tokio::test]
async fn the_fake_refuses_in_the_shape_of_the_golden_errors() {
    let (server, _client) = fake().await;
    let (status, bad) = raw_json(&server, "POST", "/v1/leases", "{\"secret-field\": 1}").await;
    assert_eq!(status, 400);
    assert_eq!(shape(&bad), shape(&golden("error_bad_request")));
    assert!(!bad.to_string().contains("secret-field"));
    let (status, bad) = raw_json(&server, "POST", "/v1/leases", "{").await;
    assert_eq!(status, 400);
    assert!(bad["reason"].as_str().unwrap().contains("not valid JSON"));

    let (status, no_room) = {
        let _ = raw_json(
            &server,
            "POST",
            "/v1/leases",
            &serde_json::to_string(&whole_host()).unwrap(),
        )
        .await;
        let _ = raw_json(
            &server,
            "POST",
            "/v1/leases",
            &serde_json::to_string(&whole_host()).unwrap(),
        )
        .await;
        raw_json(
            &server,
            "POST",
            "/v1/leases",
            &serde_json::to_string(&whole_host()).unwrap(),
        )
        .await
    };
    assert_eq!((status, no_room), (409, golden("error_no_capacity")));
    let (status, gone) = raw_json(&server, "GET", "/v1/leases/4-1/runs/run-4-9", "").await;
    assert_eq!((status, gone), (404, json!({ "error": "unknown-run" })));
    server.restart();
    let (status, forgotten) = raw_json(&server, "GET", "/v1/leases/4-1/runs/run-4-1", "").await;
    assert_eq!((status, forgotten), (410, golden("error_run_forgotten")));
    let (status, nothing) = raw_json(&server, "PUT", "/v1/leases", "").await;
    assert_eq!((status, nothing), (405, Value::Null));
    let (status, missing) = raw_json(&server, "GET", "/v2/leases", "").await;
    assert_eq!((status, missing), (404, json!({ "error": "not-found" })));
}

/// Asserts that a run without files is written as the bytes it always was: the minimal and
/// the full golden read, and write back byte for byte, with no `files` key. It catches a
/// field that is always serialised (an older lease server would answer 400 for it, and one
/// that knows it would see an empty list on every start).
#[test]
fn a_start_without_files_has_the_bytes_it_had_before() {
    for name in ["run_request_minimal", "run_request_full"] {
        let spec: RunSpec = round_trip(name);
        assert!(spec.files.is_empty(), "{name}");
        let path = format!("{}/tests/golden/{name}.json", env!("CARGO_MANIFEST_DIR"));
        let on_disk = std::fs::read_to_string(path).unwrap();
        assert_eq!(serde_json::to_string(&spec).unwrap(), on_disk.trim_end());
    }
}

/// Asserts that a run's files are sent as a list of `{name, contents}` in the order given and
/// read back the same, and that the typed body is exactly the golden one. It catches a
/// renamed field, a lost order, and a client that sends contents under another key.
#[test]
fn files_serialize_as_name_and_contents_and_round_trip() {
    let spec: RunSpec = round_trip("run_request_files");
    let names: Vec<&str> = spec.files.iter().map(|file| file.name.as_str()).collect();
    assert_eq!(names, ["put-url", "grant-token"]);
    assert_eq!(spec.files[1].contents, "tok-123");
    let mut built = RunSpec::new(
        support::IMAGE,
        RunLimits {
            cpu_millis: 4000,
            memory_bytes: 8 << 30,
            pids: None,
            wall_ms: None,
            disk_bytes: None,
        },
    );
    built.args = vec!["--build".to_owned(), "b1".to_owned()];
    built.files = vec![
        RunFile {
            name: "put-url".to_owned(),
            contents: "https://store.example/put?sig=abc".to_owned(),
        },
        RunFile {
            name: "grant-token".to_owned(),
            contents: "tok-123".to_owned(),
        },
    ];
    assert_eq!(built, spec);
}
