//! `noop` against fake frontends in this process (tests/support/front.rs): the actions it
//! sends, the order and the window it sends them in, what each run records, each way a run
//! fails, and the failures that stop a benchmark. Times are tokio's paused clock: no test
//! reads the wall clock or bounds a time.

mod support;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_bench::connect::{self, Tls};
use kbf_bench::noop::{self, Arm, NoopError, Phase, Plan, Record, Summary};
use kbf_bench::program::{self, Arch};
use kbf_proto::google::rpc;
use kbf_proto::reapi::{ActionResult, ExecuteResponse, ExecutedActionMetadata, platform};
use prost_types::Timestamp;
use support::front::{self, Reply, Shared, Upload, exited};
use tokio::sync::Barrier;
use tonic::Status;

fn plan(serial_runs: u64, parallel_runs: u64, in_flight: usize, block: u64) -> Plan {
    Plan {
        instance_name: "main".to_owned(),
        platform: Vec::new(),
        arch: Arch::X86_64,
        seed: 9,
        serial_runs,
        parallel_runs,
        in_flight,
        block,
    }
}

fn arm(name: &str, addr: SocketAddr) -> Arm {
    Arm {
        name: name.to_owned(),
        channel: connect::channel_to(format!("http://{addr}"), &Tls::default()).expect("channel"),
    }
}

/// Runs `plan` against `arms`; returns the summary and the records written.
async fn bench(plan: &Plan, arms: &[Arm]) -> (Summary, Vec<Record>, Vec<serde_json::Value>) {
    let mut out = Vec::new();
    let summary = noop::run(plan, arms, &mut out).await.expect("run");
    let text = String::from_utf8(out).expect("UTF-8");
    assert!(text.ends_with('\n'), "{text:?}");
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .collect();
    let records = lines.iter().map(record).collect();
    (summary, records, lines)
}

/// A record read back from its JSON line: every field there, under its name.
fn record(line: &serde_json::Value) -> Record {
    let text = |k: &str| line[k].as_str().map(str::to_owned);
    let num = |k: &str| line[k].as_f64();
    let int = |k: &str| line[k].as_u64().expect(k);
    let phase = match line["phase"].as_str().expect("phase") {
        "serial" => Phase::Serial,
        "parallel" => Phase::Parallel,
        other => panic!("phase {other}"),
    };
    assert_eq!(line.as_object().expect("an object").len(), 19, "{line}");
    Record {
        arm: text("arm").expect("arm"),
        phase,
        in_flight: int("in_flight") as usize,
        open: int("open") as usize,
        block: int("block"),
        run: int("run"),
        action: text("action").expect("action"),
        operation: text("operation"),
        execute_to_done_ms: num("execute_to_done_ms"),
        ok: line["ok"].as_bool().expect("ok"),
        error: text("error"),
        exit_code: line["exit_code"].as_i64().map(|c| c as i32),
        worker: text("worker"),
        claimed: line["claimed"].as_bool().expect("claimed"),
        queued_ms: num("queued_ms"),
        worker_ms: num("worker_ms"),
        input_fetch_ms: num("input_fetch_ms"),
        execution_ms: num("execution_ms"),
        output_upload_ms: num("output_upload_ms"),
    }
}

fn ok() -> Reply {
    Reply::Done(exited(0))
}

