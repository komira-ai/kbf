//! The REAPI calls of `mac-farm`, over one channel to the farm: which blobs the CAS lacks, their
//! upload (small ones in batches, large ones streamed), the Execute of the action until it is
//! done, and the read of a blob (the action's stdout and stderr, and its output files).

use std::collections::BTreeMap;

use futures::{StreamExt as _, stream};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::reapi::batch_update_blobs_request::Request as UploadRequest;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::execution_stage::Value as Stage;
use kbf_proto::reapi::{
    BatchUpdateBlobsRequest, Digest, ExecuteOperationMetadata, ExecuteRequest, ExecuteResponse,
    FindMissingBlobsRequest, WaitExecutionRequest,
};
use prost::Message as _;
use sha2::{Digest as _, Sha256};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Status};

/// The largest blob sent in a batch; a larger one is streamed.
pub const BATCH_BLOB_BYTES: usize = 1 << 20;

/// The most bytes of blobs in one batch: under the 4 MiB message size REAPI servers take by
/// default.
pub const BATCH_BYTES: usize = 3 << 20;

/// The size of one message of a streamed upload.
pub const CHUNK_BYTES: usize = 1 << 20;

/// The most digests asked about in one FindMissingBlobs.
pub const FIND_DIGESTS: usize = 4096;

/// The uploads in flight at once.
pub const UPLOADS: usize = 8;

/// How many cuts of the Execute stream end the run: each cut before the last is followed again
/// (WaitExecution, or a new Execute when the operation is gone).
pub const RECONNECTS: usize = 3;

/// How long the connect to the farm may take. Nothing else has a deadline of the client's own:
/// the action's timeout and the job's `timeout-minutes` bound a run.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The largest message the client takes: a ByteStream read message, or a batch answer.
const MAX_MESSAGE_BYTES: usize = 32 << 20;

/// Why a call to the farm failed.
#[derive(Debug, thiserror::Error)]
pub enum FarmError {
    #[error("the farm address {address:?} does not connect: {why}")]
    Connect { address: String, why: String },
    #[error("{call}: {code:?}: {message}")]
    Call {
        call: &'static str,
        code: Code,
        message: String,
    },
    #[error("the upload of {hash} was refused: {code}: {message}")]
    Refused {
        hash: String,
        code: i32,
        message: String,
    },
    #[error("the upload of {hash} wrote {wrote} of {size} bytes")]
    Short { hash: String, wrote: i64, size: i64 },
    #[error("{hash} is not among the blobs of the action")]
    Unknown { hash: String },
    #[error("the Execute stream was cut {RECONNECTS} times; last: {0}")]
    Cut(String),
    #[error("the operation ended with no response")]
    NoResponse,
    #[error("the operation's response does not decode: {0}")]
    Garbage(String),
    #[error("write the blob {hash}: {why}")]
    Write { hash: String, why: String },
}

fn call(call: &'static str) -> impl FnOnce(Status) -> FarmError {
    move |s| FarmError::Call {
        call,
        code: s.code(),
        message: s.message().to_owned(),
    }
}

/// What the farm says while the action waits and runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Waiting for a free Mac.
    Queued,
    /// Running on a Mac.
    Executing,
}

/// The farm: a channel and the instance name every request carries.
#[derive(Clone)]
pub struct Farm {
    channel: Channel,
    instance: String,
}

impl Farm {
    /// Connects to `endpoint` (`http://` or, with TLS and the system's roots, `https://`).
    ///
    /// # Errors
    /// [`FarmError::Connect`].
    pub async fn connect(endpoint: &str, tls: bool, instance: &str) -> Result<Farm, FarmError> {
        let fail = |why: String| FarmError::Connect {
            address: endpoint.to_owned(),
            why,
        };
        let mut ep = Endpoint::from_shared(endpoint.to_owned()).map_err(|e| fail(e.to_string()))?;
        if tls {
            ep = ep
                .tls_config(ClientTlsConfig::new().with_native_roots())
                .map_err(|e| fail(e.to_string()))?;
        }
        let channel = ep
            .connect_timeout(CONNECT_TIMEOUT)
            .connect()
            .await
            .map_err(|e| fail(e.to_string()))?;
        Ok(Farm {
            channel,
            instance: instance.to_owned(),
        })
    }

    fn cas(&self) -> ContentAddressableStorageClient<Channel> {
        ContentAddressableStorageClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
    }

    /// A resource name: `<instance>/<rest>`, or `<rest>` for the empty instance.
    fn resource(&self, rest: &str) -> String {
        if self.instance.is_empty() {
            rest.to_owned()
        } else {
            format!("{}/{rest}", self.instance)
        }
    }

