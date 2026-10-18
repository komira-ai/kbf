//! The CAS smoke against a REAPI server that plays the local listener, with a knob for
//! each way it can be wrong. Each knob is the mutant of one check: a smoke that does not
//! make that check passes the broken server, and the test for it goes red.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::Stream;
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use kbf_proto::reapi::action_cache_server::{ActionCache, ActionCacheServer};
use kbf_proto::reapi::content_addressable_storage_server::{
    ContentAddressableStorage, ContentAddressableStorageServer,
};
use kbf_proto::reapi::{
    ActionResult, BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, FindMissingBlobsRequest, FindMissingBlobsResponse,
    GetActionResultRequest, GetChunkMappingRequest, GetChunkMappingResponse, GetTreeRequest,
    GetTreeResponse, RegisterChunkMappingRequest, RegisterChunkMappingResponse, SpliceBlobRequest,
    SpliceBlobResponse, SplitBlobRequest, SplitBlobResponse, UpdateActionResultRequest,
};
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status, Streaming};

use super::smoke::CasSmoke;
use super::{Smoke, SmokeResult};

/// How the action cache answers a lookup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Ac {
    /// NOT_FOUND, as a cache without the entry.
    #[default]
    Miss,
    /// A result, for any digest.
    Hit,
    /// INTERNAL.
    Error,
    /// UNAVAILABLE.
    Unavailable,
}

/// What the server gets wrong; the default is a correct server.
#[derive(Clone, Copy, Debug, Default)]
struct Knobs {
    /// A write is acknowledged and stores nothing.
    drop_writes: bool,
    /// A write reports one byte fewer than it stored.
    short_commit: bool,
    /// FindMissing reports every digest present.
    find_all_present: bool,
    /// FindMissing reports every digest missing.
    find_all_missing: bool,
    /// A read returns the blob with its first byte changed.
    corrupt_reads: bool,
    /// A read returns the blob and one extra byte.
    long_reads: bool,
    /// A read never answers.
    hang_reads: bool,
    ac: Ac,
}

#[derive(Default)]
struct State {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    /// The calls that came, by name.
    calls: Mutex<Vec<&'static str>>,
}

#[derive(Clone)]
struct Fake {
    knobs: Knobs,
    state: Arc<State>,
}

impl Fake {
    fn call(&self, name: &'static str) {
        self.state
            .calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(name);
    }
}

type ReadStream = Pin<Box<dyn Stream<Item = Result<ReadResponse, Status>> + Send>>;

#[tonic::async_trait]
impl ByteStream for Fake {
    type ReadStream = ReadStream;

