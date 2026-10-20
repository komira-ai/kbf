//! A rehearsal of a deployment in a box: a frontend, three `storage-and-daemon` nodes and
//! one plain daemon, each a real `kbf` process started from the flags `kbf-deploy` renders
//! into its unit (`kbf_deploy::linux::modes::render`, the `ExecStart=` line, nothing
//! written by hand), on loopback addresses of their own, under certificates of the
//! rows the farm's certificate table has. A buck2-shaped client then drives the frontend:
//! CAS writes and reads, an `Execute` of an action (inputs, `Command` and `Action`
//! uploaded first), its outputs read back, and a repeat `Execute` answered from the
//! action cache.
//!
//! What it catches is the class of defect that makes a rendered unit not start or not
//! connect: a unit that omits flags its mode needs (the frontend with no `--storage`, a
//! node with no storage-half flags), a daemon dialling a name the frontend's certificate
//! does not hold (every node here has an address of its own, so `KBF_HOST_NAME` is not
//! the frontend's name), a certificate a deployed node would be refused, a unit kbf
//! refuses at start. Not covered: systemd itself, the held scripts and the host setup.
//!
//! The farm itself is `kbf_harness::farm::Farm`, which the beta scenarios share: this file
//! holds the REAPI client and the tests. The deviations from the rendered units are listed
//! in `Farm::start`: the cold store is a directory (no MinIO), the container driver runs
//! the stand-in `crun` over a planted cgroup tree and id files.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;

use kbf_harness::api::connected;
use kbf_harness::farm::{Bins, Farm, Role, Topology, node_id};
use kbf_harness::node_support::input_bytes;
use kbf_harness::procs::HANG;
use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, ActionResult, Command as ReapiCommand, Digest, Directory, DirectoryNode,
    ExecuteRequest, ExecuteResponse, FileNode, GetActionResultRequest, Platform,
};
use kbf_testkit::dir::TestDir;
use kbf_transfer::{Cas as _, TransferConfig, Transfers, digest_of};
use prost::Message as _;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

// The stand-in `crun` runs each action as `kbf-act` beside it: a link to this test
// binary, which answers as `kbf-act` through it.
kbf_testkit::act_test_hook!();

/// The bytes the action writes to its one output file.
const OUTPUT: &[u8] = b"a file\n";

/// A fresh directory for one test, removed when the farm that owns it drops.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        kbf_harness::node_support::scratch_root(PathBuf::from(env!("CARGO_TARGET_TMPDIR")))
            .join("kbf-rehearsal"),
        name,
    )
}

/// The rehearsal's farm of `storage_nodes` nodes and a daemon, started from the units
/// `kbf-deploy` renders, over the stand-in `crun`.
fn start(name: &str, storage_nodes: usize) -> Farm {
    start_with(name, &Topology::rehearsal(storage_nodes)).0
}

/// A farm of `topology`, and the directory it was started in (a node's files are under it).
fn start_with(name: &str, topology: &Topology) -> (Farm, PathBuf) {
    let scratch = scratch(name);
    let dir = scratch.path().to_path_buf();
    let bins = Bins {
        kbf: PathBuf::from(env!("CARGO_BIN_EXE_kbf")),
        fake_crun: Some(PathBuf::from(env!("CARGO_BIN_EXE_kbf-cli-fake-crun"))),
    };
    let farm = Farm::start(&dir, &dir, &bins, topology)
        .unwrap_or_else(|e| panic!("{e}"))
        .owning(scratch);
    (farm, dir)
}

/// Waits until the frontend is ready and lists every node as connected.
fn ready(farm: &mut Farm) {
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
}

/// A client of the frontend's REAPI listener (TLS under the farm CA).
async fn channel(farm: &Farm) -> Channel {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(farm.ca_pem()))
        .domain_name("localhost");
    Endpoint::from_shared(format!("https://{}", farm.reapi()))
        .expect("endpoint")
        .tls_config(tls)
        .expect("TLS")
        .connect()
        .await
        .expect("connect to the frontend")
}

/// Blob `i`: 100 distinct bytes (the disk scan anchors on 64).
fn blob(i: usize) -> Vec<u8> {
    format!("rehearsal blob {i:04} {}", "x".repeat(80)).into_bytes()
}

