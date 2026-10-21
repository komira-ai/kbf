//! For the unit tests of the single pass: [`Recording`], an intake that keeps what it
//! is handed and stores each finished file in a [`MemoryCas`] when it is finished, and
//! [`Taker`], a CAS over that store with that intake (or one that declines every file),
//! which records the digests it is asked to store with `put_records`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use kbf_daemon::cas::{Cas, CasError, Chunks, Intake, IntakeFile, Record, label, proto_digest};
use kbf_proto::reapi::Digest;

use crate::cas::MemoryCas;

/// One file handed to a [`Recording`].
#[derive(Debug, Default)]
pub(crate) struct Taken {
    /// The size it was opened with.
    pub size: u64,
    /// The length of each chunk written, in order.
    pub chunks: Vec<usize>,
    pub bytes: Vec<u8>,
    /// Its digest, once finished.
    pub finished: Option<kbf_types::Digest>,
}

/// An intake that keeps every file it is handed.
pub(crate) struct Recording {
    pub files: Mutex<Vec<Taken>>,
    /// How many times it was finished.
    pub finishes: AtomicUsize,
    /// Takes no file.
    decline: bool,
    cas: Arc<MemoryCas>,
}

impl Recording {
    pub(crate) fn new(cas: Arc<MemoryCas>, decline: bool) -> Arc<Self> {
        Arc::new(Self {
            files: Mutex::default(),
            finishes: AtomicUsize::new(0),
            decline,
            cas,
        })
    }

    fn files(&self) -> std::sync::MutexGuard<'_, Vec<Taken>> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The files finished, by digest label, with their chunk lengths.
    pub(crate) fn finished(&self) -> Vec<(String, Vec<usize>)> {
        self.files()
            .iter()
            .filter_map(|t| Some((label(&proto_digest(&t.finished?)), t.chunks.clone())))
            .collect()
    }

    /// How many files were handed over, finished or not.
    pub(crate) fn opened(&self) -> usize {
        self.files().len()
    }
}

/// The handle on one file of a [`Recording`].
struct File {
    index: usize,
    recording: Arc<Recording>,
}

impl IntakeFile for File {
    fn write(&mut self, chunk: &[u8]) {
        let mut files = self.recording.files();
        files[self.index].chunks.push(chunk.len());
        files[self.index].bytes.extend_from_slice(chunk);
    }

    fn finish(self: Box<Self>, digest: kbf_types::Digest, _: u32) {
        self.recording.files()[self.index].finished = Some(digest);
    }
}

/// The intake of a [`Taker`]: a [`Recording`] it hands out as itself.
pub(crate) struct Intaking(pub Arc<Recording>);

impl Intake for Intaking {
    fn file(&self, size: u64) -> Option<Box<dyn IntakeFile>> {
        if self.0.decline {
            return None;
        }
        let mut files = self.0.files();
        files.push(Taken {
            size,
            ..Taken::default()
        });
        Some(Box::new(File {
            index: files.len() - 1,
            recording: Arc::clone(&self.0),
        }))
    }

    fn finish(&self) -> BoxFuture<'_, Result<(), CasError>> {
        self.0.finishes.fetch_add(1, Ordering::Relaxed);
        for taken in self.0.files().iter().filter(|t| t.finished.is_some()) {
            self.0.cas.insert(taken.bytes.clone());
        }
        Box::pin(std::future::ready(Ok(())))
    }
}

/// A [`MemoryCas`] whose intake is a [`Recording`], recording what `put_records` and
/// `put_chunks` are asked to store.
pub(crate) struct Taker {
    pub cas: Arc<MemoryCas>,
    pub recording: Arc<Recording>,
    /// The digest labels `put_records` and `put_chunks` were given.
    pub put: Mutex<Vec<String>>,
}

impl Taker {
    /// A store whose intake takes every file, or (`decline`) none.
    pub(crate) fn new(decline: bool) -> Self {
        let cas = Arc::new(MemoryCas::new());
        Self {
            recording: Recording::new(Arc::clone(&cas), decline),
            cas,
            put: Mutex::default(),
        }
    }

    pub(crate) fn put(&self) -> Vec<String> {
        self.put
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(&self, labels: impl IntoIterator<Item = String>) {
        self.put
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(labels);
    }
}

impl Cas for Taker {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.cas.get(digest).await
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.cas.put(bytes).await
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        self.record([label(&digest)]);
        self.cas.put_chunks(digest, chunks).await
    }

    async fn put_records(&self, records: Vec<Record>) -> Result<(), CasError> {
        self.record(records.iter().map(|r| label(&proto_digest(&r.digest))));
        self.cas.put_records(records).await
    }

    fn intake(&self) -> Option<Arc<dyn Intake>> {
        Some(Arc::new(Intaking(Arc::clone(&self.recording))))
    }
}
