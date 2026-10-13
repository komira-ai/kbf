//! **Red-first stand-in**: the engine's API over the behaviour of the client it
//! replaces (`kbf_daemon::CasClient` before stage T1). One ByteStream call per blob, no
//! limit but the caller's, no shared reads, no FindMissing, no retry, tonic's default
//! windows and decode limit. The engine's tests (`tests/engine.rs`) run against it and
//! fail; the next commit replaces this file with the engine.

use std::sync::Arc;

use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;
use tonic::transport::{Channel, Endpoint};

use crate::cas::{Cas, CasError, Chunks, digest_of};
use crate::config::TransferConfig;
use crate::stats::{Counts, EngineStats, TransferStats, add};
use crate::wire::{Wire, WriteFailed, call_error, committed};

#[derive(Debug)]
pub struct Transfers {
    inner: Arc<Inner>,
    unscoped: Scope,
}

#[derive(Debug)]
struct Inner {
    wire: Wire,
    config: TransferConfig,
}

impl Transfers {
    #[must_use]
    pub fn new(endpoint: Endpoint, config: TransferConfig) -> Self {
        Self::over(endpoint.connect_lazy(), config)
    }

    #[must_use]
    pub fn over(channel: Channel, config: TransferConfig) -> Self {
        let inner = Arc::new(Inner {
            wire: Wire::new(channel, config.chunk_bytes.max(1)),
            config,
        });
        let unscoped = Scope::new(&inner, None);
        Self { inner, unscoped }
    }

    #[must_use]
    pub fn scope(&self, lease: &LeaseId) -> Scope {
        Scope::new(&self.inner, Some(*lease))
    }

    #[must_use]
    pub fn stats(&self) -> EngineStats {
        EngineStats::default()
    }
}

#[derive(Debug)]
pub struct Scope {
    inner: Arc<Inner>,
    counts: Arc<Counts>,
}

impl Scope {
    fn new(inner: &Arc<Inner>, lease: Option<LeaseId>) -> Self {
        Self {
            inner: Arc::clone(inner),
            counts: Arc::new(Counts::new(lease)),
        }
    }

    #[must_use]
    pub fn stats(&self) -> TransferStats {
        self.counts.stats()
    }
}

impl Cas for Scope {
    type Lease = Self;

    fn lease(self: &Arc<Self>, lease: &LeaseId) -> Self {
        Self::new(&self.inner, Some(*lease))
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        add(&self.counts.read_calls, 1);
        let fail = |status: tonic::Status| call_error(digest, &status);
        let mut messages = self.inner.wire.read(digest, 0).await.map_err(fail)?;
        let mut bytes = Vec::new();
        while let Some(message) = messages.message().await.map_err(fail)? {
            add(&self.counts.read_bytes, message.data.len() as u64);
            bytes.extend_from_slice(&message.data);
        }
        Ok(bytes)
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let digest = digest_of(&bytes);
        add(&self.counts.write_calls, 1);
        let size = self
            .inner
            .wire
            .write(&digest, &bytes)
            .await
            .map_err(|status| call_error(&digest, &status))?;
        committed(&digest, size)
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        add(&self.counts.write_calls, 1);
        match self.inner.wire.write_chunks(&digest, chunks).await {
            Ok(size) => committed(&digest, size),
            Err(WriteFailed::Call(status)) => Err(call_error(&digest, &status)),
            Err(WriteFailed::Read(e)) => {
                Err(CasError::Read(crate::cas::label(&digest), e.to_string()))
            }
        }
    }

    fn in_flight(&self) -> usize {
        self.inner.config.in_flight
    }
}

impl Cas for Transfers {
    type Lease = Scope;

    fn lease(self: &Arc<Self>, lease: &LeaseId) -> Scope {
        self.scope(lease)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.unscoped.get(digest).await
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.unscoped.put(bytes).await
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        self.unscoped.put_chunks(digest, chunks).await
    }

    fn in_flight(&self) -> usize {
        self.unscoped.in_flight()
    }
}
