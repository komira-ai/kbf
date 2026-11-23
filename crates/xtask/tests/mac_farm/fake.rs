//! A fake farm in this process for `mac-farm`, on loopback with port 0: a CAS and ByteStream
//! that keep what is uploaded and serve it back, and an Execution service that answers each
//! Execute and WaitExecution with the next of the answers a test queued.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::google::rpc;
use kbf_proto::reapi::content_addressable_storage_server::{
    ContentAddressableStorage, ContentAddressableStorageServer,
};
use kbf_proto::reapi::execution_server::{Execution, ExecutionServer};
use kbf_proto::reapi::execution_stage::Value as Stage;
use kbf_proto::reapi::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, ExecuteOperationMetadata, ExecuteRequest, ExecuteResponse,
    FindMissingBlobsRequest, FindMissingBlobsResponse, GetChunkMappingRequest,
    GetChunkMappingResponse, GetTreeRequest, GetTreeResponse, RegisterChunkMappingRequest,
    RegisterChunkMappingResponse, SpliceBlobRequest, SpliceBlobResponse, SplitBlobRequest,
    SplitBlobResponse, WaitExecutionRequest, batch_update_blobs_response,
};
use prost::Message as _;
use prost_types::Any;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status, Streaming};

/// How the fake answers one Execute or WaitExecution.
pub enum Answer {
    /// The call refused.
    Refuse(Status),
    /// The call answered with these updates, then the stream ends.
    Updates(Vec<Result<Operation, Status>>),
}

/// What the fake keeps and what a test reads back.
#[derive(Default)]
pub struct State {
    pub blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    pub answers: Mutex<VecDeque<Answer>>,
    pub executes: Mutex<Vec<ExecuteRequest>>,
    pub waits: Mutex<Vec<String>>,
    pub finds: AtomicUsize,
    pub batches: AtomicUsize,
    /// The resource name of each ByteStream write and how many messages it had.
    pub writes: Mutex<Vec<(String, usize)>>,
    pub reads: Mutex<Vec<String>>,
    /// A blob the batch upload refuses.
    pub refuse: Mutex<Option<String>>,
    /// The ByteStream write commits one byte less than it was sent.
    pub short: AtomicBool,
}

impl State {
    pub fn queue(&self, answer: Answer) {
        self.answers.lock().expect("answers").push_back(answer);
    }
}

#[derive(Clone)]
struct Fake(Arc<State>);

/// Starts the fake on `runtime`; its address.
pub fn start(runtime: &tokio::runtime::Runtime, state: &Arc<State>) -> SocketAddr {
    let fake = Fake(state.clone());
    let _guard = runtime.enter();
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("addr");
    runtime.spawn(
        Server::builder()
            .add_service(ExecutionServer::new(fake.clone()))
            .add_service(ContentAddressableStorageServer::new(fake.clone()))
            .add_service(ByteStreamServer::new(fake))
            .serve_with_incoming(incoming),
    );
    addr
}

/// An update of operation `name` at `stage`, not done.
pub fn at(name: &str, stage: Stage) -> Result<Operation, Status> {
    let meta = ExecuteOperationMetadata {
        stage: stage as i32,
        ..ExecuteOperationMetadata::default()
    };
    Ok(Operation {
        name: name.to_owned(),
        metadata: Some(Any {
            type_url:
                "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteOperationMetadata"
                    .to_owned(),
            value: meta.encode_to_vec(),
        }),
        ..Operation::default()
    })
}

/// The done operation `name` with `result`.
pub fn done(name: &str, result: Option<operation::Result>) -> Result<Operation, Status> {
    Ok(Operation {
        name: name.to_owned(),
        done: true,
        result,
        ..Operation::default()
    })
}

/// The done operation `name` carrying `response`.
pub fn answered(name: &str, response: &ExecuteResponse) -> Result<Operation, Status> {
    done(
        name,
        Some(operation::Result::Response(Any {
            type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse"
                .to_owned(),
            value: response.encode_to_vec(),
        })),
    )
}

type Updates = BoxStream<'static, Result<Operation, Status>>;

impl Fake {
    fn next(&self) -> Result<Response<Updates>, Status> {
        match self.0.answers.lock().expect("answers").pop_front() {
            Some(Answer::Refuse(status)) => Err(status),
            Some(Answer::Updates(updates)) => Ok(Response::new(stream::iter(updates).boxed())),
            None => Err(Status::not_found("no such operation")),
        }
    }
}

