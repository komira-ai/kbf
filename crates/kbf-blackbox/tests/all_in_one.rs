//! Suite A of the black-box suites (crates/kbf-blackbox/README.md): the all-in-one farm,
//! end to end. The farm is `kbf_harness::farm::Farm`; this file holds a REAPI client of
//! the frontend and the tests.
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
use kbf_harness::stand_in::sibling_bin;
use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, Command as ReapiCommand, Digest, Directory, DirectoryNode, ExecuteRequest,
    ExecuteResponse, FileNode, Platform,
};
use kbf_testkit::dir::TestDir;
use kbf_transfer::{Cas as _, TransferConfig, Transfers, digest_of};
use prost::Message as _;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

kbf_testkit::act_test_hook!();

/// The bytes the stand-in `crun`'s planted action writes to its one output file.
const OUTPUT: &[u8] = b"a file\n";

/// A farm of `topology` in a fresh directory, removed when the farm drops.
fn start(name: &str, topology: &Topology) -> Farm {
    let scratch = TestDir::under(
        scratch_root(PathBuf::from(env!("CARGO_TARGET_TMPDIR"))).join("kbf-blackbox"),
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

/// The action that reads `input` as `in/input.bin` and has `out/file.txt` written,
/// uploaded through `cas`; its digest.
async fn upload_action(cas: &Transfers, image: &str, input: &[u8]) -> Digest {
    let put = async |bytes: Vec<u8>| cas.put(bytes).await.expect("upload");
    let inner = Directory {
        files: vec![FileNode {
            name: "input.bin".to_owned(),
            digest: Some(put(input.to_vec()).await),
            ..FileNode::default()
        }],
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

/// Executes `action` until its operation is done.
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
        let channel = channel(&farm).await;
        let cas = Transfers::over(channel.clone(), TransferConfig::default());
        let action = upload_action(&cas, farm.image(), &input_bytes()).await;
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
