//! One walk of an action's input tree per Execute, through the frontend's real pieces: the
//! Execute's input check ([`kbf_front::ExecutionService`]) and this [`Front`] over the frontend's
//! own preparation ([`PutDirPrep`], with its cache of named Directories), both over the rig's
//! backend, whose storage node records every blob read and every digest FindMissing names. The
//! run's mint reads no Directory and asks about no blob the Execute already did; a mint with no
//! Execute before it reads and asks for itself. The run's facts and closure take the Action and
//! the Command the Execute decoded, which its submission carries, so each is read once per
//! Execute; a run with no decode (an operation taken up from another process) reads them.

use std::sync::{Mutex, OnceLock, PoisonError};

use kbf_auth::Authorizers;
use kbf_front::prepare_run::{ClosureLimits, DIRECTORY_MEMORY_BYTES, DirectoryCache};
use kbf_front::{Closer, Dispatch, ExecutionService, Stage, Submission, Ticket};
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::execution_server::Execution as _;
use kbf_proto::reapi::{ActionResult, DirectoryNode, ExecuteRequest};
use kbf_proto::worker::segments_server::Segments;
use kbf_storage::{Answer, PutStream, ReadStream};
use tokio::sync::watch;
use tonic::Status;

use super::put_dir::{WALL, base};
use super::*;

/// The slack the preparation gives a put directory's keys past the run's wall.
const SLACK: Duration = Duration::from_secs(600);

/// What the storage node was asked: the blobs read, and the digests FindMissing named.
#[derive(Default)]
struct Log {
    read: Mutex<Vec<kbf_types::Digest>>,
    asked: Mutex<Vec<kbf_types::Digest>>,
}

impl Log {
    /// How many times `digest` was read.
    fn reads_of(&self, digest: &kbf_types::Digest) -> usize {
        let read = self.read.lock().unwrap_or_else(PoisonError::into_inner);
        read.iter().filter(|read| *read == digest).count()
    }

    /// Every digest FindMissing named, sorted, once per time it was named.
    fn asked(&self) -> Vec<kbf_types::Digest> {
        let mut asked = self
            .asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        asked.sort();
        asked
    }
}

/// A storage that records in its [`Log`] what it is asked, over another.
struct Recording {
    inner: Arc<dyn Storage>,
    log: Arc<Log>,
}

impl Storage for Recording {
    fn find_missing<'a>(
        &'a self,
        digests: &'a [kbf_types::Digest],
    ) -> Answer<'a, Vec<kbf_types::Digest>> {
        let mut asked = self
            .log
            .asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        asked.extend_from_slice(digests);
        drop(asked);
        self.inner.find_missing(digests)
    }

    fn find_durable<'a>(
        &'a self,
        digests: &'a [kbf_types::Digest],
    ) -> Answer<'a, Vec<kbf_types::Digest>> {
        self.inner.find_durable(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a kbf_types::Digest) -> Answer<'a, Bytes> {
        let mut read = self.log.read.lock().unwrap_or_else(PoisonError::into_inner);
        read.push(*digest);
        drop(read);
        self.inner.read_blob(digest)
    }

    fn read_range<'a>(
        &'a self,
        d: &'a kbf_types::Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        self.inner.read_range(d, offset, limit)
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, kbf_types::Digest> {
        self.inner.put(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.inner.stream(size)
    }

    fn put_streamed(
        &self,
        size: u64,
        bytes: futures::stream::BoxStream<'static, std::io::Result<Bytes>>,
        claim: futures::future::BoxFuture<'static, Option<kbf_types::Digest>>,
    ) -> Answer<'_, Option<kbf_types::Digest>> {
        self.inner.put_streamed(size, bytes, claim)
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }

    fn action_result<'a>(
        &'a self,
        a: &'a kbf_types::Digest,
    ) -> Answer<'a, Option<(kbf_types::Digest, ActionResult)>> {
        self.inner.action_result(a)
    }

    fn prepare_action_result<'a>(&'a self, r: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.inner.prepare_action_result(r)
    }

    fn commit_action_record(
        &self,
        role: Role,
        a: kbf_types::Digest,
        r: ActionRecord,
    ) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, a, r)
    }
}

/// The frontend's own preparation, made by the rig once it has its backend.
#[derive(Default)]
struct Real(OnceLock<PutDirPrep>);

impl Real {
    fn prep(&self) -> &PutDirPrep {
        self.0.get().expect("the rig made it")
    }
}

#[tonic::async_trait]
impl RunPrepare for Real {
    async fn prepare_put_dir(
        &self,
        run: &str,
        action: &kbf_types::Digest,
        decoded: Option<&Action>,
        now: SystemTime,
        wall: Duration,
    ) -> Result<PreparedPutDir, PrepareError> {
        RunPrepare::prepare_put_dir(self.prep(), run, action, decoded, now, wall).await
    }

