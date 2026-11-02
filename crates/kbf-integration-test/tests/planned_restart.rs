//! A planned restart of a loaded node, through a multi-node farm of real `kbf` processes.
//!
//! The test spans components (the frontend's scheduler and operator API, the storage nodes
//! and their daemon halves, the stand-in `crun`), so it lives here and not in `kbf-cli`.
//! The farm is `kbf_harness::farm::Farm`, the REAPI client of its frontend is
//! `kbf_harness::reapi`, the action's input is a `kbf_testkit::tree::Tree`.
//!
//! The processes are found beside the test binary (`kbf`, and the stand-in `crun` that
//! runs each action as `kbf-act`, a link to this test binary, answered by the hook below).

// Not under coverage: the pull-request gate does not run this test (it is on the slow list,
// .config/nextest.toml; the beta workflow runs it), and its unrun lines would count against
// the 99% of the crate.
#![cfg(all(target_os = "linux", not(coverage)))]

use std::path::PathBuf;
use std::sync::Arc;

use kbf_harness::farm::{Bins, Farm, Role, Topology};
use kbf_harness::node_support::scratch_root;
use kbf_harness::procs::HANG;
use kbf_harness::reapi::{connect, upload_action};
use kbf_harness::stand_in::sibling_bin;
use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{Digest, ExecuteRequest, ExecuteResponse};
use kbf_testkit::dir::TestDir;
use kbf_testkit::tree::{Job, Tree};
use kbf_transfer::{TransferConfig, Transfers, digest_of};
use prost::Message as _;
use tonic::transport::Channel;

// The stand-in `crun` runs each action as `kbf-act` beside it: a link to this test
// binary, which answers as `kbf-act` through it.
kbf_testkit::act_test_hook!();

/// The bytes the action writes to its one output file.
const OUTPUT: &[u8] = b"a file\n";

/// A farm of `topology` in a fresh directory, and that directory (a node's files are under
/// it); both removed when the farm drops.
fn start_with(name: &str, topology: &Topology) -> (Farm, PathBuf) {
    let scratch = TestDir::under(
        scratch_root(PathBuf::from(env!("CARGO_TARGET_TMPDIR"))).join("kbf-planned-restart"),
        name,
    );
    let dir = scratch.path().to_path_buf();
    let bins = Bins {
        kbf: sibling_bin("kbf"),
        fake_crun: Some(sibling_bin("kbf-cli-fake-crun")),
    };
    let farm = Farm::start(&dir, &dir, &bins, topology)
        .unwrap_or_else(|e| panic!("{e}"))
        .owning(scratch);
    (farm, dir)
}

/// A planned restart of a loaded node (`a_loaded_node_restarts_planned_down_and_loses_no_action`).
const LOAD_NODES: usize = 3;
const LOAD_ACTIONS: usize = 12;
/// How long each action sleeps: longer than the planned-down, SIGTERM and restart take to
/// begin, so the victim still runs leases when it is stopped.
const SLEEP_SECS: &str = "4";

/// Plants the steps every node's stand-in `crun` runs (it reads them at each run): sleep,
/// then write the output file.
fn plant_slow_action(farm: &Farm, dir: &std::path::Path) {
    let steps = [
        "sleep",
        SLEEP_SECS,
        ";",
        "write-lines",
        "$UPPER/out/file.txt",
        "a file",
    ];
    let text: String = steps.iter().map(|s| format!("{s}\n")).collect();
    for n in 1..=farm.storage_nodes() {
        let file = dir.join(format!("node-{n}/bin/crun-state/action"));
        std::fs::write(&file, &text).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    }
}

/// Uploads the action of `input` (its command, its input tree and the action itself) and
/// returns the action's digest.
async fn upload_one(cas: Arc<Transfers>, image: String, input: usize) -> Digest {
    let bytes = format!("planned restart input {input:04} {}", "x".repeat(80)).into_bytes();
    let job = Job {
        input: Tree::with_input("input.bin", &bytes),
        outputs: vec!["out/file.txt".to_owned()],
        cacheable: true,
        ..Job::default()
    };
    upload_action(&cas, &image, &job).await
}

/// Executes the uploaded `action` to the end of its operation: its response, or why there
/// is none (a failed call, an operation that ended with no response, a hang).
async fn execute_one(reapi: Channel, action: Digest) -> Result<ExecuteResponse, String> {
    let call = async {
        let mut ops = ExecutionClient::new(reapi)
            .execute(ExecuteRequest {
                action_digest: Some(action),
                ..ExecuteRequest::default()
            })
            .await
            .map_err(|e| format!("Execute: {e}"))?
            .into_inner();
        loop {
            let op = ops
                .message()
                .await
                .map_err(|e| format!("the stream: {e}"))?
                .ok_or("the stream ended before the operation was done")?;
            if !op.done {
                continue;
            }
            let Some(operation::Result::Response(any)) = op.result else {
                return Err(format!("no response: {op:?}"));
            };
            return ExecuteResponse::decode(any.value.as_slice()).map_err(|e| e.to_string());
        }
    };
    tokio::time::timeout(HANG, call)
        .await
        .map_err(|_| "no answer in time".to_owned())?
}

/// The node (1-based) whose log first says a lease started, once any does.
fn node_with_a_lease(farm: &Farm) -> Option<usize> {
    let hits = farm.log_hits("lease started");
    let (file, _) = hits.first()?.split_once(": ")?;
    let rest = file.strip_prefix("node-")?;
    rest.split('.').next()?.parse().ok()
}

