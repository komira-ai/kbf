//! Scenario B0 over a real farm, the runs that turn each gate red that tests/beta_b0.rs does
//! not: a run asked to stop, a member drained, a failed Execute, a frontend that stops
//! answering, a load of no builds, and the farms of one process told ports of their own. A gate
//! that is never seen red is not a gate: `support::RED_IN_A_SCENARIO` names the test for each.

#![cfg(target_os = "linux")]

#[path = "beta_support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::time::Duration;

use kbf_harness::b0::{Ended, run};
use kbf_harness::farm::{Bins, Farm, Role, Topology, control_socket};

use support::*;

/// Catches: a scan for key material that cannot find any (the next test reads "none found").
#[test]
fn the_scan_for_private_keys_finds_one() {
    let dir = scratch("scan");
    std::fs::create_dir_all(dir.join("a/b")).expect("dirs");
    std::fs::write(dir.join("a/b/leak.log"), "x -----BEGIN PRIVATE KEY----- y").expect("file");
    std::fs::write(dir.join("a/ok.log"), "nothing").expect("file");
    assert_eq!(
        files_containing(&dir, "PRIVATE KEY"),
        [dir.join("a/b/leak.log")]
    );
}

/// Catches: a run that is told to stop (SIGTERM, SIGINT: a CI job's cancel or timeout) going
/// on for the rest of its load, or dying at once with the farm, the Buck2 daemons and the
/// document left behind: it stops the farm (every process ended and reaped), kills the daemon
/// of every loop, and ends `FAILED` with the reason, long before its load would have.
#[test]
fn a_run_that_is_asked_to_stop_ends_failed_with_nothing_left_running() {
    let args = b0("interrupted", "ok", 100);
    let (work, interrupt) = (args.work.clone(), args.interrupt.clone());
    let asker = std::thread::spawn(move || {
        wait_for_the_load(&work);
        interrupt.set();
    });
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    asker.join().expect("the asker");
    let doc = document(&args);
    assert_eq!(doc["status"], "FAILED");
    assert!(
        doc["error"]
            .as_str()
            .expect("error")
            .starts_with("interrupted by a signal: the farm was stopped"),
        "{doc}"
    );
    let table = doc["processes"].as_array().expect("processes");
    assert_eq!(table.len(), 6, "{table:?}");
    for row in table {
        assert!(row["ended"].is_string(), "{row}");
        let pid = row["pid"].as_u64().expect("pid");
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "{row}"
        );
    }
    for lane in 0..2 {
        assert!(
            !kill_markers(&args.work, lane).is_empty(),
            "loop {lane}'s daemon was not killed"
        );
    }
    assert!(
        doc["duration_secs"].as_f64().expect("duration") < 90.0,
        "{doc}"
    );
}

/// Catches: a run asked to stop while it only waits for what is in flight to be counted (the
/// settle window after the load) going on to the end of that wait and then judging: it ends
/// `FAILED` as one stopped during the load does. The load is two seconds and the settle two
/// minutes; the asker waits for both loops to end (each kills its daemon as it does), which
/// leaves the run in its settle window, and a run that did not look at the flag there would
/// be `PASSED` two minutes later.
#[test]
fn a_run_asked_to_stop_while_it_settles_ends_failed() {
    let mut args = b0("interrupted-settle", "ok", 2);
    args.settle = Duration::from_secs(120);
    let (work, interrupt) = (args.work.clone(), args.interrupt.clone());
    let asker = std::thread::spawn(move || {
        let until = std::time::Instant::now() + Duration::from_secs(120);
        while !(0..2).all(|lane| !kill_markers(&work, lane).is_empty()) {
            assert!(std::time::Instant::now() < until, "the loops never ended");
            std::thread::sleep(Duration::from_millis(50));
        }
        interrupt.set();
    });
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    asker.join().expect("the asker");
    let doc = document(&args);
    assert_eq!(doc["status"], "FAILED");
    assert!(
        doc["error"]
            .as_str()
            .expect("error")
            .starts_with("interrupted by a signal"),
        "{doc}"
    );
}

/// Catches: `farm_whole` that cannot go red: a storage member an operator drained is alive but
/// no longer an active member, and the farm is not whole; nothing else is wrong with the run.
#[test]
fn a_member_that_is_drained_fails_the_whole_farm_gate_and_only_it() {
    let args = b0("not-whole", "ok", 6);
    let work = args.work.clone();
    let operator = std::thread::spawn(move || {
        wait_for_the_load(&work);
        let (_, api) = frontend_ports(&work);
        let (_, text) = kbf_harness::api::get(api, "/v1/members").expect("members");
        let view: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        let id = view["members"][0]["id"].as_u64().expect("an id");
        assert_eq!(
            post(api, &format!("/v1/members/{id}"), r#"{"state":"draining"}"#),
            200
        );
    });
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    operator.join().expect("the operator");
    let doc = document(&args);
    assert_eq!(failed(&doc), ["farm_whole"], "{doc}");
}

/// Catches: `execute_errors` that cannot go red: an Execute of an action nobody uploaded ends in
/// an error status and is counted; the delta over the run is not zero.
#[test]
fn an_execute_that_fails_fails_the_execute_error_gate() {
    use kbf_proto::reapi::execution_client::ExecutionClient;
    use kbf_proto::reapi::{Digest, ExecuteRequest};
    use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};

    let args = b0("execute-error", "ok", 6);
    let work = args.work.clone();
    let client = std::thread::spawn(move || {
        wait_for_the_load(&work);
        let (reapi, _) = frontend_ports(&work);
        let ca = std::fs::read(work.join("farm/frontend/etc/ca.pem")).expect("the CA");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let tls = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(ca))
                .domain_name("localhost");
            let channel = Endpoint::from_shared(format!("https://127.0.0.1:{reapi}"))
                .expect("endpoint")
                .tls_config(tls)
                .expect("TLS")
                .connect()
                .await
                .expect("connect");
            let missing = Digest {
                hash: "0".repeat(64),
                size_bytes: 0,
            };
            let request = ExecuteRequest {
                action_digest: Some(missing),
                ..Default::default()
            };
            let mut stream = ExecutionClient::new(channel)
                .execute(request)
                .await
                .map(tonic::Response::into_inner);
            // The failure is the call's status or the stream's, whichever the server uses.
            let failed = match stream.as_mut() {
                Ok(stream) => stream.message().await.is_err(),
                Err(_) => true,
            };
            assert!(
                failed,
                "an Execute of an action nobody uploaded did not fail"
            );
        });
    });
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    client.join().expect("the client");
    let doc = document(&args);
    assert_eq!(failed(&doc), ["execute_errors"], "{doc}");
}