/// Catches runs sent out of their alternating blocks, an action shared by two runs (a
/// stale digest a frontend could deduplicate), `skip_cache_lookup` or the instance name
/// not sent, the Command not `./noop`, the program not executable or not the program,
/// the platform unsorted, and a record that does not say which run it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_arms_alternate_by_block_and_every_run_is_a_new_action() {
    let shared = Arc::new(Shared::default());
    let a = front::start("a", &shared, Upload::Keep, |_| ok());
    let b = front::start("b", &shared, Upload::Keep, |_| ok());
    let mut plan = plan(5, 4, 2, 2);
    plan.platform = vec![
        ("arch".to_owned(), "x86_64".to_owned()),
        ("OSFamily".to_owned(), "linux".to_owned()),
        ("OSFamily".to_owned(), "Linux".to_owned()),
    ];
    let (summary, records, _) = bench(&plan, &[arm("a", a), arm("b", b)]).await;

    let ran = shared.ran.lock().expect("ran").clone();
    let serial: Vec<String> = ran[..10].iter().map(|r| r.env[1..].join("-")).collect();
    assert_eq!(
        serial,
        [
            "a-serial-0",
            "a-serial-1",
            "b-serial-0",
            "b-serial-1",
            "a-serial-2",
            "a-serial-3",
            "b-serial-2",
            "b-serial-3",
            "a-serial-4",
            "b-serial-4",
        ]
    );
    let parallel: BTreeSet<String> = ran[10..].iter().map(|r| r.env[1..].join("-")).collect();
    let expected: BTreeSet<String> = ["a", "b"]
        .iter()
        .flat_map(|arm| (0..4).map(move |i| format!("{arm}-parallel-{i}")))
        .collect();
    assert_eq!(parallel, expected);
    let mut actions = BTreeSet::new();
    for r in &ran {
        assert_eq!(r.env[0], "9", "the seed");
        assert_eq!(r.env[1], r.arm, "sent to its own arm");
        assert!(r.request.skip_cache_lookup);
        assert_eq!(r.request.instance_name, "main");
        assert_eq!(r.command.arguments, ["./noop"]);
        assert_eq!(r.command.environment_variables.len(), 1);
        assert!(r.command.output_paths.is_empty());
        assert_eq!(r.program, program::noop(Arch::X86_64));
        assert_eq!(r.root.files.len(), 1);
        assert!(r.root.files[0].is_executable);
        assert!(r.root.directories.is_empty() & r.root.symlinks.is_empty());
        let properties = &r.action.platform.as_ref().expect("platform").properties;
        let pairs: Vec<(&str, &str)> = properties
            .iter()
            .map(|p: &platform::Property| (p.name.as_str(), p.value.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("OSFamily", "Linux"),
                ("OSFamily", "linux"),
                ("arch", "x86_64")
            ]
        );
        assert!(!r.action.do_not_cache);
        actions.insert(r.request.action_digest.clone().expect("digest").hash);
    }
    assert_eq!(actions.len(), 18, "every run its own action");
    assert_eq!(
        *shared.uploads.lock().expect("uploads"),
        [("a".to_owned(), 1), ("b".to_owned(), 1)].into()
    );

    assert_eq!(records.len(), 18);
    for r in &records {
        assert!(r.ok, "{r:?}");
        assert_eq!(r.error, None);
        assert_eq!(r.exit_code, Some(0));
        let phase = match r.phase {
            Phase::Serial => "serial",
            Phase::Parallel => "parallel",
        };
        assert_eq!(
            r.operation.as_deref(),
            Some(format!("operations/{}-{phase}-{}", r.arm, r.run).as_str())
        );
        assert_eq!(r.block, r.run / 2);
        assert_eq!(r.in_flight, if r.phase == Phase::Serial { 1 } else { 2 });
        assert!(r.execute_to_done_ms.is_some());
        let digest = &ran
            .iter()
            .find(|x| x.env[1..].join("-") == format!("{}-{phase}-{}", r.arm, r.run))
            .expect("ran")
            .request
            .action_digest
            .clone()
            .expect("digest");
        assert_eq!(r.action, format!("{}/{}", digest.hash, digest.size_bytes));
        // No metadata in the fake's results: the worker is empty and no phase is known.
        assert_eq!(r.worker.as_deref(), Some(""));
        assert!(!r.claimed);
        assert_eq!(
            [
                r.queued_ms,
                r.worker_ms,
                r.input_fetch_ms,
                r.execution_ms,
                r.output_upload_ms
            ],
            [None; 5]
        );
    }
    assert!(
        records
            .iter()
            .filter(|r| r.phase == Phase::Serial)
            .all(|r| r.open == 1)
    );

    assert!(summary.complete());
    assert_eq!(summary.failed, 0);
    let rows: Vec<(&str, Phase, u64, u64, Option<u64>)> = summary
        .rows
        .iter()
        .map(|r| {
            (
                r.arm.as_str(),
                r.phase,
                r.runs,
                r.failed,
                r.latency.as_ref().map(|l| l.count),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("a", Phase::Serial, 5, 0, Some(5)),
            ("a", Phase::Parallel, 4, 0, Some(4)),
            ("b", Phase::Serial, 5, 0, Some(5)),
            ("b", Phase::Parallel, 4, 0, Some(4)),
        ]
    );
    let table = summary.table();
    let mut lines = table.lines();
    assert_eq!(
        lines.next(),
        Some("noop x86_64, seed 9, blocks of 2, parallel 2 in flight")
    );
    assert_eq!(
        lines.next(),
        Some("arm         phase        runs  failed     p50 ms     p90 ms     p99 ms     max ms")
    );
    assert!(
        lines
            .next()
            .expect("a row")
            .starts_with("a           serial          5       0 ")
    );
    assert_eq!(table.lines().last(), Some("failed: 0"));
}

