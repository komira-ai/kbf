//! The REAPI helpers, against an in-memory store and a TLS `Execution` service that answers
//! as scripted.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::reapi::execution_server::{Execution, ExecutionServer};
use kbf_proto::reapi::{
    Action, Command as ReapiCommand, Digest, Directory, ExecuteRequest, ExecuteResponse,
    WaitExecutionRequest,
};
use kbf_testkit::tree::{Job, Tree};
use kbf_transfer::{Cas, CasError, digest_of};
use prost::Message as _;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Identity, Server, ServerTlsConfig};

use super::{connect, execute, upload_action};

/// A store that keeps what it is given, by digest.
#[derive(Default)]
struct MemoryCas {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryCas {
    fn blob(&self, digest: &Digest) -> Vec<u8> {
        self.blobs.lock().expect("lock")[&digest.hash].clone()
    }
}

impl Cas for MemoryCas {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        Ok(self.blob(digest))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let digest = digest_of(&bytes);
        self.blobs
            .lock()
            .expect("lock")
            .insert(digest.hash.clone(), bytes);
        Ok(digest)
    }
}

fn job() -> Job {
    Job {
        input: Tree {
            files: vec![
                ("b".to_owned(), b"second".to_vec()),
                ("a".to_owned(), b"first".to_vec()),
            ],
            links: vec![
                ("z".to_owned(), "a".to_owned()),
                ("y".to_owned(), "b".to_owned()),
            ],
            dirs: vec![
                ("two".to_owned(), Tree::with_input("f", b"x")),
                ("one".to_owned(), Tree::default()),
            ],
        },
        outputs: vec!["out/file.txt".to_owned()],
        timeout: Some(Duration::from_secs(90)),
        cacheable: false,
    }
}

async fn action_of(cas: &MemoryCas, job: &Job) -> Action {
    let digest = upload_action(cas, "image:1", job).await;
    Action::decode(cas.blob(&digest).as_slice()).expect("an action")
}

/// Catches: an action that drops what the job says: the declared outputs, the timeout (in
/// whole seconds), the image property, `do_not_cache` for a job that is not cacheable (and
/// not set for one that is), and the `true` command the stand-in `crun` ignores.
#[tokio::test]
async fn an_action_carries_the_jobs_outputs_timeout_image_and_cache_flag() {
    let cas = MemoryCas::default();
    let action = action_of(&cas, &job()).await;
    let command = ReapiCommand::decode(
        cas.blob(action.command_digest.as_ref().expect("command"))
            .as_slice(),
    )
    .expect("a command");
    assert_eq!(command.arguments, ["true"]);
    assert_eq!(command.output_paths, ["out/file.txt"]);
    assert_eq!(action.timeout.map(|t| (t.seconds, t.nanos)), Some((90, 0)));
    assert!(action.do_not_cache);
    let property = &action.platform.expect("a platform").properties[0];
    assert_eq!(
        (property.name.as_str(), property.value.as_str()),
        ("container-image", "image:1")
    );

    let cacheable = Job {
        cacheable: true,
        timeout: None,
        ..job()
    };
    let action = action_of(&cas, &cacheable).await;
    assert!(!action.do_not_cache);
    assert_eq!(action.timeout, None);
}

/// Catches: an input root that loses a file, a link or a directory, that lists them out of
/// name order (REAPI requires each list sorted, and a server may refuse the tree), or whose
/// child directory digests do not lead to the child.
#[tokio::test]
async fn the_input_root_holds_every_entry_sorted_by_name() {
    let cas = MemoryCas::default();
    let action = action_of(&cas, &job()).await;
    let root = Directory::decode(
        cas.blob(action.input_root_digest.as_ref().expect("root"))
            .as_slice(),
    )
    .expect("a root");
    let names = |v: Vec<&str>| v.into_iter().map(str::to_owned).collect::<Vec<_>>();
    assert_eq!(
        names(root.files.iter().map(|f| f.name.as_str()).collect()),
        ["a", "b"]
    );
    assert_eq!(
        names(root.symlinks.iter().map(|f| f.name.as_str()).collect()),
        ["y", "z"]
    );
    assert_eq!(root.symlinks[1].target, "a");
    assert_eq!(
        names(root.directories.iter().map(|f| f.name.as_str()).collect()),
        ["one", "two"]
    );
    assert_eq!(
        cas.blob(root.files[0].digest.as_ref().expect("digest")),
        b"first"
    );
    let two = Directory::decode(
        cas.blob(root.directories[1].digest.as_ref().expect("digest"))
            .as_slice(),
    )
    .expect("the child");
    assert_eq!(two.directories[0].name, "in");
}