/// Catches: `scrape_coverage` that cannot go red: a frontend that does not answer for most of
/// the load (stopped, as a wedged one would be, for longer than a scrape waits) leaves the
/// scraper without pages, and the gate says so.
#[test]
fn a_frontend_that_stops_answering_fails_the_scrape_coverage_gate() {
    let args = b0("coverage", "ok", 14);
    let work = args.work.clone();
    let wedger = std::thread::spawn(move || {
        wait_for_the_load(&work);
        let stopped = Stopped::new(pid_of(&work, "frontend"));
        std::thread::sleep(Duration::from_secs(11));
        drop(stopped);
    });
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    wedger.join().expect("the wedger");
    let doc = document(&args);
    assert_eq!(failed(&doc), ["scrape_coverage"], "{doc}");
}

/// Catches: `builds_run` that cannot go red: a load of no time runs no builds.
#[test]
fn a_load_that_runs_no_build_fails_the_builds_run_gate() {
    let args = b0("no-builds", "ok", 0);
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    let doc = document(&args);
    // A load of no time also scrapes nothing, so the coverage of the scrapes is red with it.
    assert_eq!(failed(&doc), ["builds_run", "scrape_coverage"], "{doc}");
}

/// Catches: two farms started together in one process (parallel tests, a harness running two
/// scenarios) told the same ports: each is given a block of ports of its own, every port its
/// processes were told lies in it, the frontend's units name them, and the blocks do not
/// overlap. The old probe-and-close ports could give both a port. Also, that the control socket
/// the frontend is told is the one the sentinel removes after a SIGKILL.
///
/// Each farm is started on a thread of its own, which stays until the checks are done: a
/// process dies when the thread that started it ends (the parent-death signal), so a thread
/// that returned the farm and ended would have the roles killed under the checks.
#[test]
fn farms_started_together_in_one_process_are_told_ports_of_their_own() {
    let _slots = (take_slot(), take_slot());
    let start = |name: &str| {
        let dir = scratch(name);
        let bins = Bins {
            kbf: PathBuf::from(env!("CARGO_BIN_EXE_kbf")),
            fake_crun: None,
        };
        let (farm_tx, farm_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let farm_dir = dir.join("farm");
        let thread = std::thread::spawn(move || {
            let farm = Farm::start(
                &dir.join("logs"),
                &dir.join("farm"),
                &bins,
                &Topology::beta(None),
            )
            .expect("a farm starts");
            farm_tx.send(farm).expect("the farm is taken");
            // Until the test is done with the farm (the sender is dropped).
            let _ = done_rx.recv();
        });
        (farm_rx, farm_dir, done_tx, thread)
    };
    let (a_rx, a_dir, a_done, a_thread) = start("ports-a");
    let (b_rx, b_dir, b_done, b_thread) = start("ports-b");
    let (a, b) = (a_rx.recv().expect("a farm"), b_rx.recv().expect("a farm"));
    let (ports_a, block_a) = a.ports().expect("a started farm has ports");
    let (ports_b, block_b) = b.ports().expect("a started farm has ports");
    assert!(
        block_a.end <= block_b.start || block_b.end <= block_a.start,
        "{block_a:?} {block_b:?}"
    );
    for (ports, block, farm, dir) in [
        (ports_a, &block_a, &a, &a_dir),
        (ports_b, &block_b, &b, &b_dir),
    ] {
        assert!(
            ports.all().iter().all(|p| block.contains(p)),
            "{ports:?} {block:?}"
        );
        let frontend = farm.spec(Role::Frontend).argv.join(" ");
        assert!(
            frontend.contains(&format!(":{}", ports.front.0)),
            "{frontend}"
        );
        assert!(
            frontend.contains(&format!(
                "--handover-socket={}",
                control_socket(dir).display()
            )),
            "{frontend}"
        );
        let node = farm.spec(Role::Node(1)).argv.join(" ");
        assert!(
            node.contains(&format!("--metrics-listen=127.0.0.1:{}", ports.metrics[0])),
            "{node}"
        );
    }
    assert_ne!(control_socket(&a_dir), control_socket(&b_dir));
    drop((a, b));
    drop((a_done, b_done));
    a_thread.join().expect("a thread");
    b_thread.join().expect("b thread");
}
