//! A rehearsal of a deployment in a box: a frontend and storage nodes, each a real `kbf`
//! process started as the packaged units start it (`kbf --mode=<mode>` on a generated
//! `kbf.toml`, `kbf_harness::farm::Farm`; the flags the `kbf-deploy` units used to give are
//! held by `harness_toml_equivalence.rs`), on loopback addresses of their own, under
//! certificates of the rows the farm's certificate table has. A client then
//! drives the frontend's CAS: blob writes and reads, through the frontend, across the storage
//! nodes' map.
//!
//! What it catches is the class of defect that makes a rendered unit not start or not
//! connect: a unit that omits flags its mode needs (the frontend with no `--storage`, a
//! node with no storage-half flags), a certificate a deployed node would be refused, a unit
//! kbf refuses at start. Not covered: systemd itself, the held scripts and the host setup.
//! The farm runs no action: placed actions run as js-agent jobs, not in this farm.
//!
//! The farm itself is `kbf_harness::farm::Farm`, which other tests share: this file holds
//! the tests. The deviations from the rendered units are listed in `Farm::start`: the cold
//! store is a directory (no MinIO).

#![cfg(target_os = "linux")]

use std::path::PathBuf;

use kbf_harness::farm::{Bins, Farm, Role, Topology};
use kbf_harness::node_support::input_bytes;
use kbf_harness::reapi::connect;
use kbf_proto::reapi::Digest;
use kbf_testkit::dir::TestDir;
use kbf_transfer::{Cas as _, TransferConfig, Transfers, digest_of};

/// A fresh directory for one test, removed when the farm that owns it drops.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-rehearsal"),
        name,
    )
}

/// The rehearsal's farm of `storage_nodes` nodes, started from a generated `kbf.toml`.
fn start(name: &str, storage_nodes: usize) -> Farm {
    let scratch = scratch(name);
    let dir = scratch.path().to_path_buf();
    let bins = Bins {
        kbf: PathBuf::from(env!("CARGO_BIN_EXE_kbf")),
    };
    Farm::start(&dir, &dir, &bins, &Topology::rehearsal(storage_nodes))
        .unwrap_or_else(|e| panic!("{e}"))
        .owning(scratch)
}

/// Waits until the frontend is ready and lists every storage member active.
fn ready(farm: &mut Farm) {
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
}

/// A client of the frontend's CAS (REAPI listener, TLS under the farm CA).
async fn client(farm: &Farm) -> Transfers {
    let channel = connect(farm.ca_pem(), farm.reapi()).await;
    Transfers::over(channel, TransferConfig::default())
}

/// Blob `i`: 100 distinct bytes (the disk scan anchors on 64).
fn blob(i: usize) -> Vec<u8> {
    format!("rehearsal blob {i:04} {}", "x".repeat(80)).into_bytes()
}

/// Writes the blobs numbered `range` through `cas`, checks they are all there, and reads each
/// back; returns them.
async fn write_and_read(cas: &Transfers, range: std::ops::Range<usize>) -> Vec<Vec<u8>> {
    let blobs: Vec<Vec<u8>> = range.map(blob).collect();
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

/// Catches a rendered unit that does not start or does not connect, for the whole topology of
/// a frontend and three storage nodes: any process exiting before its start line (a flag kbf
/// refuses, a unit with its flags missing, a certificate refused); a node that never
/// registers (the frontend's name taken from the node's own, a certificate lacking the role or
/// name); a CAS write or read through the frontend that fails, for small blobs and for one
/// longer than any chunk or buffer of the read path; a map that sends every blob to one node
/// (a frontend that reaches storage through its one `--storage` node leaves two empty); and a
/// process not exiting 0 on SIGTERM.
#[test]
fn the_rendered_units_start_connect_and_serve_cas() {
    let mut farm = start("topology", 3);
    ready(&mut farm);
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let blobs = tokio.block_on(async {
        let cas = client(&farm).await;
        let big = input_bytes();
        let digest = cas.put(big.clone()).await.expect("write the large blob");
        assert_eq!(digest, digest_of(&big));
        assert_eq!(cas.get(&digest).await.expect("read the large blob"), big);
        write_and_read(&cas, 0..24).await
    });
    // The map spreads the blobs: each of the three nodes holds some.
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

/// The farm of one member the frontend reaches through `--storage`: a CAS write and read
/// through the frontend, and the blobs on the one node.
///
/// Catches a frontend that cannot work with a map of one member (the first member's address
/// as the map holds it), and a node that holds nothing after a write acknowledged.
#[test]
fn one_storage_node_serves_cas() {
    let mut farm = start("one-node", 1);
    ready(&mut farm);
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let blobs = tokio.block_on(async { write_and_read(&client(&farm).await, 0..24).await });
    farm.wait("every blob on the one node", |f| {
        blobs.iter().all(|b| f.holders(b) == [1]).then_some(())
    });
    farm.stop();
}

/// No blob is lost with a storage node, and the farm keeps taking writes.
///
/// A frontend and three storage nodes, from the rendered units (R=2: the map is the one the
/// frontend and the nodes share). The client writes 24 blobs. Once every blob is on two
/// storage nodes, the node holding most of them is killed with SIGKILL, and the frontend is
/// asked, with the node dead and not yet marked down anywhere the client can see:
/// - every blob written before the kill reads back;
/// - writes after the kill succeed: 24 more blobs, whose first owner is the dead node for
///   about a third of them, go on to the other owner, and read back.
///
/// Catches a farm that writes a blob once (a primary that does not send its copy, a copy
/// that is owed and never made: the replica check before the kill, then the read after it,
/// fail) and a frontend that fails a write because the first owner is dead instead of going on
/// to the next. It reaches the write failover of the frontend's raw upload; `Router::put` and
/// skipping a `Down` owner are covered by the `kbf-route` tests
/// (`a_write_goes_on_to_the_next_owner_when_the_first_cannot_be_reached`,
/// `a_write_skips_an_owner_that_is_down`), not here.
#[test]
fn a_killed_storage_node_loses_no_blob() {
    let mut farm = start("kill", 3);
    ready(&mut farm);
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let (cas, blobs) = tokio.block_on(async {
        let cas = client(&farm).await;
        let blobs = write_and_read(&cas, 0..24).await;
        (cas, blobs)
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

    let written = tokio.block_on(async {
        for bytes in &blobs {
            let read = cas
                .get(&digest_of(bytes))
                .await
                .expect("a blob lost with the node");
            assert_eq!(&read, bytes);
        }
        write_and_read(&cas, 1000..1024).await
    });
    // What was written after the kill is on the live nodes, whole.
    for bytes in &written {
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