    /// The digests of `digests` the CAS does not have.
    ///
    /// # Errors
    /// [`FarmError::Call`].
    pub async fn missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, FarmError> {
        let mut out = Vec::new();
        for chunk in digests.chunks(FIND_DIGESTS) {
            let answer = self
                .cas()
                .find_missing_blobs(FindMissingBlobsRequest {
                    instance_name: self.instance.clone(),
                    blob_digests: chunk.to_vec(),
                    ..FindMissingBlobsRequest::default()
                })
                .await
                .map_err(call("FindMissingBlobs"))?;
            out.extend(answer.into_inner().missing_blob_digests);
        }
        Ok(out)
    }

    /// Uploads the blobs `missing` names from `blobs`, [`UPLOADS`] calls at a time.
    ///
    /// # Errors
    /// [`FarmError`] for the first upload that fails.
    pub async fn upload(
        &self,
        blobs: &BTreeMap<String, Vec<u8>>,
        missing: &[Digest],
    ) -> Result<(), FarmError> {
        // Each job borrows its blobs from `blobs`: nothing is copied before it is encoded.
        let mut jobs: Vec<Vec<(&Digest, &[u8])>> = Vec::new();
        let mut batch: Vec<(&Digest, &[u8])> = Vec::new();
        let mut batch_bytes = 0;
        for digest in missing {
            let bytes = blobs.get(&digest.hash).ok_or_else(|| FarmError::Unknown {
                hash: digest.hash.clone(),
            })?;
            if bytes.len() > BATCH_BLOB_BYTES {
                jobs.push(vec![(digest, bytes.as_slice())]);
                continue;
            }
            if batch_bytes + bytes.len() > BATCH_BYTES {
                jobs.push(std::mem::take(&mut batch));
                batch_bytes = 0;
            }
            batch_bytes += bytes.len();
            batch.push((digest, bytes.as_slice()));
        }
        if !batch.is_empty() {
            jobs.push(batch);
        }
        let results: Vec<Result<(), FarmError>> = stream::iter(jobs)
            .map(|job| async move {
                match job.as_slice() {
                    [(digest, bytes)] if bytes.len() > BATCH_BLOB_BYTES => {
                        self.write(digest, bytes).await
                    }
                    _ => self.batch(&job).await,
                }
            })
            .buffer_unordered(UPLOADS)
            .collect()
            .await;
        results.into_iter().collect()
    }

    async fn batch(&self, blobs: &[(&Digest, &[u8])]) -> Result<(), FarmError> {
        let requests = blobs
            .iter()
            .map(|(digest, data)| UploadRequest {
                digest: Some((*digest).clone()),
                data: data.to_vec(),
                ..UploadRequest::default()
            })
            .collect();
        let answer = self
            .cas()
            .batch_update_blobs(BatchUpdateBlobsRequest {
                instance_name: self.instance.clone(),
                requests,
                ..BatchUpdateBlobsRequest::default()
            })
            .await
            .map_err(call("BatchUpdateBlobs"))?;
        for response in answer.into_inner().responses {
            let status = response.status.unwrap_or_default();
            if status.code != 0 {
                return Err(FarmError::Refused {
                    hash: response.digest.unwrap_or_default().hash,
                    code: status.code,
                    message: status.message,
                });
            }
        }
        Ok(())
    }

    /// Streams one blob with ByteStream Write, [`CHUNK_BYTES`] a message.
    async fn write(&self, digest: &Digest, bytes: &[u8]) -> Result<(), FarmError> {
        // A name no other write of this run has: the blob's own hash is unique among them.
        let name = self.resource(&format!(
            "uploads/{}-{}-4{}-8{}-{}/blobs/{}/{}",
            &digest.hash[..8],
            &digest.hash[8..12],
            &digest.hash[13..16],
            &digest.hash[17..20],
            &digest.hash[20..32],
            digest.hash,
            digest.size_bytes
        ));
        let mut messages = Vec::new();
        for (at, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
            let offset = at * CHUNK_BYTES;
            messages.push(WriteRequest {
                resource_name: if at == 0 { name.clone() } else { String::new() },
                write_offset: i64::try_from(offset).unwrap_or(i64::MAX),
                finish_write: offset + chunk.len() == bytes.len(),
                data: chunk.to_vec(),
            });
        }
        let answer = ByteStreamClient::new(self.channel.clone())
            .write(stream::iter(messages))
            .await
            .map_err(call("ByteStream.Write"))?;
        let wrote = answer.into_inner().committed_size;
        if wrote != digest.size_bytes {
            return Err(FarmError::Short {
                hash: digest.hash.clone(),
                wrote,
                size: digest.size_bytes,
            });
        }
        Ok(())
    }

