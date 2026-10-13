//! A run: the requests that send (or read back) a tree's blobs, made `in_flight` at a
//! time over one channel, each one timed.
//!
//! `bytestream` moves each blob through the daemon's transfer engine
//! ([`kbf_transfer::Transfers`], over the run's channel, its in-flight limit
//! `in_flight`, the daemon's `--transfer-in-flight`; every other limit the engine's
//! default): an upload is the engine's `put_chunks`, as the container driver stores an
//! output file, so the engine first asks FindMissing about the blob (its calls
//! coalesced, one at a time) and sends a ByteStream Write only when the server lacks
//! it; a read is the engine's ByteStream Read through memory, checked against its
//! digest ([`kbf_transfer::fetch`]). `batch` packs blobs into BatchUpdateBlobs and
//! BatchReadBlobs calls of at most `batch_bytes` of blob data, made directly (the
//! engine does not batch yet); a larger blob goes through the engine. An upload with
//! `find_missing` first asks FindMissingBlobs about every digest,
//! [`FIND_MISSING_DIGESTS`] per call, and sends only the missing ones.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures::stream::{self, BoxStream};
use futures::{FutureExt as _, StreamExt as _};
use kbf_proto::google::rpc;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{
    BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest, FindMissingBlobsRequest,
    batch_update_blobs_request,
};
use kbf_transfer::{Cas as _, CasError, Chunks, TransferConfig, Transfers, fetch, label};
use tonic::transport::Channel;

use crate::report::{Kind, Report, Tally};
use crate::tree::{Tree, digest_of};

/// How many digests one FindMissingBlobs call asks about.
pub const FIND_MISSING_DIGESTS: usize = 1000;

/// How the blobs travel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// One ByteStream Write (or Read) per blob, as the daemon does.
    Bytestream,
    /// BatchUpdateBlobs (or BatchReadBlobs) calls of up to `--batch-bytes` of data.
    Batch,
}

impl Method {
    /// The flag value.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bytestream => "bytestream",
            Self::Batch => "batch",
        }
    }
}

/// How a run makes its requests.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub method: Method,
    /// The most requests in flight at once; at least 1. It bounds the requests the run
    /// makes and is the engine's limit for the whole run (as `--transfer-in-flight` is
    /// for a daemon), so a ByteStream write and the FindMissing call before it share it.
    pub in_flight: usize,
    /// Upload only: ask FindMissingBlobs first and send only what is missing.
    pub find_missing: bool,
    /// The most blob data one batch call carries.
    pub batch_bytes: u64,
    /// Stop when this much time has passed, and report what finished.
    pub timeout: Option<Duration>,
    /// Print a progress line to stderr this often (and once at the start).
    pub progress: Option<Duration>,
}

/// One request a run makes: indices into the tree's blobs.
enum Request {
    Find(Vec<usize>),
    Write(usize),
    Batch(Vec<usize>),
    Read(usize),
    BatchRead(Vec<usize>),
}

/// What one finished request did.
pub(crate) struct Outcome {
    pub(crate) kind: Kind,
    pub(crate) elapsed: Duration,
    /// Blobs it sent or read whole.
    pub(crate) blobs: u64,
    pub(crate) bytes: u64,
    pub(crate) errors: Vec<String>,
    /// FindMissingBlobs only: the blobs reported missing.
    pub(crate) missing: Vec<usize>,
}

/// The connection's engine and batch client, and the tree, shared by every request.
struct Run<'a> {
    cas: Transfers,
    batch: ContentAddressableStorageClient<Channel>,
    tree: &'a Tree,
    options: Options,
    start: Instant,
}

/// Sends `tree`'s blobs over `channel`.
pub async fn upload(channel: Channel, tree: &Tree, options: Options) -> Report {
    let run = Run::new(channel, tree, options);
    let mut tally = Tally::default();
    let all: Vec<usize> = (0..tree.blobs.len()).collect();
    let mut send = all.clone();
    if options.find_missing {
        let finds = all
            .chunks(FIND_MISSING_DIGESTS)
            .map(|c| Request::Find(c.to_vec()));
        run.requests(finds.collect(), &mut tally).await;
        send = std::mem::take(&mut tally.missing);
        send.sort_unstable();
    }
    let requests = run.plan(&send, Request::Write, Request::Batch);
    tally.blobs_planned = send.len() as u64;
    if !tally.timed_out {
        run.requests(requests, &mut tally).await;
    }
    Report::new("upload", tree, options, run.start.elapsed(), tally)
}

