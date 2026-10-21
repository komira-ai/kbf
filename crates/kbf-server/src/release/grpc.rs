//! `kbf.update.v1.ReleaseService`: a stored tarball, streamed to a host by its digest, and
//! what a release holds.
//!
//! `Fetch` serves the file whose SHA-256 is the request's, in chunks of [`CHUNK_BYTES`].
//! `Describe` names the files of a commit ([`super::describe`]). The service holds no
//! trust: a host checks the digest and the build attestation itself. Admitting only nodes
//! (mutual TLS on the worker listener, one row of the role table per method) is the
//! listener's rule and is wired where the service is mounted.

use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use kbf_proto::update::release_service_server::{ReleaseService, ReleaseServiceServer};
use kbf_proto::update::{DescribeRequest, FetchChunk, FetchRequest, ReleaseDescription};
use kbf_types::ReleasePlatform;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tonic::{Request, Response, Status};

use super::describe::{DescribeError, describe};
use super::key::{Commit, KeyError, Sha256Hex};
use super::store::ReleaseStore;

/// The size of a chunk of `Fetch`: 1 MiB.
pub const CHUNK_BYTES: usize = 1 << 20;

/// The service over `store`; `None` answers every call `UNAVAILABLE`.
pub struct Releases {
    store: Option<Arc<ReleaseStore>>,
}

/// The server a listener mounts.
pub type ReleasesServer = ReleaseServiceServer<Releases>;

/// The service to mount on a listener.
#[must_use]
pub fn service(store: Option<Arc<ReleaseStore>>) -> ReleasesServer {
    ReleaseServiceServer::new(Releases { store })
}

type Chunks = Pin<Box<dyn Stream<Item = Result<FetchChunk, Status>> + Send>>;

/// A file as chunks: each read fills a buffer of [`CHUNK_BYTES`] unless the file ends.
fn chunks(file: File) -> Chunks {
    Box::pin(futures::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut buffer = vec![0_u8; CHUNK_BYTES];
        let mut filled = 0;
        while filled < CHUNK_BYTES {
            match file.read(&mut buffer[filled..]).await {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => {
                    tracing::error!("release store: read: {e}");
                    return Some((Err(Status::internal("the release store failed")), None));
                }
            }
        }
        buffer.truncate(filled);
        // A short chunk is the last: the file ended inside it.
        let next = (filled == CHUNK_BYTES).then_some(file);
        (filled > 0).then_some((Ok(FetchChunk { data: buffer }), next))
    }))
}

/// The refusal of a server without a store.
fn off() -> Status {
    Status::unavailable("the release store is off: the server was started without --releases-dir")
}

#[tonic::async_trait]
impl ReleaseService for Releases {
    type FetchStream = Chunks;

    async fn fetch(&self, request: Request<FetchRequest>) -> Result<Response<Chunks>, Status> {
        let Some(store) = &self.store else {
            return Err(off());
        };
        let sha256 = Sha256Hex::parse(&request.get_ref().sha256)
            .ok_or_else(|| Status::invalid_argument("sha256 is 64 lowercase hex digits"))?;
        let opened = store.open_tarball(&sha256).await.map_err(|e| {
            tracing::error!("release store: open: {e}");
            Status::internal("the release store failed")
        })?;
        let file = opened.ok_or_else(|| Status::not_found("no stored file has that digest"))?;
        Ok(Response::new(chunks(file)))
    }

    async fn describe(
        &self,
        request: Request<DescribeRequest>,
    ) -> Result<Response<ReleaseDescription>, Status> {
        let Some(store) = &self.store else {
            return Err(off());
        };
        let asked = request.get_ref();
        let commit = Commit::parse(&asked.commit).ok_or_else(|| {
            Status::invalid_argument(KeyError::Commit(asked.commit.clone()).to_string())
        })?;
        let platform = match asked.platform.as_str() {
            "" => None,
            name => Some(ReleasePlatform::parse(name).ok_or_else(|| {
                Status::invalid_argument(KeyError::Platform(name.to_owned()).to_string())
            })?),
        };
        match describe(store, &commit, platform).await {
            Ok(description) => Ok(Response::new(description)),
            Err(e @ DescribeError::Unknown(_)) => Err(Status::not_found(e.to_string())),
            Err(e @ DescribeError::Incomplete { .. }) => {
                Err(Status::failed_precondition(e.to_string()))
            }
            Err(DescribeError::Store(e)) => {
                tracing::error!("release store: describe: {e}");
                Err(Status::internal("the release store failed"))
            }
        }
    }
}