    async fn mint_put_dir(
        &self,
        run: &str,
        action: &kbf_types::Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<MintedDir, PrepareError> {
        RunPrepare::mint_put_dir(self.prep(), run, action, now, wall).await
    }
}

/// A dispatch that takes every submission, keeps it, and runs nothing: the executor's part here
/// is the facts and the mint the test asks for itself.
#[derive(Default)]
struct Taken(Mutex<Vec<Submission>>);

impl Taken {
    /// The decode the one submission taken carries.
    fn decoded(&self) -> Option<Arc<kbf_front::DecodedAction>> {
        let taken = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let [submission] = taken.as_slice() else {
            panic!("one submission, not {}", taken.len());
        };
        submission.decoded.clone()
    }
}

impl Dispatch for Taken {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let mut taken = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        taken.push(submission.clone());
        let key = submission.key;
        Ok(Ticket {
            name: "operations/1".to_owned(),
            instance: key.instance,
            action: key.action,
            stage: watch::channel(Stage::Queued).1,
        })
    }

    fn wait(&self, _name: &str) -> Option<Ticket> {
        None
    }

    fn cancel(&self, _name: &str) -> bool {
        false
    }
}

/// The rig over the recording node, its Execution service over the same backend, and the log.
struct Walking {
    rig: Rig,
    service: ExecutionService<Taken>,
    taken: Arc<Taken>,
    log: Arc<Log>,
    _closer: Closer,
}

fn walking() -> Walking {
    let real = Arc::new(Real::default());
    let log = Arc::new(Log::default());
    let stash: Arc<OnceLock<Backend>> = Arc::new(OnceLock::new());
    let (made, stashed, recording) = (Arc::clone(&real), Arc::clone(&stash), Arc::clone(&log));
    let rig = Rig::reading_through(
        real,
        move |packs, backend| {
            let prep = PutDirPrep::new(
                backend.clone(),
                PackKeyMint::new(SECRET).expect("a mint"),
                SLACK,
                ClosureLimits::DEFAULT,
            )
            .with_directories(Arc::new(DirectoryCache::in_memory(DIRECTORY_MEMORY_BYTES)));
            made.0.set(prep).expect("set once");
            stashed.set(backend.clone()).expect("set once");
            let slot = Arc::new(OnceLock::new());
            slot.set(base()).expect("set once");
            PutDirParts::new(packs, backend, slot)
        },
        move |inner| {
            Arc::new(Recording {
                inner,
                log: recording,
            })
        },
    );
    let backend = stash.get().expect("the rig set it").clone();
    let (closer, closing) = kbf_front::closing();
    let taken = Arc::new(Taken::default());
    let service = ExecutionService::with_authorizers(
        backend,
        Arc::clone(&taken),
        closing,
        Arc::new(Authorizers::allow_all()),
    );
    Walking {
        rig,
        service,
        taken,
        log,
        _closer: closer,
    }
}

/// An Action whose input root names the file `one` and the Directory `sub`, which names the
/// file `two`: the Action, its Command, the two Directories and the two files, each stored on
/// the node.
struct InputSet {
    action: Digest,
    command: kbf_types::Digest,
    root: kbf_types::Digest,
    sub: kbf_types::Digest,
    files: [kbf_types::Digest; 2],
}

async fn input_set(rig: &Rig) -> InputSet {
    let file = |text: &str| reapi::Command {
        arguments: vec![text.to_owned()],
        ..reapi::Command::default()
    };
    let one = rig.store(&file("file one")).await;
    let two = rig.store(&file("file two")).await;
    let node = |name: &str, digest: &kbf_types::Digest| FileNode {
        name: name.to_owned(),
        digest: Some(wire(digest)),
        ..FileNode::default()
    };
    let sub = rig
        .store(&Directory {
            files: vec![node("two", &two)],
            ..Directory::default()
        })
        .await;
    let root = rig
        .store(&Directory {
            files: vec![node("one", &one)],
            directories: vec![DirectoryNode {
                name: "sub".to_owned(),
                digest: Some(wire(&sub)),
            }],
            ..Directory::default()
        })
        .await;
    let command = rig
        .store(&reapi::Command {
            output_paths: vec!["out".to_owned()],
            ..reapi::Command::default()
        })
        .await;
    let action = rig
        .store(&Action {
            command_digest: Some(wire(&command)),
            input_root_digest: Some(wire(&root)),
            ..Action::default()
        })
        .await;
    InputSet {
        action: wire(&action),
        command,
        root,
        sub,
        files: [one, two],
    }
}