/// A planned restart of a loaded node: a frontend and three `storage-and-daemon` nodes (no
/// plain daemon, so every action lands on a node's own daemon half) run twelve distinct
/// actions at once, each a stand-in `crun` run that sleeps four seconds. Once a node has
/// running leases it is the victim: its storage member is marked `planned_down` through the
/// operator API, the frontend lists it down, the node gets SIGTERM and is started again.
///
/// The twelve actions are uploaded, and the uploads joined, before the victim is chosen and
/// its member marked `planned_down`: the restart moves the storage map's generation (the
/// node re-enters it catching up, then active), and an upload that spans those moves can be
/// refused `STALE_MAP`: the frontend's ByteStream `Write` passes that answer through
/// without a map refetch (#2157). A real planned restart can cause it, so uploads during a
/// restart are deliberately not exercised here until #2157 lands; its own tests cover the
/// retry. Only the Execute streams are in flight during the restart.
///
/// What it proves is the contract of a planned restart as an operator lives it: every
/// `Execute` still completes with exit code 0 (none cached, each output the action wrote),
/// no run is dropped and no lease fails or is fenced in any process's log in any
/// generation, the restarted node's member returns to `active`, and every process stops
/// with exit 0. Catches a node that, on SIGTERM, drops the leases it runs (a client sees a
/// failed action, the log says `run dropped`), a frontend that fails an action because the
/// member it ran on is planned down, and a member that does not come back.
///
/// A node that stopped its leases at SIGTERM completed every action too (the dropped lease
/// is run again on another node), so the test reads the logs: the victim drains (kbf#1087),
/// and no process logs `run dropped`.
///
/// It runs about half a minute and is on the slow list of the pull-request gate
/// (`.config/nextest.toml`); the beta workflow runs it.
#[test]
fn a_loaded_node_restarts_planned_down_and_loses_no_action() {
    // No plain daemon: every lease runs on a node's own daemon half.
    let topology = Topology {
        daemons: 0,
        ..Topology::rehearsal(LOAD_NODES)
    };
    let (mut farm, dir) = start_with("planned-restart", &topology);
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
    plant_slow_action(&farm, &dir);

    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let (channel, cas) = tokio.block_on(async {
        let channel = connect(farm.ca_pem(), farm.reapi()).await;
        let cas = Arc::new(Transfers::over(channel.clone(), TransferConfig::default()));
        (channel, cas)
    });
    let image = farm.image().to_owned();
    // The uploads are joined first: only the Execute streams are in flight once the victim
    // is chosen.
    let uploads: Vec<_> = (0..LOAD_ACTIONS)
        .map(|i| tokio.spawn(upload_one(cas.clone(), image.clone(), i)))
        .collect();
    let mut actions = Vec::with_capacity(LOAD_ACTIONS);
    for upload in uploads {
        actions.push(tokio.block_on(upload).expect("the upload task ran"));
    }
    // The load: every action is executed at once and awaited at the end.
    let running: Vec<_> = actions
        .into_iter()
        .map(|action| tokio.spawn(execute_one(channel.clone(), action)))
        .collect();

    // The victim is a node that runs leases now (its own log says so).
    let victim = farm.wait("a node to start a lease", node_with_a_lease);
    let member = farm
        .wait("the victim's member to be listed", |f| f.member_of(victim))
        .id;
    eprintln!("victim: node {victim}, member {member}");

    let body = r#"{"state":"planned_down","for_secs":300}"#;
    let (code, answer) = farm
        .post(&format!("/v1/members/{member}"), body)
        .expect("the API answers");
    assert_eq!(code, 200, "planned_down: {answer}");
    farm.wait("the victim's member to be listed down", |f| {
        (f.member_of(victim)?.state == "down").then_some(())
    });

    let status = farm.term(Role::Node(victim)).expect("SIGTERM was answered");
    assert!(status.success(), "the victim ended {status:?}");
    farm.restart(Role::Node(victim));
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));

    // Every action completed, ran (not a cache hit), and wrote its output.
    let mut failures = Vec::new();
    for (i, handle) in running.into_iter().enumerate() {
        match tokio.block_on(handle).expect("the task ran") {
            Err(why) => failures.push(format!("action {i}: {why}")),
            Ok(response) => {
                let result = response.result.clone().unwrap_or_default();
                let ok = result.exit_code == 0
                    && !response.cached_result
                    && response.status.as_ref().is_none_or(|s| s.code == 0)
                    && result.output_files.len() == 1
                    && result.output_files[0].digest.as_ref() == Some(&digest_of(OUTPUT));
                if !ok {
                    failures.push(format!("action {i}: {response:?}"));
                }
            }
        }
    }
    let logs = farm.logs();
    assert!(failures.is_empty(), "{failures:#?}\n{logs}");

    let member = farm.member_of(victim).expect("the member is listed");
    assert_eq!(member.state, "active", "the restarted node's member");

    // Nothing was dropped or fenced, in any process, in any generation.
    for needle in ["run dropped", "lease failed", "lease fenced"] {
        let hits = farm.log_hits(needle);
        // The victim's lease lines, to say what its leases were doing when this happened.
        let victim_leases = farm
            .log_hits("lease")
            .into_iter()
            .filter(|l| l.starts_with(&format!("node-{victim}.")))
            .collect::<Vec<_>>();
        assert!(
            hits.is_empty(),
            "{needle:?}:\n{}\nthe victim's lease lines:\n{}\n{logs}",
            hits.join("\n"),
            victim_leases.join("\n")
        );
    }
    farm.stop();
}
