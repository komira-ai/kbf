//! A sink for tests that keeps what it is given, in the order it is given it.

use std::io::Read;
use std::sync::Mutex;

use kbf_types::Digest;
use sha2::{Digest as _, Sha256};

use crate::digest;
use crate::outputs::Blobs;
use crate::put::PutError;
use crate::task::PackSink;

/// One PUT the sink accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Put {
    /// Pack `slot`, `len` bytes long.
    Pack { slot: usize, len: usize },
    /// Large object `slot`: the bytes read from the blob, counted and hashed as they streamed.
    Large {
        slot: usize,
        len: u64,
        sha256: String,
    },
}

/// A sink with `packs` pack slots and `large` large slots, which can refuse its pack PUTs or its
/// large PUTs.
pub struct Fake {
    /// The bodies of the pack PUTs it accepted, in order.
    pub stored: Mutex<Vec<Vec<u8>>>,
    /// Every PUT it accepted, in order.
    pub puts: Mutex<Vec<Put>>,
    /// Every PUT it was asked for, accepted or not, in order: whether it was a large object, and
    /// the slot.
    pub tried: Mutex<Vec<(bool, usize)>>,
    refuse: bool,
    refuse_large: bool,
    packs: usize,
    large: usize,
}

impl Fake {
    /// A sink of one pack slot and no large slot, which refuses every pack PUT when `refuse`.
    pub fn new(refuse: bool) -> Self {
        Self {
            stored: Mutex::new(Vec::new()),
            puts: Mutex::new(Vec::new()),
            tried: Mutex::new(Vec::new()),
            refuse,
            refuse_large: false,
            packs: 1,
            large: 0,
        }
    }

    /// A sink of `packs` pack slots and `large` large slots that accepts every PUT.
    pub fn with_slots(packs: usize, large: usize) -> Self {
        Self {
            packs,
            large,
            ..Self::new(false)
        }
    }

    /// The same sink, refusing every large PUT.
    pub fn refusing_large(self) -> Self {
        Self {
            refuse_large: true,
            ..self
        }
    }

    /// The same sink, refusing every pack PUT.
    pub fn refusing_packs(self) -> Self {
        Self {
            refuse: true,
            ..self
        }
    }

    /// The PUTs asked for so far, accepted or not.
    pub fn tried(&self) -> Vec<(bool, usize)> {
        self.tried.lock().unwrap().clone()
    }

    /// The PUTs accepted so far.
    pub fn accepted(&self) -> Vec<Put> {
        self.puts.lock().unwrap().clone()
    }
}

impl PackSink for Fake {
    fn pack_slots(&self) -> usize {
        self.packs
    }

    fn large_slots(&self) -> usize {
        self.large
    }

    fn store_pack(&self, slot: usize, body: &[u8]) -> Result<(), PutError> {
        self.tried.lock().unwrap().push((false, slot));
        if self.refuse {
            return Err(PutError::Exists {
                url: "https://store.example/pack".to_owned(),
            });
        }
        self.stored.lock().unwrap().push(body.to_vec());
        self.puts.lock().unwrap().push(Put::Pack {
            slot,
            len: body.len(),
        });
        Ok(())
    }

    fn store_large(&self, slot: usize, blobs: &Blobs, blob: &Digest) -> Result<(), PutError> {
        self.tried.lock().unwrap().push((true, slot));
        if self.refuse_large {
            return Err(PutError::Exists {
                url: "https://store.example/large".to_owned(),
            });
        }
        let mut hasher = Sha256::new();
        let mut len = 0;
        let mut reader = blobs.reader(&digest::to_reapi(blob)).unwrap();
        let mut chunk = vec![0; 1 << 16];
        loop {
            let n = reader.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            hasher.update(&chunk[..n]);
            len += n as u64;
        }
        self.puts.lock().unwrap().push(Put::Large {
            slot,
            len,
            sha256: hex::encode(hasher.finalize()),
        });
        Ok(())
    }
}