/// Asserts that an Execute and the mint of its run read each Directory from the node once and
/// name each file to FindMissing once, and that the mint still names and stores the run's tree:
/// the Execute's check reads the two Directories and asks about the two files; the mint takes
/// the Directories from the cache the check filled, and takes the check's finding instead of
/// asking about the files and the Command again. Catches a second walk of the store (each
/// Directory read twice) and a second presence check (each file named twice, and the Command).
#[tokio::test]
async fn an_execute_and_the_mint_of_its_run_walk_the_inputs_once() {
    let Walking {
        rig, service, log, ..
    } = walking();
    let InputSet {
        action,
        root,
        sub,
        files,
        ..
    } = input_set(&rig).await;
    let request = ExecuteRequest {
        action_digest: Some(action.clone()),
        skip_cache_lookup: true,
        ..ExecuteRequest::default()
    };
    let submitted = service.execute(tonic::Request::new(request)).await;
    submitted.map(drop).expect("submitted");
    rig.front
        .mint(lease(), &action, WALL, InputTree::Manifests, None)
        .await
        .expect("a dir");

    assert_eq!((log.reads_of(&root), log.reads_of(&sub)), (1, 1));
    let mut once = files.to_vec();
    once.sort();
    assert_eq!(log.asked(), once);
    let tree = rig.front.tree(lease()).await.expect("a tree");
    assert_eq!(tree.len(), 64, "{tree}");
    rig.front.stored(lease()).await.expect("stored");
}

/// Asserts that a mint with no Execute before it on this backend (an operation another
/// frontend began, or a later attempt) reads its Directories and asks about the Command and
/// every file itself. Catches a preparation that skips the check with no finding to rest on.
#[tokio::test]
async fn a_mint_with_no_execute_before_it_walks_and_asks_for_itself() {
    let Walking { rig, log, .. } = walking();
    let InputSet {
        action,
        command,
        root,
        sub,
        files,
    } = input_set(&rig).await;
    rig.front
        .mint(lease(), &action, WALL, InputTree::Manifests, None)
        .await
        .expect("a dir");

    assert_eq!((log.reads_of(&root), log.reads_of(&sub)), (1, 1));
    let mut asked = files.to_vec();
    asked.push(command);
    asked.sort();
    assert_eq!(log.asked(), asked);
}

/// The Execute of `action`, its cache lookup skipped, submitted.
async fn execute(service: &ExecutionService<Taken>, action: &Digest) {
    let request = ExecuteRequest {
        action_digest: Some(action.clone()),
        skip_cache_lookup: true,
        ..ExecuteRequest::default()
    };
    let submitted = service.execute(tonic::Request::new(request)).await;
    submitted.map(drop).expect("submitted");
}

/// Asserts that an Execute and its run (the run's facts, then its mint, which
/// collects the closure) read the Action and the Command from the node once each, the
/// Execute's reads, and that the run still names and stores its tree and has the facts of the
/// Action: the run takes both from the decode its submission carries. Catches the facts reading
/// the Action and the Command again, and the closure reading the Action again.
#[tokio::test]
async fn an_execute_and_its_run_read_the_action_and_the_command_once() {
    let Walking {
        rig,
        service,
        taken,
        log,
        ..
    } = walking();
    let InputSet {
        action, command, ..
    } = input_set(&rig).await;
    let cas_action = cas_digest(&action).expect("a digest");
    execute(&service, &action).await;
    let decoded = taken.decoded().expect("the submission carries the decode");

    let facts = rig.front.action(&action, Some(&decoded)).await;
    let minted = (rig.front)
        .mint(lease(), &action, WALL, InputTree::Manifests, Some(&decoded))
        .await;

    assert_eq!(facts.expect("facts").timeout, None);
    minted.expect("a dir");
    rig.front.stored(lease()).await.expect("stored");
    assert!(rig.front.tree(lease()).await.is_ok(), "a tree is named");
    assert_eq!(
        (log.reads_of(&cas_action), log.reads_of(&command)),
        (1, 1),
        "the Action and the Command, each read once"
    );
}

/// Asserts that a run with no decode, as an operation taken up from another process has, reads
/// the Action and the Command itself, with no Execute before it: its facts read both, and its
/// closure the Action. Catches a run that has no facts or no closure when nothing was carried.
#[tokio::test]
async fn a_run_with_no_decode_reads_the_action_and_the_command_itself() {
    let Walking { rig, log, .. } = walking();
    let InputSet {
        action, command, ..
    } = input_set(&rig).await;
    let cas_action = cas_digest(&action).expect("a digest");

    let facts = rig.front.action(&action, None).await;
    let minted = (rig.front)
        .mint(lease(), &action, WALL, InputTree::Manifests, None)
        .await;

    assert_eq!(facts.expect("facts").timeout, None);
    minted.expect("a dir");
    rig.front.stored(lease()).await.expect("stored");
    assert_eq!(
        (log.reads_of(&cas_action), log.reads_of(&command)),
        (2, 1),
        "the facts read both, the closure the Action"
    );
}
