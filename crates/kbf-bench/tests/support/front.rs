//! A fake frontend in this process for `noop`: a CAS that keeps what BatchUpdateBlobs
//! sends, and an Execution service that reads each Execute's Action, Command and input
//! root back from it, logs the run, and answers as the test's script says.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream::{self, BoxStream};
use futures::{FutureExt as _, StreamExt as _};
use kbf_bench::noop::{ENV, PROGRAM};
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::google::rpc;
use kbf_proto::reapi::content_addressable_storage_server::{
    ContentAddressableStorage, ContentAddressableStorageServer,
};
use kbf_proto::reapi::execution_server::{Execution, ExecutionServer};
use kbf_proto::reapi::{
    Action, BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, Command, Digest, Directory, ExecuteRequest, ExecuteResponse,
    FindMissingBlobsRequest, FindMissingBlobsResponse, GetChunkMappingRequest,
    GetChunkMappingResponse, GetTreeRequest, GetTreeResponse, RegisterChunkMappingRequest,
    RegisterChunkMappingResponse, SpliceBlobRequest, SpliceBlobResponse, SplitBlobRequest,
    SplitBlobResponse, WaitExecutionRequest, batch_update_blobs_response,
};
use prost::Message as _;
use prost_types::Any;
use tokio::sync::Barrier;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status, Streaming};

/// How the fake answers one Execute.
#[derive(Clone, Debug)]
pub enum Reply {
    /// A first update, then the done operation carrying this response.
    Done(ExecuteResponse),
    /// The call answered `before` the first update, and the done operation carrying
    /// this response `after` it, on the runtime's clock.
    DoneAfter(ExecuteResponse, Duration, Duration),
    /// The call refused.
    Refuse(Status),
    /// A first update, then the stream fails.
    Break(Status),
    /// A first update, then the stream ends.
    End,
    /// The done operation with an error.
    Failed(rpc::Status),
    /// The done operation with no result.
    NoResult,
    /// The done operation with a response that does not decode.
    Garbage,
}

/// How the fake's BatchUpdateBlobs answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upload {
    Keep,
    /// The call refused.
    RefuseCall,
    /// The call answered, its last blob refused.
    RefuseLast,
}

/// One Execute the fake saw.
#[derive(Clone, Debug)]
pub struct Ran {
    pub arm: String,
    pub request: ExecuteRequest,
    pub action: Action,
    pub command: Command,
    pub root: Directory,
    /// The bytes of the input root's one file.
    pub program: Vec<u8>,
    /// `<seed>-<arm>-<phase>-<run>` split.
    pub env: Vec<String>,
}

/// What every fake of a test shares: what was run, in order, and how many Executes were
/// open at once.
#[derive(Default)]
pub struct Shared {
    pub ran: Mutex<Vec<Ran>>,
    open: AtomicUsize,
    pub peak: AtomicUsize,
    /// When set, every Execute waits here before it is answered.
    pub gate: Mutex<Option<Arc<Barrier>>>,
    /// Calls to BatchUpdateBlobs, per arm.
    pub uploads: Mutex<BTreeMap<String, usize>>,
    /// The hash of the blob [`Upload::RefuseLast`] refused.
    pub refused: Mutex<Option<String>>,
}

type Script = Arc<dyn Fn(&Ran) -> Reply + Send + Sync>;

#[derive(Clone)]
struct Fake {
    arm: String,
    blobs: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    script: Script,
    upload: Upload,
    shared: Arc<Shared>,
}

/// Starts a fake frontend for arm `arm`; returns its address.
pub fn start(
    arm: &str,
    shared: &Arc<Shared>,
    upload: Upload,
    script: impl Fn(&Ran) -> Reply + Send + Sync + 'static,
) -> SocketAddr {
    let fake = Fake {
        arm: arm.to_owned(),
        blobs: Arc::default(),
        script: Arc::new(script),
        upload,
        shared: shared.clone(),
    };
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("addr");
    tokio::spawn(
        Server::builder()
            .add_service(ExecutionServer::new(fake.clone()))
            .add_service(ContentAddressableStorageServer::new(fake))
            .serve_with_incoming(incoming),
    );
    addr
}

/// A response of a run of `./noop` that exited `exit_code`.
#[must_use]
pub fn exited(exit_code: i32) -> ExecuteResponse {
    ExecuteResponse {
        result: Some(kbf_proto::reapi::ActionResult {
            exit_code,
            ..kbf_proto::reapi::ActionResult::default()
        }),
        ..ExecuteResponse::default()
    }
}

impl Fake {
    fn blob(&self, digest: Option<&Digest>) -> Result<Vec<u8>, Status> {
        let digest = digest.ok_or_else(|| Status::invalid_argument("no digest"))?;
        let blobs = self.blobs.lock().expect("blobs");
        blobs
            .get(&digest.hash)
            .cloned()
            .ok_or_else(|| Status::failed_precondition(format!("missing {}", digest.hash)))
    }

    fn read(&self, request: ExecuteRequest) -> Result<Ran, Status> {
        let decode = |e: prost::DecodeError| Status::internal(e.to_string());
        let action = Action::decode(self.blob(request.action_digest.as_ref())?.as_slice())
            .map_err(decode)?;
        let command = Command::decode(self.blob(action.command_digest.as_ref())?.as_slice())
            .map_err(decode)?;
        let root = Directory::decode(self.blob(action.input_root_digest.as_ref())?.as_slice())
            .map_err(decode)?;
        let file = root
            .files
            .iter()
            .find(|f| f.name == PROGRAM)
            .ok_or_else(|| Status::failed_precondition("no program"))?;
        let program = self.blob(file.digest.as_ref())?;
        let var = command
            .environment_variables
            .iter()
            .find(|v| v.name == ENV)
            .ok_or_else(|| Status::failed_precondition("no ENV"))?;
        let env = var.value.split('-').map(str::to_owned).collect();
        Ok(Ran {
            arm: self.arm.clone(),
            request,
            action,
            command,
            root,
            program,
            env,
        })
    }
}

