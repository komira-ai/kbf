//! Helpers shared by the kbf-storage tests: the clock, a metadata log that races a touch,
//! blob builders and the faulty store.

#![allow(dead_code)]

pub mod faulty;

use std::mem;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use kbf_meta::{Applied, Command, MetaState, Retention};
use kbf_objstore::MemoryStore;
use kbf_proto::reapi;
use kbf_storage::{Cache, MemoryMetaLog, MetaLog, MetaLogError};
use kbf_types::{Digest, FarmTime};

pub const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Farm time `n` days after the epoch.
pub fn days(n: u32) -> FarmTime {
    FarmTime::from_millis(0).saturating_add(DAY.saturating_mul(n))
}

/// A [`MemoryMetaLog`] that can run extra commands just before the next touch commits,
/// to play a collection that wins the race between a read and its touch.
#[derive(Debug)]
pub struct RaceLog {
    inner: MemoryMetaLog,
    before_touch: Mutex<Vec<Command>>,
}

impl RaceLog {
    fn new() -> Self {
        Self {
            inner: MemoryMetaLog::new(Retention::default()),
            before_touch: Mutex::new(Vec::new()),
        }
    }

    /// Commits `commands` just before the next `Command::Touch`.
    pub fn before_next_touch(&self, commands: Vec<Command>) {
        *self.lock() = commands;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Command>> {
        self.before_touch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl MetaLog for RaceLog {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        if matches!(command, Command::Touch(_)) {
            let racing = mem::take(&mut *self.lock());
            for c in racing {
                self.inner.commit(c).await?;
            }
        }
        self.inner.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.inner.query(f).await
    }
}

pub type TestCache = Cache<RaceLog, MemoryStore>;

/// A blob: its bytes and its digest in both forms.
#[derive(Clone, Debug)]
pub struct Blob {
    pub data: Vec<u8>,
    pub digest: Digest,
    pub proto: reapi::Digest,
}

impl Blob {
    pub fn new(data: impl Into<Vec<u8>>) -> Self {
        let data = data.into();
        let digest = kbf_segments::sha256(&data);
        let proto = reapi::Digest {
            hash: digest.hash_hex(),
            size_bytes: i64::try_from(digest.size_bytes).expect("size"),
        };
        Self {
            data,
            digest,
            proto,
        }
    }

    /// The protobuf encoding of `message` as a blob.
    pub fn of(message: &impl prost::Message) -> Self {
        Self::new(message.encode_to_vec())
    }
}

/// `len` deterministic pseudo-random bytes (xorshift64*) for seed `seed`.
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.extend_from_slice(&x.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    }
    out.truncate(len);
    out
}