/// Catches the serial phase sending a run before the last one is done, and the parallel
/// window not filled across the arms' blocks (a barrier at each block's end, or a window
/// per arm): the fake holds every Execute until four are open at once, so a window that
/// never reaches four hangs the test, and one past four is seen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_runs_never_overlap_and_the_parallel_window_spans_the_arms() {
    let shared = Arc::new(Shared::default());
    let a = front::start("a", &shared, Upload::Keep, |_| ok());
    let b = front::start("b", &shared, Upload::Keep, |_| ok());
    let arms = [arm("a", a), arm("b", b)];
    let (_, records, _) = bench(&plan(6, 0, 4, 2), &arms).await;
    assert_eq!(records.len(), 12);
    assert!(
        records
            .iter()
            .all(|r| (r.open == 1) & (r.phase == Phase::Serial))
    );
    assert_eq!(shared.peak.load(std::sync::atomic::Ordering::SeqCst), 1);

    let shared = Arc::new(Shared::default());
    *shared.gate.lock().expect("gate") = Some(Arc::new(Barrier::new(4)));
    let a = front::start("a", &shared, Upload::Keep, |_| ok());
    let b = front::start("b", &shared, Upload::Keep, |_| ok());
    let arms = [arm("a", a), arm("b", b)];
    // Blocks of 1: four runs open at once are two blocks of each arm.
    let (summary, records, _) = bench(&plan(0, 4, 4, 1), &arms).await;
    assert!(summary.complete(), "{}", summary.table());
    assert_eq!(shared.peak.load(std::sync::atomic::Ordering::SeqCst), 4);
    assert_eq!(records.len(), 8);
    assert!(
        records
            .iter()
            .all(|r| (r.phase == Phase::Parallel) & (r.in_flight == 4))
    );
    let most = records.iter().map(|r| r.open).max();
    assert_eq!(most, Some(4));
    let first_wave: BTreeSet<(String, u64)> = shared.ran.lock().expect("ran")[..4]
        .iter()
        .map(|r| (r.arm.clone(), r.env[3].parse().expect("run")))
        .collect();
    assert_eq!(
        first_wave,
        [("a", 0), ("a", 1), ("b", 0), ("b", 1)]
            .map(|(arm, run)| (arm.to_owned(), run))
            .into()
    );
    assert_eq!(summary.rows.len(), 2, "no row for a phase of no runs");
}

fn at(seconds: i64, nanos: i32) -> Option<Timestamp> {
    Some(Timestamp { seconds, nanos })
}

/// Catches the time measured from anything but the send to the done operation (the first
/// update, the stream's opening), and a phase of the result's metadata read from the wrong
/// two stamps, made up when a stamp is missing, negative when they are out of order, or
/// wrong across a second's boundary; the task's claim not told from a daemon's stamps.
#[tokio::test(start_paused = true)]
async fn a_run_records_execute_to_done_and_the_phases_its_result_carries() {
    let mut stamped = ExecutedActionMetadata {
        worker: "node-3".to_owned(),
        queued_timestamp: at(100, 0),
        worker_start_timestamp: at(100, 500_000_000),
        worker_completed_timestamp: at(101, 250_000_000),
        input_fetch_start_timestamp: at(100, 900_000_000),
        input_fetch_completed_timestamp: at(101, 100_000_000),
        execution_start_timestamp: at(101, 100_000_000),
        execution_completed_timestamp: at(101, 110_000_000),
        output_upload_start_timestamp: at(101, 120_000_000),
        ..ExecutedActionMetadata::default()
    };
    kbf_proto::task_claim::mark(&mut stamped);
    let reversed = ExecutedActionMetadata {
        worker_start_timestamp: at(7, 0),
        worker_completed_timestamp: at(6, 999_999_999),
        execution_completed_timestamp: at(7, 0),
        ..ExecutedActionMetadata::default()
    };
    let with = |metadata: ExecutedActionMetadata| ExecuteResponse {
        result: Some(ActionResult {
            execution_metadata: Some(metadata),
            ..ActionResult::default()
        }),
        ..ExecuteResponse::default()
    };
    let (stamped, reversed) = (with(stamped), with(reversed));
    let shared = Arc::new(Shared::default());
    let a = front::start("a", &shared, Upload::Keep, move |ran| {
        match ran.env[3].as_str() {
            "0" => Reply::DoneAfter(stamped.clone(), Duration::from_millis(250)),
            _ => Reply::DoneAfter(reversed.clone(), Duration::from_millis(1_500)),
        }
    });
    let (summary, records, lines) = bench(&plan(2, 0, 1, 1), &[arm("a", a)]).await;

    let first = &records[0];
    assert_eq!(first.execute_to_done_ms, Some(250.0));
    assert_eq!(first.worker.as_deref(), Some("node-3"));
    assert!(first.claimed);
    assert_eq!(first.queued_ms, Some(500.0));
    assert_eq!(first.worker_ms, Some(750.0));
    assert_eq!(first.input_fetch_ms, Some(200.0));
    assert_eq!(first.execution_ms, Some(10.0));
    assert_eq!(first.output_upload_ms, None, "no end stamp");
    assert_eq!(lines[0]["output_upload_ms"], serde_json::Value::Null);

    let second = &records[1];
    assert_eq!(second.execute_to_done_ms, Some(1_500.0));
    assert!(!second.claimed);
    assert_eq!(second.worker_ms, None, "ends before it starts");
    assert_eq!(second.execution_ms, None, "no start stamp");
    assert_eq!(second.queued_ms, None);

    let latency = summary.rows[0].latency.clone().expect("latency");
    assert_eq!((latency.p50_ms, latency.max_ms), (250.0, 1_500.0));
    assert!(
        summary.table().contains(
            "\na           serial          2       0    250.000   1500.000   1500.000   1500.000\n"
        ),
        "{}",
        summary.table()
    );
}

