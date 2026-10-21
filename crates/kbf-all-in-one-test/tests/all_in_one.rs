//! The all-in-one suite of the black-box crates (crates/kbf-all-in-one-test/README.md): the farm,
//! end to end. The farm is `kbf_harness::farm::Farm`, the REAPI client of its frontend is
//! `kbf_harness::reapi`; this file holds the tests.
//!
//! The processes are found beside the test binary (`kbf`, and the stand-in `crun` that
//! runs each action as `kbf-act`, a link to this test binary, answered by the hook below).

// Not under coverage: the pull-request gate does not run this crate (it is a beta suite,
// .config/nextest.toml), and its unrun lines would count against the 99% of the crate.
#![cfg(all(target_os = "linux", not(coverage)))]

use std::path::PathBuf;

use kbf_harness::farm::{Bins, Farm, Topology};
use kbf_harness::node_support::{input_bytes, scratch_root};
use kbf_harness::procs::HANG;
use kbf_harness::reapi::{connect, execute, upload_action};
use kbf_harness::stand_in::sibling_bin;
use kbf_testkit::dir::TestDir;
use kbf_testkit::tree::{Job, Tree};
use kbf_transfer::{Cas as _, TransferConfig, Transfers, digest_of};

kbf_testkit::act_test_hook!();

/// The bytes the stand-in `crun`'s planted action writes to its one output file.
const OUTPUT: &[u8] = b"a file\n";

/// A farm of `topology` in a fresh directory, removed when the farm drops.
fn start(name: &str, topology: &Topology) -> Farm {
    let scratch = TestDir::under(
        scratch_root(PathBuf::from(env!("CARGO_TARGET_TMPDIR"))).join("kbf-all-in-one-test"),
        name,
    );
    let dir = scratch.path().to_path_buf();
    let bins = Bins {
        kbf: sibling_bin("kbf"),
        fake_crun: Some(sibling_bin("kbf-cli-fake-crun")),
    };
    Farm::start(&dir, &dir, &bins, topology)
        .unwrap_or_else(|e| panic!("{e}"))
        .owning(scratch)
}

/// A-wiring: one storage-and-daemon node and a daemon behind a frontend, all rendered
/// units, then one action through the frontend. The node registers, its daemon half takes
/// the lease, the action runs, and the output is read back through the frontend.
///
/// Catches a farm that does not start, a node whose daemon half never connects (so no
/// lease is ever taken and `Execute` hangs, which `HANG` bounds), an action that runs and
/// reports the wrong exit code or output, and a frontend that cannot serve what the node
/// wrote.
#[test]
fn a_storage_node_takes_a_lease_and_runs_an_action() {
    let mut farm = start("one-action", &Topology::rehearsal(1));
    farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    tokio.block_on(async {
        let channel = connect(farm.ca_pem(), farm.reapi()).await;
        let job = Job {
            input: Tree::with_input("input.bin", &input_bytes()),
            outputs: vec!["out/file.txt".to_owned()],
            cacheable: true,
            ..Job::default()
        };
        let cas = Transfers::over(channel.clone(), TransferConfig::default());
        let action = upload_action(&cas, farm.image(), &job).await;
        let response = tokio::time::timeout(HANG, execute(channel, action))
            .await
            .expect("the action ran in time");
        assert!(
            !response.cached_result,
            "a first run was cached: {response:?}"
        );
        let Some(result) = &response.result else {
            panic!("no result: {response:?}");
        };
        assert_eq!(result.exit_code, 0, "{result:?}");
        let [file] = &result.output_files[..] else {
            panic!("one output file: {result:?}");
        };
        assert_eq!(file.path, "out/file.txt");
        assert_eq!(file.digest.as_ref(), Some(&digest_of(OUTPUT)));
        let read = cas
            .get(&digest_of(OUTPUT))
            .await
            .expect("the output, through the frontend");
        assert_eq!(read, OUTPUT);
    });
    farm.stop();
}