/// The action that reads `input` as `in/input.bin` and has `out/file.txt` written,
/// uploaded through `cas`; its digest.
async fn upload_action(cas: &Transfers, image: &str, input: &[u8]) -> Digest {
    let put = async |bytes: Vec<u8>| cas.put(bytes).await.expect("upload");
    let file = FileNode {
        name: "input.bin".to_owned(),
        digest: Some(put(input.to_vec()).await),
        ..FileNode::default()
    };
    let inner = Directory {
        files: vec![file],
        ..Directory::default()
    };
    let root = Directory {
        directories: vec![DirectoryNode {
            name: "in".to_owned(),
            digest: Some(put(inner.encode_to_vec()).await),
        }],
        ..Directory::default()
    };
    // The stand-in `crun` runs its planted steps whatever the arguments are.
    let command = ReapiCommand {
        arguments: vec!["true".to_owned()],
        output_paths: vec!["out/file.txt".to_owned()],
        ..ReapiCommand::default()
    };
    let action = Action {
        command_digest: Some(put(command.encode_to_vec()).await),
        input_root_digest: Some(put(root.encode_to_vec()).await),
        platform: Some(Platform {
            properties: vec![Property {
                name: "container-image".to_owned(),
                value: image.to_owned(),
            }],
        }),
        ..Action::default()
    };
    put(action.encode_to_vec()).await
}

/// Executes `action` (the action cache consulted) until its operation is done.
async fn execute(reapi: Channel, action: Digest) -> ExecuteResponse {
    let mut ops = ExecutionClient::new(reapi)
        .execute(ExecuteRequest {
            action_digest: Some(action),
            ..ExecuteRequest::default()
        })
        .await
        .expect("Execute")
        .into_inner();
    let done = loop {
        let op = ops.message().await.expect("stream").expect("an update");
        if op.done {
            break op;
        }
    };
    let Some(operation::Result::Response(any)) = done.result else {
        panic!("no response: {done:?}");
    };
    ExecuteResponse::decode(any.value.as_slice()).expect("response")
}

/// Writes 24 blobs through the frontend and reads them back; returns them.
async fn write_and_read(cas: &Transfers) -> Vec<Vec<u8>> {
    let blobs: Vec<Vec<u8>> = (0..24).map(blob).collect();
    for bytes in &blobs {
        let digest = cas
            .put(bytes.clone())
            .await
            .expect("write through the frontend");
        assert_eq!(digest, digest_of(bytes));
    }
    let digests: Vec<Digest> = blobs.iter().map(|b| digest_of(b)).collect();
    assert!(
        cas.missing(&digests).await.is_empty(),
        "a written blob is missing"
    );
    for bytes in &blobs {
        let read = cas
            .get(&digest_of(bytes))
            .await
            .expect("read through the frontend");
        assert_eq!(&read, bytes);
    }
    blobs
}

/// A buck2-shaped client's session against the ready farm: CAS writes and reads
/// through the frontend, then an `Execute` (inputs, `Command` and `Action` uploaded
/// first) whose output is read back through the frontend, then the same `Execute` again,
/// which the action cache answers. Returns the blobs written.
fn drive(farm: &Farm) -> Vec<Vec<u8>> {
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    tokio.block_on(async {
        let channel = channel(farm).await;
        let cas = Transfers::over(channel.clone(), TransferConfig::default());
        let blobs = write_and_read(&cas).await;

        let action = upload_action(&cas, farm.image(), &input_bytes()).await;
        let first = tokio::time::timeout(HANG, execute(channel.clone(), action.clone()))
            .await
            .expect("the action ran in time");
        let result = first
            .result
            .clone()
            .unwrap_or_else(|| panic!("no result: {first:?}"));
        assert!(!first.cached_result, "the first run was cached: {first:?}");
        assert_eq!(result.exit_code, 0, "{first:?}");
        let [file] = &result.output_files[..] else {
            panic!("one output file: {first:?}");
        };
        assert_eq!(file.path, "out/file.txt");
        assert_eq!(file.digest.as_ref(), Some(&digest_of(OUTPUT)));
        let read = cas
            .get(&digest_of(OUTPUT))
            .await
            .expect("the output, through the frontend");
        assert_eq!(read, OUTPUT);

        let again = tokio::time::timeout(HANG, execute(channel.clone(), action.clone()))
            .await
            .expect("the repeat answered in time");
        assert!(again.cached_result, "the repeat ran again: {again:?}");
        assert_eq!(again.result, first.result);
        let cached: ActionResult = ActionCacheClient::new(channel)
            .get_action_result(GetActionResultRequest {
                action_digest: Some(action),
                ..GetActionResultRequest::default()
            })
            .await
            .expect("GetActionResult")
            .into_inner();
        assert_eq!(Some(cached), first.result);
        blobs
    })
}

