//! A client of a farm's frontend for the black-box test crates (`kbf-farm-test`,
//! `kbf-daemon-test`): the REAPI channel under the farm CA ([`connect`]), one action built
//! from a [`Job`] and uploaded through a [`Cas`] ([`upload_action`]), and its `Execute`
//! ([`execute`]).
//!
//! These are the calls a deployer's build client makes, with nothing of the product's
//! server or node code; the farm itself is [`crate::farm::Farm`]. They live here, not in
//! `kbf-testkit`, because they name the REAPI messages and a gRPC channel, and `kbf-testkit`
//! links no other kbf crate.

use std::future::Future;
use std::pin::Pin;

use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, Command as ReapiCommand, Digest, Directory, DirectoryNode, ExecuteRequest,
    ExecuteResponse, FileNode, Platform, SymlinkNode,
};
use kbf_testkit::tree::{Job, Tree};
use kbf_transfer::Cas;
use prost::Message as _;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

/// A channel to the REAPI listener at `reapi` (`host:port`), TLS under the CA `ca_pem`
/// (a farm's [`crate::farm::Farm::ca_pem`] and [`crate::farm::Farm::reapi`]).
///
/// # Panics
/// When the address is not an endpoint, or the listener cannot be reached: a test cannot
/// go on without it.
pub async fn connect(ca_pem: &[u8], reapi: &str) -> Channel {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_pem))
        .domain_name("localhost");
    Endpoint::from_shared(format!("https://{reapi}"))
        .expect("endpoint")
        .tls_config(tls)
        .expect("TLS")
        .connect()
        .await
        .expect("connect to the frontend")
}

/// Uploads `tree` (children first); its digest.
fn upload_tree<'a, C: Cas>(
    cas: &'a C,
    tree: &'a Tree,
) -> Pin<Box<dyn Future<Output = Digest> + Send + 'a>> {
    Box::pin(async move {
        let mut dir = Directory::default();
        for (name, bytes) in &tree.files {
            let digest = cas.put(bytes.clone()).await.expect("upload");
            dir.files.push(FileNode {
                name: name.clone(),
                digest: Some(digest),
                ..FileNode::default()
            });
        }
        for (name, target) in &tree.links {
            dir.symlinks.push(SymlinkNode {
                name: name.clone(),
                target: target.clone(),
                ..SymlinkNode::default()
            });
        }
        for (name, child) in &tree.dirs {
            dir.directories.push(DirectoryNode {
                name: name.clone(),
                digest: Some(upload_tree(cas, child).await),
            });
        }
        // REAPI: each list is sorted by name.
        dir.files.sort_by(|a, b| a.name.cmp(&b.name));
        dir.directories.sort_by(|a, b| a.name.cmp(&b.name));
        dir.symlinks.sort_by(|a, b| a.name.cmp(&b.name));
        cas.put(dir.encode_to_vec()).await.expect("upload")
    })
}

/// Uploads the action of `job`, naming `image` as its container image; its digest. Unless
/// the job is cacheable the action is `do_not_cache`, so every run is a run.
///
/// The stand-in `crun` runs its planted steps whatever the command's arguments are, so the
/// command is `true`.
///
/// # Panics
/// When an upload fails.
pub async fn upload_action<C: Cas>(cas: &C, image: &str, job: &Job) -> Digest {
    let root = upload_tree(cas, &job.input).await;
    let command = ReapiCommand {
        arguments: vec!["true".to_owned()],
        output_paths: job.outputs.clone(),
        ..ReapiCommand::default()
    };
    let action = Action {
        command_digest: Some(cas.put(command.encode_to_vec()).await.expect("upload")),
        input_root_digest: Some(root),
        timeout: job.timeout.map(|t| prost_types::Duration {
            seconds: i64::try_from(t.as_secs()).expect("seconds"),
            nanos: 0,
        }),
        do_not_cache: !job.cacheable,
        platform: Some(Platform {
            properties: vec![Property {
                name: "container-image".to_owned(),
                value: image.to_owned(),
            }],
        }),
        ..Action::default()
    };
    cas.put(action.encode_to_vec()).await.expect("upload")
}

/// Executes `action` until its operation is done.
///
/// # Panics
/// When the call or its stream fails, or the operation ends with no response.
pub async fn execute(reapi: Channel, action: Digest) -> ExecuteResponse {
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

#[cfg(test)]
#[path = "reapi_tests.rs"]
mod tests;
