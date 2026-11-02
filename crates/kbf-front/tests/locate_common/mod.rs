//! Helpers of the `GET /locate` tests (`job_read_locate.rs`, `job_read_locate_bounds.rs`): a
//! source with a pack index and large objects of its own, Actions to build, and a rig.
//!
//! Originally: `GET /locate` on the job read listener: the table of where the blobs of a grant's Action
//! are, over a source that has a pack index and large objects of its own to answer from.

#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_front::digest_to_proto;
use kbf_front::grants::{GrantKey, Grants, MIN_SECRET_LEN};
use kbf_front::job_read::{BlobSource, JobRead, LOCATE_PATH, Limits, Reply, Span, StoreGrants};
use kbf_proto::reapi::{Action, Directory, DirectoryNode, FileNode};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::Digest;
use prost::Message as _;
use serde_json::Value;
use tonic::codegen::http::{Method, StatusCode};

pub const BUILD: &str = "build-0014";
pub const NOT_AFTER: u64 = 1_000;

#[derive(Default)]
pub struct Source {
    pub blobs: Mutex<HashMap<Digest, Bytes>>,
    pub spans: Mutex<HashMap<Digest, Span>>,
    /// The large objects tierstore holds, with their lengths.
    pub large: Mutex<HashMap<Digest, u64>>,
    pub reads: Mutex<Vec<Digest>>,
    pub asked: Mutex<Vec<Vec<Digest>>>,
    pub heads: Mutex<Vec<Digest>>,
    pub index_down: AtomicBool,
    /// The index never answers.
    pub index_hang: AtomicBool,
    pub blobs_down: AtomicBool,
    /// Presence checks wait here when it is set, and are counted while they wait.
    pub head_gate: Option<tokio::sync::Semaphore>,
    pub head_open: Mutex<(usize, usize)>,
    /// Presence checks that are never answered.
    pub head_hang: AtomicBool,
    pub head_down: AtomicBool,
}

impl Source {
    pub fn reads_of(&self, digest: &Digest) -> usize {
        let reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.iter().filter(|read| *read == digest).count()
    }

    pub fn heads_of(&self, digest: &Digest) -> usize {
        let heads = self.heads.lock().unwrap_or_else(PoisonError::into_inner);
        heads.iter().filter(|head| *head == digest).count()
    }

    pub fn asked(&self) -> Vec<Vec<Digest>> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

pub fn down() -> StorageError {
    StorageError::Unavailable(Arc::new(tonic::Status::unavailable("down")))
}

impl BlobSource for Source {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(async move {
            self.reads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(*digest);
            tokio::task::yield_now().await;
            if self.blobs_down.load(Ordering::SeqCst) {
                return Err(down());
            }
            self.blobs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(digest)
                .cloned()
                .ok_or(StorageError::NotFound(*digest))
        })
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        Box::pin(async move { Err(StorageError::NotFound(*digest)) })
    }

    fn locate_packed<'a>(
        &'a self,
        digests: &'a [Digest],
    ) -> BoxFuture<'a, Result<Vec<Option<Span>>, StorageError>> {
        Box::pin(async move {
            self.asked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(digests.to_vec());
            if self.index_hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.index_down.load(Ordering::SeqCst) {
                return Err(down());
            }
            let spans = self.spans.lock().unwrap_or_else(PoisonError::into_inner);
            Ok(digests.iter().map(|d| spans.get(d).cloned()).collect())
        })
    }

    fn large_len<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<Option<u64>, StorageError>> {
        Box::pin(async move {
            self.heads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(*digest);
            if self.head_hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if let Some(gate) = &self.head_gate {
                {
                    let mut open = self
                        .head_open
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    open.0 += 1;
                    open.1 = open.1.max(open.0);
                }
                gate.acquire().await.expect("the gate is open").forget();
                self.head_open
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .0 -= 1;
            }
            if self.head_down.load(Ordering::SeqCst) {
                return Err(down());
            }
            let large = self.large.lock().unwrap_or_else(PoisonError::into_inner);
            Ok(large.get(digest).copied())
        })
    }
}

pub fn put(source: &Source, bytes: Vec<u8>) -> Digest {
    let digest = kbf_segments::sha256(&bytes);
    source
        .blobs
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(digest, Bytes::from(bytes));
    digest
}

/// An Action over a root with files of these contents and a subdirectory with one more file.
pub struct Built {
    pub action: Digest,
    pub command: Digest,
    pub root: Digest,
    pub sub: Digest,
    pub files: Vec<Digest>,
    pub deep: Digest,
}