/// An `Execution` service that answers an `Execute` by the hash of the action it names:
/// `"error"` ends the operation with no response; anything else sends one update that is
/// not done, then the response (exit code 7, a cached result).
struct Scripted;

#[tonic::async_trait]
impl Execution for Scripted {
    type ExecuteStream =
        futures::stream::Iter<std::vec::IntoIter<Result<Operation, tonic::Status>>>;
    type WaitExecutionStream = Self::ExecuteStream;

    async fn execute(
        &self,
        request: tonic::Request<ExecuteRequest>,
    ) -> Result<tonic::Response<Self::ExecuteStream>, tonic::Status> {
        let hash = request
            .into_inner()
            .action_digest
            .map(|d| d.hash)
            .unwrap_or_default();
        let running = Operation {
            name: "operations/1".to_owned(),
            ..Operation::default()
        };
        let mut done = Operation {
            name: "operations/1".to_owned(),
            done: true,
            ..Operation::default()
        };
        if hash != "error" {
            let response = ExecuteResponse {
                cached_result: true,
                result: Some(kbf_proto::reapi::ActionResult {
                    exit_code: 7,
                    ..Default::default()
                }),
                ..ExecuteResponse::default()
            };
            done.result = Some(operation::Result::Response(prost_types::Any {
                type_url: String::new(),
                value: response.encode_to_vec(),
            }));
        }
        Ok(tonic::Response::new(futures::stream::iter(vec![
            Ok(running),
            Ok(done),
        ])))
    }

    async fn wait_execution(
        &self,
        _: tonic::Request<WaitExecutionRequest>,
    ) -> Result<tonic::Response<Self::WaitExecutionStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("not scripted"))
    }
}

/// Serves [`Scripted`] under a certificate of the CA the harness makes for `localhost`; the
/// channel [`connect`] makes to it.
async fn channel(name: &str) -> tonic::transport::Channel {
    let etc = crate::scratch(name);
    let pki = crate::pki::Pki::new();
    pki.write_ca(&etc);
    pki.issue_reapi(&etc, &["localhost", "127.0.0.1"]);
    let read = |file: &str| std::fs::read(etc.join(file)).expect("read");
    let tls =
        ServerTlsConfig::new().identity(Identity::from_pem(read("reapi.pem"), read("reapi.key")));
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("address")).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(async move {
        Server::builder()
            .tls_config(tls)
            .expect("TLS")
            .add_service(ExecutionServer::new(Scripted))
            .serve_with_incoming(incoming)
            .await
            .expect("serve");
    });
    connect(&read("ca.pem"), &addr.to_string()).await
}

/// Catches: a channel that does not trust the farm CA or the name `localhost` (the
/// connection fails), and an `execute` that returns at the first update rather than the
/// one that is done, or that misreads the response it carries.
#[tokio::test]
async fn execute_waits_for_the_done_operation_and_decodes_its_response() {
    let channel = channel("reapi-execute").await;
    let response = execute(channel, digest_of(b"anything")).await;
    assert!(response.cached_result, "{response:?}");
    assert_eq!(response.result.expect("a result").exit_code, 7);
}

/// Catches: an operation that is done with no response being passed off as a result.
#[tokio::test]
#[should_panic(expected = "no response")]
async fn an_operation_done_with_no_response_is_a_failure() {
    let channel = channel("reapi-no-response").await;
    let error = Digest {
        hash: "error".to_owned(),
        size_bytes: 0,
    };
    let _ = execute(channel, error).await;
}
