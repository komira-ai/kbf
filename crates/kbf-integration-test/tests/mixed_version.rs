//! Releases N and N-1 in one farm of real `kbf` processes: the N/N-1 check of
//! docs/design/kbf-cicd.md (N/N-1 compatibility in CI).
//!
//! The farm is `kbf_harness::farm::Farm`: a frontend and storage nodes, each a
//! `kbf --mode=<mode>` on the `kbf.toml` the harness writes. Each role runs the `kbf` the
//! topology names for it (`Topology::binaries`): N is the `kbf` built beside this test
//! binary, N-1 is the file `target/tmp/n-1/kbf` (under the target directory's `tmp`, where
//! this test binary's `CARGO_TARGET_TMPDIR` is). The `mixed-version` job of beta.yml writes
//! that file with `cargo xtask release fetch-previous --out target/tmp/n-1`: the `kbf` of the
//! release before the commit, out of its attested tarball. With no such file (a run of the
//! whole suite, or a repository with no release yet) each test says `SKIPPED` on stderr and
//! returns. The step also writes its `key=value` lines to `target/n-1.source`: a
//! `previous-source=release` there with no file is a failure, not a skip.
//!
//! Both versions get the same settings, the ones the tip's units render. A setting N added
//! and N-1 does not know fails the start of N-1's process, and the farm's error names it.
//!
//! What it proves: a frontend of one version works with the storage nodes of the other, in
//! both directions, over the real wire: blobs written through the frontend and read back, and
//! two storage nodes of different versions hold every blob (the copy the primary sends, or the
//! repair worker's if it owed one; a copy owed is not forced).
//!
//! Every format changes additively, so each release starts on the data the other wrote. Two
//! tests restart the storage nodes of a farm under the other version, one at a time, on the
//! files they had: `n_minus_1_restarts_on_data_n_wrote` writes blobs on a farm of N and
//! restarts each node as N-1, and `n_restarts_on_data_n_minus_1_wrote` does the same from a
//! farm of N-1, restarting each node as N. After the restarts the blobs read back through the
//! frontend and are on both nodes, every member is `active`, a new session runs, and no
//! process of either version has ended with status 2 (a start the binary refused).
//! The pull-request gate holds the same rule without a previous binary: the fixture and
//! golden tests of `kbf-log` and `kbf-server` (`n_minus_1.rs`, `golden_corpus.rs`,
//! `n_minus_1_log.rs`, `n_minus_1_rows.rs`).
//!
//! Not under coverage: the pull-request gate has no N-1 to give it, and its lines would
//! count against the 99% of the crate.

#![cfg(all(target_os = "linux", not(coverage)))]

use std::path::{Path, PathBuf};

use kbf_harness::farm::{Bins, Farm, Role, Topology};
use kbf_harness::procs::HANG;
use kbf_harness::reapi::connect;
use kbf_harness::stand_in::sibling_bin;
use kbf_proto::reapi::Digest;
use kbf_testkit::dir::TestDir;
use kbf_transfer::{Cas as _, TransferConfig, Transfers, digest_of};

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

/// A farm of `topology` in a fresh directory, removed when the farm drops. The roles `older`
/// run the `kbf` at `previous`, the others the tip's.
fn start(name: &str, topology: Topology, older: &[Role], previous: &Path) -> Farm {
    let topology = Topology {
        binaries: older.iter().map(|r| (*r, previous.to_owned())).collect(),
        ..topology
    };
    let scratch = TestDir::under(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-mixed-version"),
        name,
    );
    let dir = scratch.path().to_path_buf();
    let bins = Bins {
        kbf: sibling_bin("kbf"),
    };
    let farm = Farm::start(&dir, &dir, &bins, &topology)
        .unwrap_or_else(|e| panic!("{e}"))
        .owning(scratch);
    for role in older {
        assert_eq!(farm.spec(*role).program, previous, "{role} is not on N-1");
    }
    farm
}

/// Every storage node of a farm of `nodes` nodes.
fn storage_nodes(nodes: usize) -> Vec<Role> {
    (1..=nodes).map(Role::Node).collect()
}

/// Blob `i`: 99 bytes, distinct within the first 64 (the disk scan anchors on those).
fn blob(i: usize) -> Vec<u8> {
    format!("mixed version {i:04} {}", "x".repeat(80)).into_bytes()
}