/// Catches a failed run counted as a success, or recorded without saying how it failed:
/// each way an Execute can fail has its own message, compared whole; a run with no done
/// operation has no time; the first ten failures are kept, all are counted; a phase whose
/// every run failed has no percentiles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_way_a_run_fails_is_recorded_by_name() {
    let shared = Arc::new(Shared::default());
    let a = front::start("a", &shared, Upload::Keep, |ran| {
        if ran.env[2] == "parallel" {
            return Reply::Refuse(Status::unavailable("draining"));
        }
        match ran.env[3].parse::<u32>().expect("run") {
            0 => Reply::Refuse(Status::unavailable("draining")),
            1 => Reply::Break(Status::internal("cut")),
            2 => Reply::End,
            3 => Reply::Failed(rpc::Status {
                code: 4,
                message: "deadline".to_owned(),
                ..rpc::Status::default()
            }),
            4 => Reply::NoResult,
            5 => Reply::Garbage,
            6 => Reply::Done(ExecuteResponse {
                status: Some(rpc::Status {
                    code: 9,
                    message: "no worker".to_owned(),
                    ..rpc::Status::default()
                }),
                ..exited(0)
            }),
            7 => Reply::Done(ExecuteResponse::default()),
            8 => Reply::Done(ExecuteResponse {
                cached_result: true,
                ..exited(0)
            }),
            9 => Reply::Done(exited(3)),
            10 => ok(),
            _ => Reply::Done(exited(1)),
        }
    });
    let (summary, records, _) = bench(&plan(12, 1, 1, 20), &[arm("a", a)]).await;

    let serial: Vec<&Record> = records
        .iter()
        .filter(|r| r.phase == Phase::Serial)
        .collect();
    let errors: Vec<Option<&str>> = serial.iter().map(|r| r.error.as_deref()).collect();
    assert_eq!(
        errors,
        [
            Some("Execute: Unavailable: draining"),
            Some("the Execute stream: Internal: cut"),
            Some("the Execute stream ended before the operation was done"),
            Some("the operation failed: code 4: deadline"),
            Some("the done operation has no result"),
            Some(
                "the done operation's response is not an ExecuteResponse: failed to decode \
                 Protobuf message: invalid varint"
            ),
            Some("the ExecuteResponse's status: code 9: no worker"),
            Some("the ExecuteResponse has no ActionResult"),
            Some("answered from the action cache, though the Execute set skip_cache_lookup"),
            Some("./noop exited 3"),
            None,
            Some("./noop exited 1"),
        ]
    );
    let oks: Vec<bool> = serial.iter().map(|r| r.ok).collect();
    assert_eq!(
        oks,
        [
            false, false, false, false, false, false, false, false, false, false, true, false
        ]
    );
    let timed: Vec<bool> = serial
        .iter()
        .map(|r| r.execute_to_done_ms.is_some())
        .collect();
    assert_eq!(
        timed,
        [
            false, false, false, true, true, true, true, true, true, true, true, true
        ]
    );
    assert_eq!(serial[0].operation, None, "refused before any update");
    assert_eq!(
        serial[1].operation.as_deref(),
        Some("operations/a-serial-1")
    );
    let exits: Vec<Option<i32>> = serial.iter().map(|r| r.exit_code).collect();
    assert_eq!(
        exits,
        [
            None,
            None,
            None,
            None,
            None,
            None,
            Some(0),
            None,
            Some(0),
            Some(3),
            Some(0),
            Some(1)
        ]
    );

    assert!(!summary.complete());
    assert_eq!(summary.failed, 12);
    assert_eq!(summary.errors.len(), 10);
    assert_eq!(
        summary.errors[0],
        "a serial run 0: Execute: Unavailable: draining"
    );
    assert_eq!(summary.errors[9], "a serial run 9: ./noop exited 3");
    let table = summary.table();
    assert!(
        table.contains("\na           serial         12      11 "),
        "{table}"
    );
    assert!(
        table.contains(
            "\na           parallel        1       1          -          -          -          -\n"
        ),
        "{table}"
    );
    assert!(
        table.contains("\nfailed: 12\n  a serial run 0: Execute: Unavailable: draining\n"),
        "{table}"
    );
}