/// Catches a rendered unit that does not start or does not connect, for the whole
/// topology of a frontend, three storage-and-daemon nodes and a daemon: any process
/// exiting before its start line (a flag kbf refuses, a unit with its flags missing, a
/// certificate refused); a node that never registers or whose daemon half never connects
/// to the frontend (the frontend's name taken from the node's own, a certificate lacking
/// the role or name); a CAS write or read through the frontend that fails; and a process
/// not exiting 0 on SIGTERM.
#[test]
fn the_rendered_units_start_connect_and_serve_cas() {
    let mut farm = start("topology", 3);
    ready(&mut farm);
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    tokio.block_on(async {
        let cas = Transfers::over(channel(&farm).await, TransferConfig::default());
        write_and_read(&cas).await;
    });
    farm.stop();
}

/// Catches what the topology test cannot, a REAPI round trip through the rendered units
/// (a frontend, one storage-and-daemon node, a daemon): inputs uploaded, `Execute`
/// answered by a daemon, the output read back through the frontend, a repeat answered
/// from the action cache (`cached_result`, and `GetActionResult` alike). One storage
/// node: the farm of one member the frontend reaches through `--storage`; see
/// [`three_storage_nodes_run_an_action`] for the map with three.
#[test]
fn one_storage_node_runs_an_action_and_repeats_it_from_the_cache() {
    let mut farm = start("one-node", 1);
    ready(&mut farm);
    drive(&farm);
    farm.stop();
}

/// The same session on three storage nodes, and that the map spreads the blobs (each
/// node holds some).
///
/// Catches a frontend that reaches storage through its one `--storage` node (the
/// `Owners::single` and `Backend::remote` it started with) while the daemons route by the
/// map: a client's CAS writes would all land on that node, a daemon puts an action's
/// output on its rendezvous owner, so when that is another node the frontend asks the
/// `--storage` node `prepare_action_result`, is told the blob "is not held", and fails the
/// lease as an infrastructure failure on every node in turn. The frontend routes by the
/// members' map (`kbf_front::Routed`), so the output is found at its owner, the entry is
/// committed with its blobs at the action's owner, and the repeat is a cache hit.
#[test]
fn three_storage_nodes_run_an_action() {
    let mut farm = start("three-nodes", 3);
    ready(&mut farm);
    let blobs = drive(&farm);
    // The map spreads the blobs: each of the three nodes holds some (a frontend that
    // sent everything to its one `--storage` node leaves two empty).
    let spread = farm.wait("blobs on every storage node", |f| {
        let mut held = vec![0_usize; f.storage_nodes() + 1];
        blobs
            .iter()
            .flat_map(|b| f.holders(b))
            .for_each(|n| held[n] += 1);
        held[1..].iter().all(|n| *n > 0).then_some(held)
    });
    eprintln!("blobs held per node (index 1-3): {spread:?}");
    farm.stop();
}

/// How many actions the kill test runs before the kill: enough that the killed node
/// has committed some action's cache entry (it commits those of which it is the first
/// owner not marked down, a third of them), so the miss-and-rerun path runs.
const ACTIONS: usize = 6;

/// Writes the blobs numbered `range` through `cas` and reads each back.
async fn write_blobs(cas: &Transfers, range: std::ops::Range<usize>) -> Vec<Vec<u8>> {
    let blobs: Vec<Vec<u8>> = range.map(blob).collect();
    for bytes in &blobs {
        let digest = cas
            .put(bytes.clone())
            .await
            .expect("a write after the kill goes on to the blob's other owner");
        assert_eq!(digest, digest_of(bytes));
    }
    for bytes in &blobs {
        let read = cas.get(&digest_of(bytes)).await.expect("read back");
        assert_eq!(&read, bytes);
    }
    blobs
}

/// Runs an action whose input is `blob(input)` and returns its digest and result.
async fn run_action(
    channel: &Channel,
    cas: &Transfers,
    image: &str,
    input: usize,
) -> (Digest, ActionResult) {
    let action = upload_action(cas, image, &blob(input)).await;
    let response = tokio::time::timeout(HANG, execute(channel.clone(), action.clone()))
        .await
        .expect("the action answered in time");
    let result = response
        .result
        .clone()
        .unwrap_or_else(|| panic!("no result: {response:?}"));
    assert_eq!(result.exit_code, 0, "{response:?}");
    let [file] = &result.output_files[..] else {
        panic!("one output file: {response:?}");
    };
    assert_eq!(file.digest.as_ref(), Some(&digest_of(OUTPUT)));
    (action, result)
}

