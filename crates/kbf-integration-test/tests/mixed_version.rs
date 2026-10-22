//! Releases N and N-1 in one farm of real `kbf` processes: the N/N-1 check of
//! docs/design/kbf-cicd.md (N/N-1 compatibility in CI).
//!
//! The farm is `kbf_harness::farm::Farm`: a frontend, `storage-and-daemon` nodes and a plain
//! daemon, each a `kbf --mode=<mode>` on the `kbf.toml` the harness writes. Each role runs
//! the `kbf` the topology names for it (`Topology::binaries`): N is the `kbf` built beside
//! this test binary, N-1 is the file `target/tmp/n-1/kbf` (under the target directory's
//! `tmp`, where this test binary's `CARGO_TARGET_TMPDIR` is). The `mixed-version` job of
//! beta.yml writes that file with `cargo xtask release fetch-previous --out target/tmp/n-1`:
//! the `kbf` of the release before the commit, out of its attested tarball. With no such
//! file (a run of the whole suite, or a repository with no release yet) each test says
//! `SKIPPED` on stderr and returns. The step also writes its `key=value` lines to
//! `target/n-1.source`: a `previous-source=release` there with no file is a failure, not a
//! skip.
//!
//! Both versions get the same settings, the ones the tip's units render. A setting N added
//! and N-1 does not know fails the start of N-1's process, and the farm's error names it.
//!
//! What it proves: a frontend of one version works with the storage nodes and daemons of
//! the other, in both directions, over the real wire: blobs written through the frontend
//! and read back, one action run on whichever daemon the scheduler picks, its output read
//! back; two storage nodes of different versions hold every blob (the copy the primary sends,
//! or the repair worker's if it owed one; a copy owed is not forced); a node restarted under
//! the other version in the middle of a load fails no action.
//!
//! Not under coverage: the pull-request gate has no N-1 to give it, and its lines would
//! count against the 99% of the crate.

#![cfg(all(target_os = "linux", not(coverage)))]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kbf_harness::farm::{Bins, Farm, Role, Topology};
use kbf_harness::node_support::scratch_root;
use kbf_harness::procs::HANG;
use kbf_harness::reapi::{connect, execute, upload_action};
use kbf_harness::stand_in::sibling_bin;
use kbf_proto::reapi::{Digest, ExecuteResponse};
use kbf_testkit::dir::TestDir;
use kbf_testkit::tree::{Job, Tree};
use kbf_transfer::{Cas as _, TransferConfig, Transfers, digest_of};
use tonic::transport::Channel;

// The stand-in `crun` runs each action as `kbf-act` beside it: a link to this test
// binary, which answers as `kbf-act` through it.
kbf_testkit::act_test_hook!();

/// The bytes the action writes to its one output file.
const OUTPUT: &[u8] = b"a file\n";

/// The `kbf` of N-1, or `None` when this run has none (said on stderr: the test is then
/// skipped). It must not be N's own file, or the run would be no mixture.
///
/// # Panics
/// When the file is absent although `fetch-previous` said (in `target/n-1.source`) that it
/// found a release: a skip then would be a pass of a check that never ran.
fn previous_kbf() -> Option<PathBuf> {
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let path = tmp.join("n-1/kbf");
    if !path.is_file() {
        let said = std::fs::read_to_string(tmp.with_file_name("n-1.source")).unwrap_or_default();
        let found = said.lines().any(|line| line == "previous-source=release");
        assert!(
            !found,
            "fetch-previous found a release but there is no N-1 kbf at {}",
            path.display()
        );
        eprintln!(
            "SKIPPED: no N-1 kbf at {}; the mixed-version job of beta.yml puts the kbf of \
             the previous release there",
            path.display()
        );
        return None;
    }
    let tip = std::fs::read(sibling_bin("kbf")).expect("read the tip's kbf");
    let old = std::fs::read(&path).expect("read the N-1 kbf");
    assert!(
        tip != old,
        "the N-1 kbf is the tip's: not a mixed-version run"
    );
    Some(path)
}

/// A farm of `topology` in a fresh directory, and that directory (a node's files are under
/// it); both removed when the farm drops. The roles `older` run the `kbf` at `previous`, the
/// others the tip's.
fn start(name: &str, topology: Topology, older: &[Role], previous: &Path) -> (Farm, PathBuf) {
    let topology = Topology {
        binaries: older.iter().map(|r| (*r, previous.to_owned())).collect(),
        ..topology
    };
    let scratch = TestDir::under(
        scratch_root(PathBuf::from(env!("CARGO_TARGET_TMPDIR"))).join("kbf-mixed-version"),
        name,
    );
    let dir = scratch.path().to_path_buf();
    let bins = Bins {
        kbf: sibling_bin("kbf"),
        fake_crun: Some(sibling_bin("kbf-cli-fake-crun")),
    };
    let farm = Farm::start(&dir, &dir, &bins, &topology)
        .unwrap_or_else(|e| panic!("{e}"))
        .owning(scratch);
    for role in older {
        assert_eq!(farm.spec(*role).program, previous, "{role} is not on N-1");
    }
    (farm, dir)
}