    async fn read(&self, request: Request<ReadRequest>) -> Result<Response<ReadStream>, Status> {
        self.call("read");
        if self.knobs.hang_reads {
            std::future::pending::<()>().await;
        }
        let name = request.into_inner().resource_name;
        // `blobs/{hash}/{size}`
        let hash = name.split('/').nth(1).unwrap_or_default();
        let Some(mut bytes) = self
            .state
            .blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(hash)
            .cloned()
        else {
            return Err(Status::not_found(name));
        };
        if self.knobs.corrupt_reads {
            bytes[0] ^= 1;
        }
        if self.knobs.long_reads {
            bytes.push(0);
        }
        let (a, b) = bytes.split_at(bytes.len() / 2);
        let messages = vec![
            Ok(ReadResponse { data: a.to_vec() }),
            Ok(ReadResponse { data: b.to_vec() }),
        ];
        Ok(Response::new(Box::pin(futures::stream::iter(messages))))
    }

    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        self.call("write");
        let mut stream = request.into_inner();
        let mut name = String::new();
        let mut bytes = Vec::new();
        while let Some(m) = stream.message().await? {
            if name.is_empty() {
                name = m.resource_name;
            }
            bytes.extend_from_slice(&m.data);
        }
        // `uploads/{uuid}/blobs/{hash}/{size}`
        let hash = name.split('/').nth(3).unwrap_or_default().to_owned();
        let size = bytes.len() as i64;
        if !self.knobs.drop_writes {
            self.state
                .blobs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(hash, bytes);
        }
        Ok(Response::new(WriteResponse {
            committed_size: if self.knobs.short_commit {
                size - 1
            } else {
                size
            },
        }))
    }

    async fn query_write_status(
        &self,
        _: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for Fake {
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        self.call("find_missing");
        let held = self
            .state
            .blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let missing = request
            .into_inner()
            .blob_digests
            .into_iter()
            .filter(|d| {
                !self.knobs.find_all_present
                    && (self.knobs.find_all_missing || !held.contains_key(&d.hash))
            })
            .collect();
        Ok(Response::new(FindMissingBlobsResponse {
            missing_blob_digests: missing,
        }))
    }

    async fn batch_update_blobs(
        &self,
        _: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }

    async fn batch_read_blobs(
        &self,
        _: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }

    type GetTreeStream = Pin<Box<dyn Stream<Item = Result<GetTreeResponse, Status>> + Send>>;

    async fn get_tree(
        &self,
        _: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        Err(Status::unimplemented("not used"))
    }

    async fn split_blob(
        &self,
        _: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }

    type GetChunkMappingStream =
        Pin<Box<dyn Stream<Item = Result<GetChunkMappingResponse, Status>> + Send>>;

    async fn get_chunk_mapping(
        &self,
        _: Request<GetChunkMappingRequest>,
    ) -> Result<Response<Self::GetChunkMappingStream>, Status> {
        Err(Status::unimplemented("not used"))
    }

    async fn splice_blob(
        &self,
        _: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }

    async fn register_chunk_mapping(
        &self,
        _: Request<Streaming<RegisterChunkMappingRequest>>,
    ) -> Result<Response<RegisterChunkMappingResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }
}

#[tonic::async_trait]
impl ActionCache for Fake {
    async fn get_action_result(
        &self,
        _: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        self.call("get_action_result");
        match self.knobs.ac {
            Ac::Miss => Err(Status::not_found("no such action")),
            Ac::Hit => Ok(Response::new(ActionResult::default())),
            Ac::Error => Err(Status::internal("broken for the test")),
            Ac::Unavailable => Err(Status::unavailable("down for the test")),
        }
    }

    async fn update_action_result(
        &self,
        _: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        Err(Status::unimplemented("not used"))
    }
}

/// A running fake: its URL, its state, and the runtime that serves it.
struct Served {
    url: String,
    state: Arc<State>,
    _rt: tokio::runtime::Runtime,
}

fn serve(knobs: Knobs) -> Served {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let fake = Fake {
        knobs,
        state: Arc::default(),
    };
    let state = Arc::clone(&fake.state);
    let incoming = {
        let _enter = rt.enter();
        TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind")
    };
    let addr = incoming.local_addr().expect("an address");
    rt.spawn(
        Server::builder()
            .add_service(ByteStreamServer::new(fake.clone()))
            .add_service(ContentAddressableStorageServer::new(fake.clone()))
            .add_service(ActionCacheServer::new(fake))
            .serve_with_incoming(incoming),
    );
    Served {
        url: format!("http://{addr}"),
        state,
        _rt: rt,
    }
}

fn smoke(url: &str, secs: u64) -> CasSmoke {
    CasSmoke::new(url, Duration::from_secs(secs)).expect("a URL")
}

/// Runs the smoke against a fake with `knobs`; the verdict and the calls the fake saw.
fn verdict(knobs: Knobs) -> (SmokeResult, Vec<&'static str>) {
    let served = serve(knobs);
    let result = smoke(&served.url, 30).run();
    let calls = served
        .state
        .calls
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    (result, calls)
}

/// Asserts a failure whose message names `what`.
#[track_caller]
fn fails_with(knobs: Knobs, what: &str) {
    let (result, _) = verdict(knobs);
    match result {
        SmokeResult::Fail(why) => assert!(why.contains(what), "{why:?} does not name {what:?}"),
        other => panic!("expected a failure naming {what:?}, got {other:?}"),
    }
}