/// What the client asks of the frontend after the kill, `round` numbering the asking
/// (fresh blobs are numbered from it). `before` is what was written before it.
///
/// Every blob and the actions' output read back. Each action's cache entry either
/// answers as it did before (`GetActionResult`, and a repeat `Execute` with
/// `cached_result` set) or is a miss (NOT_FOUND, never another error), and the repeat
/// `Execute` of a miss runs the action again to the same output and its entry is back.
/// Then writes: 24 fresh blobs and a new action. Returns the fresh blobs and the number
/// of entries that were a miss.
async fn survivors(
    channel: &Channel,
    cas: &Transfers,
    image: &str,
    before: (&[Vec<u8>], &[(Digest, ActionResult)]),
    round: usize,
) -> (Vec<Vec<u8>>, usize) {
    let (blobs, actions) = before;
    for bytes in blobs {
        let read = cas
            .get(&digest_of(bytes))
            .await
            .expect("a blob lost with the node");
        assert_eq!(&read, bytes);
    }
    let read = cas
        .get(&digest_of(OUTPUT))
        .await
        .expect("the actions' output lost with the node");
    assert_eq!(read, OUTPUT);
    let lookup = async |action: &Digest| {
        ActionCacheClient::new(channel.clone())
            .get_action_result(GetActionResultRequest {
                action_digest: Some(action.clone()),
                ..GetActionResultRequest::default()
            })
            .await
    };
    let mut reruns = 0;
    for (action, result) in actions {
        let hit = match lookup(action).await {
            Ok(entry) => Some(entry.into_inner()),
            // A miss costs the action a rerun; any other answer is a failure of the farm.
            Err(status) if status.code() == tonic::Code::NotFound => None,
            Err(status) => panic!("GetActionResult after the kill: {status:?}"),
        };
        if let Some(entry) = &hit {
            assert_eq!(&entry.output_files, &result.output_files);
        }
        let again = tokio::time::timeout(HANG, execute(channel.clone(), action.clone()))
            .await
            .expect("the repeat answered in time");
        let ran = again.result.as_ref().expect("a result");
        assert_eq!(ran.exit_code, 0, "{again:?}");
        assert_eq!(ran.output_files, result.output_files, "{again:?}");
        assert_eq!(again.cached_result, hit.is_some(), "{again:?}");
        reruns += usize::from(hit.is_none());
        // Whichever it was, the entry is there now.
        let entry = lookup(action).await.expect("the entry after the repeat");
        assert_eq!(entry.into_inner().output_files, result.output_files);
    }
    // Writes: fresh blobs, then a fresh action (its input a fresh blob too).
    let fresh = 1000 * round;
    let written = write_blobs(cas, fresh..fresh + 24).await;
    run_action(channel, cas, image, fresh + 500).await;
    (written, reruns)
}