/// Reads each of `blobs` through the frontend; each must be the bytes that were written.
async fn read_back(cas: &Transfers, blobs: &[Vec<u8>]) {
    for bytes in blobs {
        let read = cas
            .get(&digest_of(bytes))
            .await
            .expect("read through the frontend");
        assert_eq!(&read, bytes);
    }
}

/// Writes 24 blobs, `blob(first)` on, through the frontend and reads them back; returns them.
async fn write_and_read(cas: &Transfers, first: usize) -> Vec<Vec<u8>> {
    let blobs: Vec<Vec<u8>> = (first..first + 24).map(blob).collect();
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
    read_back(cas, &blobs).await;
    blobs
}

/// A client's session against the ready farm: CAS writes and reads through the
/// frontend. Returns the blobs written.
async fn session(farm: &Farm) -> Vec<Vec<u8>> {
    session_of(farm, 0).await
}

/// [`session`] with its blobs from `blob(first)` on: a second session on a farm that ran one
/// takes other numbers.
async fn session_of(farm: &Farm, first: usize) -> Vec<Vec<u8>> {
    let channel = connect(farm.ca_pem(), farm.reapi()).await;
    let cas = Transfers::over(channel, TransferConfig::default());
    write_and_read(&cas, first).await
}

/// Waits until every blob is on both of the two storage nodes: the primary sent the
/// partner's copy before it answered, or owes it and the repair worker makes it.
fn held_by_both(farm: &mut Farm, blobs: &[Vec<u8>]) {
    farm.wait("every blob on both storage nodes", |f| {
        blobs.iter().all(|b| f.holders(b) == [1, 2]).then_some(())
    });
}

/// A frontend of N over the storage nodes of N-1.
///
/// Catches a frontend that cannot work with the older nodes' storage RPC, map or heartbeat
/// (a write or read through it fails, a member is not active), and an older process that does
/// not stop with exit 0 when asked.
#[test]
fn an_n_frontend_works_with_n_minus_1_nodes() {
    let Some(previous) = previous_kbf() else {
        return;
    };
    let older = storage_nodes(2);
    let mut farm = start("n-frontend", Topology::rehearsal(2), &older, &previous);
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let blobs = tokio.block_on(session(&farm));
    held_by_both(&mut farm, &blobs);
    farm.stop();
}