/// Every role of a farm of `nodes` nodes and a plain daemon but the frontend.
fn behind_the_frontend(nodes: usize) -> Vec<Role> {
    let mut roles: Vec<Role> = (1..=nodes).map(Role::Node).collect();
    roles.push(Role::Daemon(1));
    roles
}

/// Blob `i`: 99 bytes, distinct within the first 64 (the disk scan anchors on those).
fn blob(i: usize) -> Vec<u8> {
    format!("mixed version {i:04} {}", "x".repeat(80)).into_bytes()
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

/// Executes the action whose input is `blob(input)` (its output file written by the stand-in
/// `crun`) to the end of its operation.
async fn run_one(
    reapi: Channel,
    cas: Arc<Transfers>,
    image: String,
    input: usize,
) -> ExecuteResponse {
    let job = Job {
        input: Tree::with_input("input.bin", &blob(input)),
        outputs: vec!["out/file.txt".to_owned()],
        cacheable: true,
        ..Job::default()
    };
    let action = upload_action(&cas, &image, &job).await;
    tokio::time::timeout(HANG, execute(reapi, action))
        .await
        .expect("the action answered in time")
}

/// Why `response` is not a run of the action that wrote `OUTPUT`, or `None` when it is: its
/// exit code and status, one output file of the planted digest, and not a cache hit.
fn failure(response: &ExecuteResponse) -> Option<String> {
    let result = response.result.clone().unwrap_or_default();
    let output = result.output_files.first().and_then(|f| f.digest.as_ref());
    let wanted = digest_of(OUTPUT);
    let status = response.status.as_ref().map_or(0, |s| s.code);
    let checks = [
        (result.exit_code == 0, "the exit code is not 0"),
        (status == 0, "the status is not OK"),
        (result.output_files.len() == 1, "not one output file"),
        (output == Some(&wanted), "not the planted output"),
        (!response.cached_result, "a cache hit, not a run"),
    ];
    let broken: Vec<&str> = checks.iter().filter(|c| !c.0).map(|c| c.1).collect();
    (!broken.is_empty()).then(|| format!("{}: {response:?}", broken.join(", ")))
}

/// A buck2-shaped client's session against the ready farm: CAS writes and reads through the
/// frontend, then an `Execute` (the action's input a blob of its own) whose output is read
/// back through the frontend. Returns the blobs written.
async fn session(farm: &Farm) -> Vec<Vec<u8>> {
    let channel = connect(farm.ca_pem(), farm.reapi()).await;
    let cas = Arc::new(Transfers::over(channel.clone(), TransferConfig::default()));
    let blobs = write_and_read(&cas).await;
    let image = farm.image().to_owned();
    let response = run_one(channel, cas.clone(), image, 1000).await;
    let why = failure(&response);
    assert!(why.is_none(), "{}", why.unwrap_or_default());
    let read = cas
        .get(&digest_of(OUTPUT))
        .await
        .expect("the output, through the frontend");
    assert_eq!(read, OUTPUT);
    blobs
}

/// Waits until every blob is on both of the two storage nodes: the primary sent the
/// partner's copy before it answered, or owes it and the repair worker makes it.
fn held_by_both(farm: &mut Farm, blobs: &[Vec<u8>]) {
    farm.wait("every blob on both storage nodes", |f| {
        blobs.iter().all(|b| f.holders(b) == [1, 2]).then_some(())
    });
}

/// A frontend of N over the storage-and-daemon nodes and the plain daemon of N-1.
///
/// Catches a frontend that cannot work with the older nodes' storage RPC, map or heartbeat
/// (a write or read through it fails, a node never connects, a member is not active), a
/// worker session the older daemons are refused (the action fails), and an older process
/// that does not stop with exit 0 when asked.
#[test]
fn an_n_frontend_works_with_n_minus_1_nodes_and_daemon() {
    let Some(previous) = previous_kbf() else {
        return;
    };
    let older = behind_the_frontend(2);
    let (mut farm, _) = start("n-frontend", Topology::rehearsal(2), &older, &previous);
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let blobs = tokio.block_on(session(&farm));
    held_by_both(&mut farm, &blobs);
    farm.stop();
}

/// A frontend of N-1 over the storage-and-daemon nodes and the plain daemon of N: the
/// order of a rollback, and of a rolling update that reaches the frontend last.
///
/// Catches what the test above catches, from the other side: nodes and daemons that send a
/// message an older frontend does not read, or a map it cannot use.
/// The daemon of N sends no `UpdateStatus` (its update client is gone), so the older
/// frontend sees a node with no update report; that must not keep the node out of the farm.
#[test]
fn an_n_minus_1_frontend_works_with_n_nodes_and_daemon() {
    let Some(previous) = previous_kbf() else {
        return;
    };
    let (mut farm, _) = start(
        "n-minus-1-frontend",
        Topology::rehearsal(2),
        &[Role::Frontend],
        &previous,
    );
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let blobs = tokio.block_on(session(&farm));
    held_by_both(&mut farm, &blobs);
    farm.stop();
}

/// Two storage nodes of different versions under a frontend of N, with the plain daemon of
/// N-1 (it reads and writes the nodes of both versions directly, not through the frontend).
///
/// Catches a chain replication that one version cannot take from the other: a blob written
/// through the frontend is on both nodes, node 1 of N and node 2 of N-1 (the chain's order
/// is by digest, so across 24 blobs either node is the primary of some; the test does not
/// assert both), and an action run by whichever daemon the scheduler picks gets its inputs
/// and puts its output on storage of either version.
#[test]
fn storage_nodes_of_n_and_n_minus_1_replicate_to_each_other() {
    let Some(previous) = previous_kbf() else {
        return;
    };
    let older = [Role::Node(2), Role::Daemon(1)];
    let (mut farm, _) = start("replicate", Topology::rehearsal(2), &older, &previous);
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let blobs = tokio.block_on(session(&farm));
    held_by_both(&mut farm, &blobs);
    farm.stop();
}

/// A planned restart of a loaded node, as the update that reaches it restarts it: a frontend
/// of N and three storage-and-daemon nodes of N-1 (no plain daemon, so every action lands on
/// a node's own daemon half) run twelve distinct actions at once, each a stand-in `crun` run
/// that sleeps four seconds. Once a node has running leases it is the victim: its member is
/// marked `planned_down` through the operator API, the node gets SIGTERM and is started
/// again under N, on the same command line and files.
///
/// What it proves: every `Execute` still completes with exit code 0 and its output, none
/// from the cache, the restarted N node rejoining its N-1 peers; no process of either version
/// logs `run dropped`, `lease failed` or `lease fenced` in any generation (a dropped lease is
/// rerun elsewhere, so the responses alone cannot show it); the restarted node's member
/// returns to `active` and the farm, now of two versions, takes a session; and every
/// process, of either version, stops with exit 0. Catches an action failed by the
/// restart of a node (the lease it ran, the map the frontend holds of it, the heartbeat or
/// registration of a node that comes back as a newer version) and a restarted node that does
/// not rejoin the frontend of the other.
#[test]
fn a_loaded_node_restarted_as_n_loses_no_action() {
    let Some(previous) = previous_kbf() else {
        return;
    };
    // No plain daemon: every lease runs on a node's own daemon half.
    let topology = Topology {
        daemons: 0,
        ..Topology::rehearsal(LOAD_NODES)
    };
    let older: Vec<Role> = (1..=LOAD_NODES).map(Role::Node).collect();
    let (mut farm, dir) = start("restart", topology, &older, &previous);
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
    plant_slow_action(&farm, &dir);

    // The load: every action is submitted at once and awaited at the end.
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let (channel, cas) = tokio.block_on(async {
        let channel = connect(farm.ca_pem(), farm.reapi()).await;
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
    farm.restart_with(Role::Node(victim), &sibling_bin("kbf"));
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));

    // Every action completed, ran (not a cache hit), and wrote its output.
    let mut failures = Vec::new();
    for (i, handle) in running.into_iter().enumerate() {
        match tokio.block_on(handle) {
            Err(why) => failures.push(format!("action {i}: {why}")),
            Ok(response) => {
                let why = failure(&response);
                failures.extend(why.map(|why| format!("action {i}: {why}")));
            }
        }
    }
    let logs = farm.logs();
    assert!(failures.is_empty(), "{failures:#?}\n{logs}");
    let member = farm.member_of(victim).expect("the member is listed");
    assert_eq!(member.state, "active", "the restarted node's member");

    // The farm of two versions takes a session.
    tokio.block_on(session(&farm));

    // Nothing was dropped or fenced, in any process, in any generation.
    for needle in ["run dropped", "lease failed", "lease fenced"] {
        let hits = farm.log_hits(needle);
        assert!(hits.is_empty(), "{needle:?}:\n{}", hits.join("\n"));
    }
    farm.stop();
}

/// The nodes of the restart test.
const LOAD_NODES: usize = 3;
/// How many actions the restart test runs at once.
const LOAD_ACTIONS: usize = 12;
/// How long each action sleeps: longer than the planned-down, SIGTERM and restart take to
/// begin, so the victim still runs leases when it is stopped.
const SLEEP_SECS: &str = "4";

/// Plants the steps every node's stand-in `crun` runs (it reads them at each run): sleep,
/// then write the output file.
fn plant_slow_action(farm: &Farm, dir: &Path) {
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

/// The node (1-based) whose log first says a lease started, once any does.
fn node_with_a_lease(farm: &Farm) -> Option<usize> {
    let hits = farm.log_hits("lease started");
    let (file, _) = hits.first()?.split_once(": ")?;
    let rest = file.strip_prefix("node-")?;
    rest.split('.').next()?.parse().ok()
}