/// Reads `tree`'s blobs back over `channel`, each checked against its digest.
pub async fn read(channel: Channel, tree: &Tree, options: Options) -> Report {
    let run = Run::new(channel, tree, options);
    let mut tally = Tally::default();
    let all: Vec<usize> = (0..tree.blobs.len()).collect();
    tally.blobs_planned = all.len() as u64;
    let requests = run.plan(&all, Request::Read, Request::BatchRead);
    run.requests(requests, &mut tally).await;
    Report::new("read", tree, options, run.start.elapsed(), tally)
}

/// What [`Run::requests`] is woken by.
enum Event {
    Done(Outcome),
    Finished,
    Tick,
    Deadline,
}

impl<'a> Run<'a> {
    fn new(channel: Channel, tree: &'a Tree, options: Options) -> Self {
        // A full batch and the digests and statuses around it.
        let message = options.batch_bytes as usize + (1 << 20);
        Self {
            cas: Transfers::over(
                channel.clone(),
                TransferConfig {
                    in_flight: options.in_flight,
                    ..TransferConfig::default()
                },
            ),
            batch: ContentAddressableStorageClient::new(channel)
                .max_decoding_message_size(message)
                .max_encoding_message_size(message),
            tree,
            options,
            start: Instant::now(),
        }
    }

    /// The requests that move `blobs`: one each, or packed into batches.
    fn plan(
        &self,
        blobs: &[usize],
        one: fn(usize) -> Request,
        many: fn(Vec<usize>) -> Request,
    ) -> Vec<Request> {
        if self.options.method == Method::Bytestream {
            return blobs.iter().copied().map(one).collect();
        }
        let mut requests = Vec::new();
        let mut batch = Vec::new();
        let mut bytes = 0;
        for &i in blobs {
            let size = self.tree.blobs[i].bytes.len() as u64;
            if size > self.options.batch_bytes {
                requests.push(one(i));
                continue;
            }
            if bytes + size > self.options.batch_bytes {
                requests.push(many(std::mem::take(&mut batch)));
                bytes = 0;
            }
            batch.push(i);
            bytes += size;
        }
        if !batch.is_empty() {
            requests.push(many(batch));
        }
        requests
    }