/// A frontend of N-1 over the storage nodes of N: the order of a rollback, and of a rolling
/// update that reaches the frontend last.
///
/// Catches what the test above catches, from the other side: nodes that send a message an
/// older frontend does not read, or a map it cannot use.
#[test]
fn an_n_minus_1_frontend_works_with_n_nodes() {
    let Some(previous) = previous_kbf() else {
        return;
    };
    let mut farm = start(
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

/// Two storage nodes of different versions under a frontend of N.
///
/// Catches a chain replication that one version cannot take from the other: a blob written
/// through the frontend is on both nodes, node 1 of N and node 2 of N-1 (the chain's order
/// is by digest, so across 24 blobs either node is the primary of some; the test does not
/// assert both).
#[test]
fn storage_nodes_of_n_and_n_minus_1_replicate_to_each_other() {
    let Some(previous) = previous_kbf() else {
        return;
    };
    let older = [Role::Node(2)];
    let mut farm = start("replicate", Topology::rehearsal(2), &older, &previous);
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let blobs = tokio.block_on(session(&farm));
    held_by_both(&mut farm, &blobs);
    farm.stop();
}

/// Runs `work` to its end, or panics once [`HANG`] has passed: a node that accepts a
/// connection and never answers fails the test and not the job's timeout.
async fn within<T>(what: &str, work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG, work)
        .await
        .unwrap_or_else(|_| panic!("{what} did not finish in time"))
}

/// Stops storage node `node` and starts it again under the `kbf` at `kbf`, on the command line
/// and files it had, as an update restarts a host: its member is marked `planned_down` (and
/// listed `down`), SIGTERM, then the new process. Returns when every member is `active`
/// again, which the member of the node is only once it has registered anew.
fn restart_node(farm: &mut Farm, node: usize, kbf: &Path) {
    let role = Role::Node(node);
    let member = farm
        .wait("the node's member to be listed", |f| f.member_of(node))
        .id;
    let body = r#"{"state":"planned_down","for_secs":300}"#;
    let (code, answer) = farm
        .post(&format!("/v1/members/{member}"), body)
        .expect("the API answers");
    assert_eq!(code, 200, "planned_down: {answer}");
    farm.wait("the node's member to be listed down", |f| {
        (f.member_of(node)?.state == "down").then_some(())
    });
    let status = farm.term(role).expect("SIGTERM was answered");
    assert!(status.success(), "{role} ended {status:?}");
    farm.restart_with(role, kbf);
    let program = &farm.spec(role).program;
    assert_eq!(program, kbf, "{role} is not on the build asked for");
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
}

/// Asserts that no process of the farm, of any generation, ended with status 2 (the status of
/// a start the binary refused, which its unit does not restart) and that none ended on its own.
fn none_refused_to_start(farm: &mut Farm) {
    let refused: Vec<String> = farm
        .process_table()
        .into_iter()
        .filter(|p| p.ended.as_deref() == Some("exit 2"))
        .map(|p| format!("{} generation {}", p.role, p.generation))
        .collect();
    assert!(refused.is_empty(), "{refused:?}\n{}", farm.logs());
    let crashed: Vec<String> = farm
        .unexpected_exits()
        .into_iter()
        .map(|(role, how)| format!("{role}: {how}"))
        .collect();
    assert!(crashed.is_empty(), "{crashed:?}\n{}", farm.logs());
}

/// The body of the two restart tests: a farm of two storage nodes, whose roles `older` run
/// the `kbf` at `previous` and the rest the tip's, writes 24 blobs; then each storage node in
/// turn is stopped and started again under the `kbf` at `then`, on the files it had.
/// Afterwards the blobs read back through the frontend and are on both nodes, both members are
/// `active` (neither `catching_up`), a new session (other blobs) runs, no process ended with
/// status 2 or on its own, and every process stops with exit 0.
fn restart_each_node(name: &str, older: &[Role], previous: &Path, then: &Path) {
    let mut farm = start(name, Topology::rehearsal(2), older, previous);
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let first = session(&farm);
    let written = tokio.block_on(within("the first session", first));
    held_by_both(&mut farm, &written);
    for node in 1..=farm.storage_nodes() {
        restart_node(&mut farm, node, then);
    }
    let dial = connect(farm.ca_pem(), farm.reapi());
    let channel = tokio.block_on(within("the connection", dial));
    let cas = Transfers::over(channel, TransferConfig::default());
    let reading = read_back(&cas, &written);
    tokio.block_on(within("the read back of the blobs", reading));
    held_by_both(&mut farm, &written);
    assert_eq!(farm.whole(), Ok(()), "{}", farm.logs());
    let members = farm.members().expect("the frontend lists its members");
    let states: Vec<&str> = members.iter().map(|m| m.state.as_str()).collect();
    assert_eq!(states, ["active", "active"], "{}", farm.logs());
    let next = session_of(&farm, 100);
    let fresh = tokio.block_on(within("the new session", next));
    held_by_both(&mut farm, &fresh);
    none_refused_to_start(&mut farm);
    farm.stop();
}

/// Asserts that the two storage nodes of a farm of N, each stopped and started again as N-1 on
/// the files N wrote (the downgrade of a rollback), keep what N wrote and the farm stays whole:
/// the 24 blobs N wrote read back through the frontend and are on both nodes, both members are
/// `active`, a new session runs and no process ended with status 2. With no N-1 `kbf` the test
/// says SKIPPED and returns, as the others do.
#[test]
fn n_minus_1_restarts_on_data_n_wrote() {
    let Some(previous) = previous_kbf() else {
        return;
    };
    restart_each_node("n-minus-1-on-n-data", &[], &previous, &previous);
}

/// Asserts that the two storage nodes of a farm of N-1 (the frontend included), each stopped
/// and started again as N on the files N-1 wrote (the first step of an update), keep what N-1
/// wrote and the farm stays whole: the 24 blobs N-1 wrote read back through the frontend and
/// are on both nodes, both members are `active`, a new session runs and no process ended with
/// status 2. With no N-1 `kbf` the test says SKIPPED and returns, as the others do.
#[test]
fn n_restarts_on_data_n_minus_1_wrote() {
    let Some(previous) = previous_kbf() else {
        return;
    };
    let mut older = storage_nodes(2);
    older.push(Role::Frontend);
    let tip = sibling_bin("kbf");
    restart_each_node("n-on-n-minus-1-data", &older, &previous, &tip);
}