impl Built {
    pub fn scope(&self) -> BTreeSet<Digest> {
        let mut all: BTreeSet<Digest> = self.files.iter().copied().collect();
        all.extend([self.action, self.command, self.root, self.sub, self.deep]);
        all
    }
}

pub fn build(source: &Source, tag: &str, contents: &[Vec<u8>]) -> Built {
    let files: Vec<Digest> = contents.iter().map(|c| put(source, c.clone())).collect();
    let deep = put(source, format!("deep file of {tag}").into_bytes());
    let sub = Directory {
        files: vec![FileNode {
            name: "deep".into(),
            digest: Some(digest_to_proto(&deep)),
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let sub = put(source, sub.encode_to_vec());
    let nodes = files
        .iter()
        .enumerate()
        .map(|(i, digest)| FileNode {
            name: format!("{tag}-{i}"),
            digest: Some(digest_to_proto(digest)),
            ..FileNode::default()
        })
        .collect();
    let root = Directory {
        files: nodes,
        directories: vec![DirectoryNode {
            name: "sub".into(),
            digest: Some(digest_to_proto(&sub)),
        }],
        ..Directory::default()
    };
    let root = put(source, root.encode_to_vec());
    let command = put(source, format!("command of {tag}").into_bytes());
    let action = Action {
        command_digest: Some(digest_to_proto(&command)),
        input_root_digest: Some(digest_to_proto(&root)),
        ..Action::default()
    };
    let action = put(source, action.encode_to_vec());
    Built {
        action,
        command,
        root,
        sub,
        files,
        deep,
    }
}

pub fn span(key: &str, offset: u64, length: u64) -> Span {
    Span {
        key: key.to_owned(),
        offset,
        length,
    }
}

pub fn pack(source: &Source, digest: &Digest, at: Span) {
    source
        .spans
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(*digest, at);
}

pub struct Rig {
    pub source: Arc<Source>,
    pub read: JobRead,
    pub grants: Arc<Grants>,
    /// The time the grants are checked at, in unix seconds.
    pub clock: Arc<AtomicU64>,
}

pub fn rig(source: Source, limits: Limits) -> Rig {
    rig_granting(source, limits, None)
}

/// A rig whose locate hands out store grants made by `store`, if there is one.
pub fn rig_granting(source: Source, limits: Limits, store: Option<StoreGrants>) -> Rig {
    let key = GrantKey::from_secret(&[1; MIN_SECRET_LEN]).expect("a long enough secret");
    let clock = Arc::new(AtomicU64::new(100));
    let ticks = Arc::clone(&clock);
    let grants = Arc::new(Grants::new(
        key,
        Arc::new(move || ticks.load(Ordering::SeqCst)),
    ));
    let source = Arc::new(source);
    let read = JobRead::new(source.clone(), Arc::clone(&grants), limits).with_store_grants(store);
    Rig {
        source,
        read,
        grants,
        clock,
    }
}

impl Rig {
    pub fn token(&self, action: Digest) -> String {
        self.token_until(action, NOT_AFTER)
    }

    /// The read grant of `action` that ends at the unix time `not_after`.
    pub fn token_until(&self, action: Digest, not_after: u64) -> String {
        self.grants
            .issue(BUILD, action, not_after)
            .expect("a valid build id")
            .expose()
            .to_owned()
    }

    pub async fn locate(&self, token: &str) -> Reply {
        let header = format!("Bearer {token}");
        self.read
            .respond(&Method::GET, LOCATE_PATH, Some(&header))
            .await
    }

    pub fn metrics(&self) -> String {
        let mut out = String::new();
        self.read.render_metrics(&mut out);
        out
    }
}

/// The lines of a 200 locate body, by digest text.
pub fn table(reply: &Reply) -> HashMap<String, Value> {
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let text = std::str::from_utf8(&reply.body).expect("utf-8");
    let mut lines = HashMap::new();
    for line in text.lines() {
        let value: Value = serde_json::from_str(line).expect("a JSON line");
        let digest = value["digest"].as_str().expect("a digest").to_owned();
        assert!(lines.insert(digest, value).is_none(), "a digest twice");
    }
    lines
}

/// Limits that stream over 1000 bytes and remember presence for an hour, so that no test is
/// timed by the clock.
pub fn small() -> Limits {
    Limits {
        stream_over: 1000,
        locate_present_for: Duration::from_secs(3600),
        locate_absent_for: Duration::from_secs(3600),
        ..Limits::default()
    }
}