#[tonic::async_trait]
impl Execution for Fake {
    type ExecuteStream = Updates;
    type WaitExecutionStream = Updates;

    async fn execute(&self, request: Request<ExecuteRequest>) -> Result<Response<Updates>, Status> {
        self.0
            .executes
            .lock()
            .expect("executes")
            .push(request.into_inner());
        self.next()
    }

    async fn wait_execution(
        &self,
        request: Request<WaitExecutionRequest>,
    ) -> Result<Response<Updates>, Status> {
        self.0
            .waits
            .lock()
            .expect("waits")
            .push(request.into_inner().name);
        self.next()
    }
}

/// The hash in a resource name `.../blobs/<hash>/<size>`.
fn hash_of(name: &str) -> String {
    let parts: Vec<&str> = name.split('/').collect();
    let at = parts.iter().position(|p| *p == "blobs").expect("blobs/");
    parts[at + 1].to_owned()
}

#[tonic::async_trait]
impl ByteStream for Fake {
    type ReadStream = stream::Iter<std::vec::IntoIter<Result<ReadResponse, Status>>>;

    async fn read(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        let name = request.into_inner().resource_name;
        self.0.reads.lock().expect("reads").push(name.clone());
        let blobs = self.0.blobs.lock().expect("blobs");
        let bytes = blobs
            .get(&hash_of(&name))
            .ok_or_else(|| Status::not_found(format!("no {name}")))?;
        let messages: Vec<_> = bytes
            .chunks(3)
            .map(|c| Ok(ReadResponse { data: c.to_vec() }))
            .collect();
        Ok(Response::new(stream::iter(messages)))
    }

    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let mut messages = request.into_inner();
        let mut name = String::new();
        let mut data = Vec::new();
        let mut count = 0;
        let mut finished = false;
        while let Some(message) = messages.message().await? {
            if finished {
                return Err(Status::invalid_argument("a message after finish_write"));
            }
            finished = message.finish_write;
            if count == 0 {
                name = message.resource_name;
            } else if !message.resource_name.is_empty() {
                return Err(Status::invalid_argument(
                    "a resource name after the first message",
                ));
            }
            if i64::try_from(data.len()).expect("len") != message.write_offset {
                return Err(Status::invalid_argument("out of order"));
            }
            data.extend(message.data);
            count += 1;
        }
        if !finished {
            return Err(Status::invalid_argument(
                "the last message is not finish_write",
            ));
        }
        self.0
            .writes
            .lock()
            .expect("writes")
            .push((name.clone(), count));
        let mut committed_size = i64::try_from(data.len()).expect("len");
        if self.0.short.load(Ordering::SeqCst) {
            committed_size -= 1;
        }
        self.0
            .blobs
            .lock()
            .expect("blobs")
            .insert(hash_of(&name), data);
        Ok(Response::new(WriteResponse { committed_size }))
    }

    async fn query_write_status(
        &self,
        _: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        Err(Status::unimplemented("fake"))
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for Fake {
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        self.0.finds.fetch_add(1, Ordering::SeqCst);
        let blobs = self.0.blobs.lock().expect("blobs");
        let missing_blob_digests = request
            .into_inner()
            .blob_digests
            .into_iter()
            .filter(|d| !blobs.contains_key(&d.hash))
            .collect();
        Ok(Response::new(FindMissingBlobsResponse {
            missing_blob_digests,
        }))
    }

    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        self.0.batches.fetch_add(1, Ordering::SeqCst);
        let refuse = self.0.refuse.lock().expect("refuse").clone();
        let mut responses = Vec::new();
        for blob in request.into_inner().requests {
            let digest = blob.digest.unwrap_or_default();
            let refused = refuse.as_deref() == Some(digest.hash.as_str());
            if !refused {
                self.0
                    .blobs
                    .lock()
                    .expect("blobs")
                    .insert(digest.hash.clone(), blob.data);
            }
            responses.push(batch_update_blobs_response::Response {
                digest: Some(digest),
                status: Some(rpc::Status {
                    code: if refused { 8 } else { 0 },
                    message: if refused { "the fake is full" } else { "" }.to_owned(),
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