/// An update of the operation `name`, not done.
fn update(name: &str) -> Operation {
    Operation {
        name: name.to_owned(),
        ..Operation::default()
    }
}

fn done(name: &str, result: Option<operation::Result>) -> Operation {
    Operation {
        name: name.to_owned(),
        done: true,
        result,
        ..Operation::default()
    }
}

fn response(response: &ExecuteResponse) -> Option<operation::Result> {
    Some(operation::Result::Response(Any {
        type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse".to_owned(),
        value: response.encode_to_vec(),
    }))
}

type Updates = BoxStream<'static, Result<Operation, Status>>;

#[tonic::async_trait]
impl Execution for Fake {
    type ExecuteStream = Updates;
    type WaitExecutionStream = Updates;

    async fn execute(
        &self,
        request: Request<ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let ran = self.read(request.into_inner())?;
        let open = self.shared.open.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared.peak.fetch_max(open, Ordering::SeqCst);
        self.shared.ran.lock().expect("ran").push(ran.clone());
        let gate = self.shared.gate.lock().expect("gate").clone();
        if let Some(gate) = gate {
            gate.wait().await;
        }
        self.shared.open.fetch_sub(1, Ordering::SeqCst);
        let name = format!("operations/{}", ran.env[1..].join("-"));
        let first = || Ok(update(&name));
        let updates: Updates = match (self.script)(&ran) {
            Reply::Done(r) => stream::iter([first(), Ok(done(&name, response(&r)))]).boxed(),
            Reply::DoneAfter(r, before, after) => {
                tokio::time::sleep(before).await;
                let last = done(&name, response(&r));
                stream::iter([first()])
                    .chain(tokio::time::sleep(after).map(|()| Ok(last)).into_stream())
                    .boxed()
            }
            Reply::Refuse(status) => return Err(status),
            Reply::Break(status) => stream::iter([first(), Err(status)]).boxed(),
            Reply::End => stream::iter([first()]).boxed(),
            Reply::Failed(status) => {
                let failed = Some(operation::Result::Error(status));
                stream::iter([first(), Ok(done(&name, failed))]).boxed()
            }
            Reply::NoResult => stream::iter([first(), Ok(done(&name, None))]).boxed(),
            Reply::Garbage => {
                let garbage = Some(operation::Result::Response(Any {
                    type_url: String::new(),
                    value: vec![0xff, 0xff],
                }));
                stream::iter([first(), Ok(done(&name, garbage))]).boxed()
            }
        };
        Ok(Response::new(updates))
    }

    async fn wait_execution(
        &self,
        _: Request<WaitExecutionRequest>,
    ) -> Result<Response<Self::WaitExecutionStream>, Status> {
        Err(Status::unimplemented("fake"))
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for Fake {
    async fn find_missing_blobs(
        &self,
        _: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        Err(Status::unimplemented("fake"))
    }

    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        *self
            .shared
            .uploads
            .lock()
            .expect("uploads")
            .entry(self.arm.clone())
            .or_default() += 1;
        if self.upload == Upload::RefuseCall {
            return Err(Status::resource_exhausted("the fake is full"));
        }
        let requests = request.into_inner().requests;
        let last = requests.len() - 1;
        let mut responses = Vec::new();
        for (at, blob) in requests.into_iter().enumerate() {
            let refused = (self.upload == Upload::RefuseLast) & (at == last);
            let digest = blob.digest.unwrap_or_default();
            if refused {
                *self.shared.refused.lock().expect("refused") = Some(digest.hash.clone());
            } else {
                let mut blobs = self.blobs.lock().expect("blobs");
                blobs.insert(digest.hash.clone(), blob.data);
            }
            responses.push(batch_update_blobs_response::Response {
                digest: Some(digest),
                status: Some(rpc::Status {
                    code: if refused { 3 } else { 0 },
                    message: if refused { "too odd" } else { "" }.to_owned(),
                    ..rpc::Status::default()
                }),
            });
        }
        Ok(Response::new(BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        _: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        Err(Status::unimplemented("fake"))
    }

    type GetTreeStream = stream::Iter<std::vec::IntoIter<Result<GetTreeResponse, Status>>>;

    async fn get_tree(
        &self,
        _: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        Err(Status::unimplemented("fake"))
    }

    async fn split_blob(
        &self,
        _: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        Err(Status::unimplemented("fake"))
    }

    type GetChunkMappingStream =
        stream::Iter<std::vec::IntoIter<Result<GetChunkMappingResponse, Status>>>;

    async fn get_chunk_mapping(
        &self,
        _: Request<GetChunkMappingRequest>,
    ) -> Result<Response<Self::GetChunkMappingStream>, Status> {
        Err(Status::unimplemented("fake"))
    }

    async fn splice_blob(
        &self,
        _: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        Err(Status::unimplemented("fake"))
    }

    async fn register_chunk_mapping(
        &self,
        _: Request<Streaming<RegisterChunkMappingRequest>>,
    ) -> Result<Response<RegisterChunkMappingResponse>, Status> {
        Err(Status::unimplemented("fake"))
    }
}