/// No blob is lost with a storage node, and the farm keeps taking writes.
///
/// A frontend, three storage-and-daemon nodes and a daemon, from the rendered units
/// (R=2: the map is the one the frontend and the nodes share). The client writes 24
/// blobs and `Execute`s six actions (six action cache entries, each committed with its
/// output at its owners). Once every blob is on two storage nodes, the node holding
/// most of them is killed with SIGKILL, and the frontend is asked, with the node dead
/// and not yet marked down anywhere the client can see:
/// - every blob written before the kill reads back, and so does the actions' output;
/// - each action's cache entry either answers as before (`GetActionResult` and a repeat
///   `Execute` agree, `cached_result` set) or is a miss (NOT_FOUND), in which case the
///   repeat `Execute` runs it again to the same output and the entry is back. An entry is
///   committed at one owner and not replicated (the design's "AC miss, rerun"), so the
///   entries the dead node committed are misses; a miss costs a rerun and never a wrong
///   or failed answer;
/// - writes after the kill succeed: 24 more blobs, whose first owner is the dead node
///   for about a third of them, go on to the other owner, and a new action runs to a
///   result committed at an owner that is alive;
/// - the same holds once the frontend lists the dead node's daemon as gone (the
///   frontend's connection list, not the map: the member's lease outlives this test, so
///   the map still has the node up and a `Down` owner is not reached here), and then
///   every entry is a hit: what the reruns committed is on a live node.
///
/// Catches a farm that writes a blob once (a primary that does not send its copy, a
/// copy that is owed and never made: the replica check before the kill, then the read
/// after it, fail), a frontend that fails a write because the first owner is dead
/// instead of going on to the next, and a commit that fails because its first owner is
/// dead. It reaches the write failover of the frontend's raw upload and of
/// `Router::commit_with_closure`; `Router::put` and skipping a `Down` owner are covered
/// by the `kbf-route` tests (`a_write_goes_on_to_the_next_owner_when_the_first_cannot_be_reached`,
/// `a_write_skips_an_owner_that_is_down`), not here. Planted to see it fail: `Chain::plan` answering `Plan::Local` for the primary
/// (no copy is written, so the blobs are on one node each).
#[test]
fn a_killed_storage_node_loses_no_blob() {
    let mut farm = start("kill", 3);
    ready(&mut farm);
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let (channel, cas, blobs, actions) = tokio.block_on(async {
        let channel = channel(&farm).await;
        let cas = Transfers::over(channel.clone(), TransferConfig::default());
        let blobs = write_and_read(&cas).await;
        let mut actions = Vec::new();
        for input in 100..100 + ACTIONS {
            actions.push(run_action(&channel, &cas, farm.image(), input).await);
        }
        (channel, cas, blobs, actions)
    });
    // Every blob is on two storage nodes before the node dies: the primary sent the
    // partner's copy before it answered, or owes it and the repair worker makes it.
    farm.wait("every blob on two storage nodes", |f| {
        blobs.iter().all(|b| f.holders(b).len() >= 2).then_some(())
    });
    let held = |n: usize| {
        blobs
            .iter()
            .filter(|b| farm.holders(b).contains(&n))
            .count()
    };
    let victim = (1..=farm.storage_nodes())
        .max_by_key(|n| held(*n))
        .expect("a node");
    assert!(held(victim) > 0, "no blob on any node");
    eprintln!("killing node {victim}, which holds {} blobs", held(victim));
    farm.kill(Role::Node(victim));

    let check = |farm: &Farm, round: usize| {
        tokio.block_on(survivors(
            &channel,
            &cas,
            farm.image(),
            (&blobs, &actions),
            round,
        ))
    };
    let (first, reruns) = check(&farm, 1);
    eprintln!("{reruns} of {ACTIONS} cache entries were gone with the node: rerun");
    // The farm is fixed (its map, the digests, the image), so the node holding most blobs
    // also committed at least one entry: without this a farm where the dead node
    // committed none would pass with the commit failover never exercised.
    assert!(
        reruns > 0,
        "no cache entry was committed by the killed node: the commit failover is unexercised"
    );

    // The frontend lists the killed node's daemon as gone; the same again.
    let gone: Vec<String> = farm
        .node_ids()
        .iter()
        .filter(|id| **id != node_id(victim))
        .cloned()
        .collect();
    farm.wait("the frontend to list the killed node as gone", |f| {
        (connected(f.api_port())? == gone).then_some(())
    });
    let (second, reruns) = check(&farm, 2);
    assert_eq!(reruns, 0, "an entry rewritten after the kill is gone again");
    // What was written after the kill is on the live nodes, whole.
    for bytes in first.iter().chain(&second) {
        let live = farm.holders(bytes).into_iter().filter(|n| *n != victim);
        assert!(
            live.count() > 0,
            "a write after the kill is on no live node"
        );
    }
    // The killed process is already gone; the others stop cleanly.
    for (role, how) in farm.stop_all() {
        assert_eq!(how, "exit 0", "{role}\n{}", farm.logs());
    }
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

/// Executes the action of `input` to the end of its operation: its response, or why there
/// is none (a failed call, an operation that ended with no response, a hang).
async fn run_one(
    reapi: Channel,
    cas: Arc<Transfers>,
    image: String,
    input: usize,
) -> Result<ExecuteResponse, String> {
    let bytes = format!("planned restart input {input:04} {}", "x".repeat(80)).into_bytes();
    let action = upload_action(&cas, &image, &bytes).await;
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

    // The load: every action is submitted at once and awaited at the end.
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let (channel, cas) = tokio.block_on(async {
        let channel = channel(&farm).await;
        let cas = Arc::new(Transfers::over(channel.clone(), TransferConfig::default()));
        (channel, cas)
    });
    let image = farm.image().to_owned();
    let running: Vec<_> = (0..LOAD_ACTIONS)
        .map(|i| tokio.spawn(run_one(channel.clone(), cas.clone(), image.clone(), i)))
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