    /// The bytes of the blob `digest` (none for the empty blob).
    ///
    /// # Errors
    /// [`FarmError::Call`].
    pub async fn read(&self, digest: &Digest) -> Result<Vec<u8>, FarmError> {
        let mut out = Vec::new();
        self.read_into(digest, &mut out).await?;
        Ok(out)
    }

    /// Writes the blob `digest` into `to` as its messages arrive (none for the empty blob),
    /// hashing them on the way; its SHA-256 and size.
    ///
    /// # Errors
    /// [`FarmError::Call`] for the read, [`FarmError::Write`] for `to`.
    pub async fn read_into(
        &self,
        digest: &Digest,
        to: &mut (dyn std::io::Write + Send),
    ) -> Result<Digest, FarmError> {
        let mut hash = Sha256::new();
        let mut size = 0_usize;
        if digest.size_bytes != 0 {
            let name = self.resource(&format!("blobs/{}/{}", digest.hash, digest.size_bytes));
            let mut answer = ByteStreamClient::new(self.channel.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .read(ReadRequest {
                    resource_name: name,
                    read_offset: 0,
                    read_limit: 0,
                })
                .await
                .map_err(call("ByteStream.Read"))?
                .into_inner();
            while let Some(message) = answer.message().await.map_err(call("ByteStream.Read"))? {
                hash.update(&message.data);
                size += message.data.len();
                to.write_all(&message.data).map_err(|e| FarmError::Write {
                    hash: digest.hash.clone(),
                    why: e.to_string(),
                })?;
            }
        }
        Ok(Digest {
            hash: hex::encode(hash.finalize()),
            size_bytes: i64::try_from(size).unwrap_or(i64::MAX),
        })
    }

    /// Executes `action` and follows its operation until it is done; `progress` is told each
    /// change of stage. A cut stream is followed again with WaitExecution, or with a new
    /// Execute when the farm no longer knows the operation, until the stream has been cut
    /// [`RECONNECTS`] times.
    ///
    /// # Errors
    /// [`FarmError`]: the call refused, the stream cut too often, or a done operation with no
    /// response.
    pub async fn execute(
        &self,
        action: &Digest,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<ExecuteResponse, FarmError> {
        let mut client = ExecutionClient::new(self.channel.clone());
        let mut name = String::new();
        let mut last = None;
        let mut cuts = 0;
        loop {
            let opened = if name.is_empty() {
                client
                    .execute(ExecuteRequest {
                        instance_name: self.instance.clone(),
                        action_digest: Some(action.clone()),
                        ..ExecuteRequest::default()
                    })
                    .await
                    .map_err(call("Execute"))
            } else {
                client
                    .wait_execution(WaitExecutionRequest { name: name.clone() })
                    .await
                    .map_err(call("WaitExecution"))
            };
            let mut updates = match opened {
                Ok(updates) => updates.into_inner(),
                Err(FarmError::Call {
                    call: "WaitExecution",
                    code: Code::NotFound,
                    ..
                }) => {
                    name.clear();
                    continue;
                }
                Err(e) => return Err(e),
            };
            let why = loop {
                match updates.message().await {
                    Ok(Some(op)) => {
                        name.clone_from(&op.name);
                        if let Some(stage) = stage(&op).filter(|s| last != Some(*s)) {
                            last = Some(stage);
                            progress(stage);
                        }
                        if op.done {
                            return response(op);
                        }
                    }
                    Ok(None) => break "the stream ended before the operation was done".to_owned(),
                    Err(status) => break format!("{:?}: {}", status.code(), status.message()),
                }
            };
            cuts += 1;
            if cuts >= RECONNECTS {
                return Err(FarmError::Cut(why));
            }
        }
    }
}

/// The stage an operation's metadata says, when it is one [`Progress`] names.
fn stage(op: &Operation) -> Option<Progress> {
    let meta = ExecuteOperationMetadata::decode(op.metadata.as_ref()?.value.as_slice()).ok()?;
    match Stage::try_from(meta.stage).ok()? {
        Stage::Queued => Some(Progress::Queued),
        Stage::Executing => Some(Progress::Executing),
        _ => None,
    }
}

/// The response of a done operation. An operation that ended with an error is answered as a
/// response with that status and no result, as REAPI allows either.
fn response(op: Operation) -> Result<ExecuteResponse, FarmError> {
    match op.result {
        Some(operation::Result::Response(any)) => ExecuteResponse::decode(any.value.as_slice())
            .map_err(|e| FarmError::Garbage(e.to_string())),
        Some(operation::Result::Error(status)) => Ok(ExecuteResponse {
            status: Some(status),
            ..ExecuteResponse::default()
        }),
        None => Err(FarmError::NoResponse),
    }
}
