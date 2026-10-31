//! Fixtures of the tests of the endpoints: several fake endpoints over one store, a probe whose
//! answers a test sets, and a clock a test moves.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Semaphore;

use crate::{Backend, FaultyBackend, MemoryBackend, Op, TestClock};

use super::rank::rank;
use super::{Endpoints, EndpointsConfig, Probe};

/// A probe whose answers a test sets. An endpoint it was not told about is ready.
pub(in crate::server) struct FakeProbe {
    answers: Mutex<HashMap<String, bool>>,
    /// One permit for each time an endpoint was asked, for a test that waits for a round.
    pub(in crate::server) asked: Semaphore,
}

impl FakeProbe {
    pub(in crate::server) fn new() -> Self {
        Self {
            answers: Mutex::new(HashMap::new()),
            asked: Semaphore::new(0),
        }
    }

    /// From now on `endpoint` answers `ready`.
    pub(in crate::server) fn set(&self, endpoint: &str, ready: bool) {
        let mut answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        answers.insert(endpoint.to_owned(), ready);
    }
}

#[async_trait]
impl Probe for FakeProbe {
    async fn ready(&self, endpoint: &str) -> bool {
        self.asked.add_permits(1);
        let answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        answers.get(endpoint).copied().unwrap_or(true)
    }
}

/// The backoff of the fleet, in milliseconds.
pub(in crate::server) const BACKOFF_MS: u64 = 5_000;

/// The endpoints of a test: each is a [`FaultyBackend`] over the one store they share, as the
/// storage proxies share a store, so a test counts the calls of each and takes each down.
pub(in crate::server) struct Fleet {
    /// The store every endpoint reads.
    pub(in crate::server) store: Arc<MemoryBackend>,
    pub(in crate::server) faulty: Vec<Arc<FaultyBackend>>,
    /// The URLs, in the order of `faulty`.
    pub(in crate::server) names: Vec<String>,
    pub(in crate::server) clock: Arc<TestClock>,
    pub(in crate::server) probe: Arc<FakeProbe>,
    pub(in crate::server) endpoints: Arc<Endpoints>,
}

impl Fleet {
    /// `count` endpoints, all ready, with a backoff of [`BACKOFF_MS`] and a poll every two
    /// seconds.
    pub(in crate::server) fn new(count: usize) -> Self {
        let store = Arc::new(MemoryBackend::new());
        let faulty: Vec<Arc<FaultyBackend>> = (0..count)
            .map(|_| Arc::new(FaultyBackend::new(store.clone())))
            .collect();
        let names: Vec<String> = (1..=count)
            .map(|n| format!("http://192.0.2.{n}:9300"))
            .collect();
        let mut members: Vec<(String, Arc<dyn Backend>)> = Vec::new();
        for (name, backend) in names.iter().zip(&faulty) {
            members.push((name.clone(), backend.clone()));
        }
        let clock = Arc::new(TestClock::new());
        let probe = Arc::new(FakeProbe::new());
        let config = EndpointsConfig {
            unready_backoff: Duration::from_millis(BACKOFF_MS),
            readyz_interval: Duration::from_secs(2),
        };
        let endpoints = Endpoints::new(members, config, clock.clone(), probe.clone());
        Self {
            store,
            faulty,
            names,
            clock,
            probe,
            endpoints: Arc::new(endpoints),
        }
    }

    /// Puts an object in the store every endpoint reads.
    pub(in crate::server) async fn put(&self, key: &str, body: &[u8]) {
        let body = Bytes::copy_from_slice(body);
        self.store.put(key, body).await.expect("a put");
    }

    /// The places of the endpoints in the order `key` is tried.
    pub(in crate::server) fn order(&self, key: &str) -> Vec<usize> {
        rank(&self.names, key)
    }

    /// A key whose first endpoint is the one at `place`.
    pub(in crate::server) fn key_first_at(&self, place: usize) -> String {
        (0..)
            .map(|n| format!("cas/b/{n}"))
            .find(|key| self.order(key)[0] == place)
            .expect("an endless search finds a key")
    }

    /// The reads (`get`) that the endpoint at `place` has had.
    pub(in crate::server) fn gets(&self, place: usize) -> u64 {
        self.faulty[place].calls(Op::Get)
    }

    /// The `get` calls of every endpoint.
    pub(in crate::server) fn all_gets(&self) -> Vec<u64> {
        (0..self.faulty.len()).map(|n| self.gets(n)).collect()
    }
}