/// Proves a correct server passes, and that the smoke made every call: a write, a
/// FindMissing, a read and an action-cache lookup. Catches a smoke that returns Pass
/// without talking to the server (or one that skips a leg).
#[test]
fn a_correct_server_passes_after_every_call() {
    let (result, calls) = verdict(Knobs::default());
    assert_eq!(result, SmokeResult::Pass);
    assert_eq!(
        calls,
        ["write", "find_missing", "read", "get_action_result"]
    );
}

/// Catches a smoke that does not look at the size a write commits (a server that
/// acknowledges fewer bytes than were sent passes).
#[test]
fn a_write_that_commits_fewer_bytes_fails() {
    fails_with(
        Knobs {
            short_commit: true,
            ..Knobs::default()
        },
        "committed",
    );
}

/// Catches a smoke that does not ask whether the blob it wrote is there: a server that
/// loses writes but answers "missing" for nothing passes, and so does one that says the
/// blob is missing.
#[test]
fn a_written_blob_reported_missing_fails() {
    fails_with(
        Knobs {
            find_all_missing: true,
            ..Knobs::default()
        },
        "FindMissingBlobs",
    );
}

/// Catches a FindMissing that is never asked about a blob that is not there: a server
/// that reports everything present passes.
#[test]
fn an_absent_blob_reported_present_fails() {
    fails_with(
        Knobs {
            find_all_present: true,
            ..Knobs::default()
        },
        "FindMissingBlobs",
    );
}

/// Catches a smoke that takes a write's acknowledgement for the blob being stored: a
/// server that acknowledges and keeps nothing.
#[test]
fn an_acknowledged_write_that_stored_nothing_fails() {
    fails_with(
        Knobs {
            drop_writes: true,
            ..Knobs::default()
        },
        "FindMissingBlobs",
    );
}

/// Catches a smoke that reads the blob back and does not compare it: a read that changes
/// a byte.
#[test]
fn a_blob_read_back_with_a_changed_byte_fails() {
    fails_with(
        Knobs {
            corrupt_reads: true,
            ..Knobs::default()
        },
        "differ",
    );
}

/// Catches a comparison that checks only the first `size` bytes.
#[test]
fn a_blob_read_back_with_an_extra_byte_fails() {
    fails_with(
        Knobs {
            long_reads: true,
            ..Knobs::default()
        },
        "differ",
    );
}

/// Catches an action-cache check that takes a hit on a random digest as fine, and one
/// that takes any error as the miss: only NOT_FOUND is a miss.
#[test]
fn an_action_cache_hit_on_a_random_digest_fails() {
    fails_with(
        Knobs {
            ac: Ac::Hit,
            ..Knobs::default()
        },
        "GetActionResult",
    );
}

#[test]
fn an_action_cache_error_is_not_a_miss() {
    for ac in [Ac::Error, Ac::Unavailable] {
        fails_with(
            Knobs {
                ac,
                ..Knobs::default()
            },
            "GetActionResult",
        );
    }
}

/// Proves the time bound: a server that never answers a read fails the smoke in about the
/// timeout, naming it. Catches a smoke with no bound (this test would hang).
#[test]
fn a_server_that_never_answers_fails_at_the_timeout() {
    let served = serve(Knobs {
        hang_reads: true,
        ..Knobs::default()
    });
    let start = Instant::now();
    let result = smoke(&served.url, 1).run();
    assert!(
        matches!(&result, SmokeResult::Fail(why) if why.contains("timed out")),
        "{result:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(20));
}

/// Proves nothing listening is a failure, never a skip or a pass.
#[test]
fn nothing_listening_fails() {
    let url = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    assert!(matches!(smoke(&url, 5).run(), SmokeResult::Fail(_)));
}

/// Proves the address must be a plain `http://` URL: a bare host:port or `https://` is a
/// usage error at parse time, not a failure at the end of a deploy.
#[test]
fn only_an_http_url_is_accepted() {
    let t = Duration::from_secs(1);
    assert!(CasSmoke::new("http://127.0.0.1:8980", t).is_ok());
    for bad in ["127.0.0.1:8980", "https://127.0.0.1:8980", "http://", ""] {
        assert!(CasSmoke::new(bad, t).is_err(), "{bad:?}");
    }
}