    /// Makes `requests`, `in_flight` at a time, into `tally`, until all finish or the
    /// deadline passes.
    async fn requests(&self, requests: Vec<Request>, tally: &mut Tally) {
        tally.requests_planned += requests.len() as u64;
        let outcomes = stream::iter(requests)
            .map(|r| self.make(r))
            .buffer_unordered(self.options.in_flight)
            .map(Event::Done)
            .chain(stream::once(async { Event::Finished }));
        let ticks: BoxStream<'_, Event> = match self.options.progress {
            Some(every) => stream::unfold(tokio::time::interval(every), |mut ticks| async {
                ticks.tick().await;
                Some((Event::Tick, ticks))
            })
            .boxed(),
            None => stream::pending().boxed(),
        };
        let deadline: BoxStream<'_, Event> = match self.options.timeout {
            Some(after) => stream::once(
                tokio::time::sleep_until((self.start + after).into()).map(|()| Event::Deadline),
            )
            .boxed(),
            None => stream::pending().boxed(),
        };
        let mut events = stream::select_all([outcomes.boxed(), ticks, deadline]);
        loop {
            match events.next().await {
                Some(Event::Done(outcome)) => tally.add(outcome),
                Some(Event::Tick) => {
                    eprintln!("kbf-bench: {}", tally.progress(self.start.elapsed()))
                }
                // The streams end only after `Finished`.
                Some(Event::Finished) | None => return,
                Some(Event::Deadline) => {
                    tally.timed_out = true;
                    return;
                }
            }
        }
    }

    async fn make(&self, request: Request) -> Outcome {
        let started = Instant::now();
        let mut outcome = match request {
            Request::Find(blobs) => self.find(blobs).await,
            Request::Write(i) => self.write(i).await,
            Request::Batch(blobs) => self.batch_update(blobs).await,
            Request::Read(i) => self.read(i).await,
            Request::BatchRead(blobs) => self.batch_read(blobs).await,
        };
        outcome.elapsed = started.elapsed();
        outcome
    }

    async fn find(&self, blobs: Vec<usize>) -> Outcome {
        let by_hash: BTreeMap<&str, usize> = blobs
            .iter()
            .map(|&i| (self.tree.blobs[i].digest.hash.as_str(), i))
            .collect();
        let request = FindMissingBlobsRequest {
            blob_digests: blobs
                .iter()
                .map(|&i| self.tree.blobs[i].digest.clone())
                .collect(),
            ..FindMissingBlobsRequest::default()
        };
        let mut outcome = Outcome::new(Kind::FindMissing);
        match self.batch.clone().find_missing_blobs(request).await {
            Ok(response) => {
                let missing = response.into_inner().missing_blob_digests;
                outcome.missing = missing
                    .iter()
                    .filter_map(|d| by_hash.get(d.hash.as_str()).copied())
                    .collect();
            }
            Err(status) => outcome.errors.push(format!("FindMissingBlobs: {status}")),
        }
        outcome
    }

    async fn write(&self, i: usize) -> Outcome {
        let blob = &self.tree.blobs[i];
        let chunks: Chunks = Box::pin(stream::iter([Ok(blob.bytes.clone())]));
        let stored = self.cas.put_chunks(blob.digest.clone(), chunks).await;
        Outcome::of(Kind::Write, stored.map(|_| blob.bytes.len()))
    }

    async fn read(&self, i: usize) -> Outcome {
        let fetched = fetch(&self.cas, &self.tree.blobs[i].digest).await;
        Outcome::of(Kind::Read, fetched.map(|bytes| bytes.len()))
    }

    async fn batch_update(&self, blobs: Vec<usize>) -> Outcome {
        let request = BatchUpdateBlobsRequest {
            requests: blobs
                .iter()
                .map(|&i| batch_update_blobs_request::Request {
                    digest: Some(self.tree.blobs[i].digest.clone()),
                    data: self.tree.blobs[i].bytes.clone(),
                    ..batch_update_blobs_request::Request::default()
                })
                .collect(),
            ..BatchUpdateBlobsRequest::default()
        };
        let mut outcome = Outcome::new(Kind::BatchUpdate);
        match self.batch.clone().batch_update_blobs(request).await {
            Ok(response) => {
                for answer in response.into_inner().responses {
                    let digest = answer.digest.unwrap_or_default();
                    let status = answer.status.unwrap_or_default();
                    let bytes = digest.size_bytes as u64;
                    outcome.answer(
                        "BatchUpdateBlobs",
                        &digest,
                        &status,
                        status.code == 0,
                        bytes,
                    );
                }
            }
            Err(status) => outcome.errors.push(format!("BatchUpdateBlobs: {status}")),
        }
        outcome
    }

    async fn batch_read(&self, blobs: Vec<usize>) -> Outcome {
        let request = BatchReadBlobsRequest {
            digests: blobs
                .iter()
                .map(|&i| self.tree.blobs[i].digest.clone())
                .collect(),
            ..BatchReadBlobsRequest::default()
        };
        let mut outcome = Outcome::new(Kind::BatchRead);
        match self.batch.clone().batch_read_blobs(request).await {
            Ok(response) => {
                for answer in response.into_inner().responses {
                    let digest = answer.digest.unwrap_or_default();
                    let status = answer.status.unwrap_or_default();
                    // The bytes must be the ones the digest names; `&`, not `&&`: both
                    // are read either way.
                    let good = (status.code == 0) & (digest_of(&answer.data) == digest);
                    let bytes = answer.data.len() as u64;
                    outcome.answer("BatchReadBlobs", &digest, &status, good, bytes);
                }
            }
            Err(status) => outcome.errors.push(format!("BatchReadBlobs: {status}")),
        }
        outcome
    }
}

impl Outcome {
    const fn new(kind: Kind) -> Self {
        Self {
            kind,
            elapsed: Duration::ZERO,
            blobs: 0,
            bytes: 0,
            errors: Vec::new(),
            missing: Vec::new(),
        }
    }

    /// A batch call's answer for one blob: moved whole (`bytes` long) when `good`.
    fn answer(
        &mut self,
        call: &str,
        digest: &Digest,
        status: &rpc::Status,
        good: bool,
        bytes: u64,
    ) {
        if good {
            self.blobs += 1;
            self.bytes += bytes;
        } else {
            let (code, message) = (status.code, &status.message);
            self.errors
                .push(format!("{call} {}: code {code}: {message}", label(digest)));
        }
    }

    /// The outcome of a one-blob request that moved `bytes`, or failed.
    fn of(kind: Kind, moved: Result<usize, CasError>) -> Self {
        let mut outcome = Self::new(kind);
        match moved {
            Ok(bytes) => {
                outcome.blobs = 1;
                outcome.bytes = bytes as u64;
            }
            Err(e) => outcome.errors.push(e.to_string()),
        }
        outcome
    }
}