/// Catches an arm's refusal of the blobs ignored (every Execute would then fail as a
/// missing action, timed as if it ran), and the blobs sent in one call past the size the
/// frontend takes: 250 runs are 502 blobs, two calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_arm_that_refuses_the_blobs_stops_the_run_before_any_execute() {
    let shared = Arc::new(Shared::default());
    let a = front::start("a", &shared, Upload::Keep, |_| ok());
    let (summary, records, _) = bench(&plan(250, 0, 1, 250), &[arm("a", a)]).await;
    assert!(summary.complete());
    assert_eq!(records.len(), 250);
    assert_eq!(shared.uploads.lock().expect("uploads")["a"], 2);

    for (upload, why) in [
        (
            Upload::RefuseCall,
            "BatchUpdateBlobs: ResourceExhausted: the fake is full".to_owned(),
        ),
        (Upload::RefuseLast, String::new()),
    ] {
        let shared = Arc::new(Shared::default());
        let good = front::start("good", &shared, Upload::Keep, |_| ok());
        let bad = front::start("bad", &shared, upload, |_| ok());
        let mut out = Vec::new();
        let arms = [arm("good", good), arm("bad", bad)];
        let refused = noop::run(&plan(250, 0, 1, 20), &arms, &mut out)
            .await
            .expect_err("refused");
        let why = match upload {
            Upload::RefuseLast => format!(
                "BatchUpdateBlobs refused {}: code 3: too odd",
                shared
                    .refused
                    .lock()
                    .expect("refused")
                    .clone()
                    .expect("a refused blob")
            ),
            _ => why,
        };
        assert!(matches!(refused, NoopError::Upload { .. }));
        assert_eq!(
            refused.to_string(),
            format!("sending the actions to arm bad: {why}")
        );
        assert!(shared.ran.lock().expect("ran").is_empty(), "no Execute");
        assert!(out.is_empty());
        assert_eq!(shared.uploads.lock().expect("uploads")["bad"], 1);
    }
}

/// A sink that fails its writes, or its flushes.
struct Broken {
    flush_only: bool,
}

impl std::io::Write for Broken {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.flush_only {
            return Ok(buf.len());
        }
        Err(std::io::Error::other("disk full"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("cannot flush"))
    }
}

/// Catches a record lost without a word: a run whose line cannot be written, or flushed,
/// stops the benchmark with the cause.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_record_that_cannot_be_written_stops_the_run() {
    let shared = Arc::new(Shared::default());
    let a = front::start("a", &shared, Upload::Keep, |_| ok());
    let arms = [arm("a", a)];
    for (flush_only, why) in [
        (false, "--jsonl: disk full"),
        (true, "--jsonl: cannot flush"),
    ] {
        let mut out = Broken { flush_only };
        let stopped = noop::run(&plan(3, 0, 1, 1), &arms, &mut out)
            .await
            .expect_err("stopped");
        assert!(matches!(stopped, NoopError::Write(_)));
        assert_eq!(stopped.to_string(), why);
    }
    assert_eq!(
        shared.ran.lock().expect("ran").len(),
        2,
        "one run each, then the stop"
    );
}
