//! The data plane's conformance checks (`kbf_cas_conformance`) against [`PackedCas`] over a fake
//! plane, twice: once with no pack holding any blob (every read goes to the plane underneath,
//! so the packed plane must be indistinguishable from it), and once with a plane whose writes
//! are packs and whose own reads find nothing (every read is served from a pack through the
//! index, hashed, and every other call is the plane's).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use kbf_cas_conformance::Subject;
use kbf_daemon::PackedCas;
use kbf_daemon::cas::{Cas, CasError, digest_of, label};
use kbf_objstore::{Capabilities, MemoryStore};
use kbf_pack::{Blob, PackKeys, Packer, PackerConfig, Reader, ReaderConfig, WriterKind};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, PackIndex};
use kbf_proto::reapi::Digest;
use kbf_state::ManualClock;
use kbf_types::LeaseId;

/// How a fake plane keeps what it is given: the packs of an object store and an index.
struct Packing {
    packer: Packer<MemoryStore>,
    index: Arc<MemoryPackIndex>,
}

/// A fake plane. Its lease is a view of itself. With `packing` its blobs are packs, which only
/// the reader of the packed plane reads; without, they are in memory, which only its own `get`
/// reads.
#[derive(Clone)]
struct Shelf {
    blobs: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    packing: Option<Arc<Packing>>,
}

/// The index's name for a wire digest.
fn name(digest: &Digest) -> BlobDigest {
    let bytes = hex::decode(&digest.hash).expect("hex");
    BlobDigest {
        hash: <[u8; 32]>::try_from(bytes).expect("32 bytes"),
        size: u64::try_from(digest.size_bytes).expect("a length"),
    }
}

impl Shelf {
    fn keep(&self, digest: &Digest, bytes: Vec<u8>) {
        let mut blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        blobs.insert(label(digest), bytes);
    }

    fn holds(&self, digest: &Digest) -> bool {
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        blobs.contains_key(&label(digest))
    }
}

impl Cas for Shelf {
    type Lease = Shelf;

    fn lease(self: &Arc<Self>, _: &LeaseId) -> Shelf {
        Shelf::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        let held = blobs.get(&label(digest)).cloned();
        held.ok_or_else(|| CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let digest = digest_of(&bytes);
        // An empty blob is kept in memory in both: a pack holds blobs of at least a byte.
        let Some(packing) = self.packing.as_ref().filter(|_| !bytes.is_empty()) else {
            self.keep(&digest, bytes);
            return Ok(digest);
        };
        let bytes = Bytes::from(bytes);
        let blob = Blob {
            digest: name(&digest),
            bytes,
        };
        packing.packer.pack(vec![blob]).await.expect("packed");
        Ok(digest)
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        let Some(packing) = &self.packing else {
            let absent = digests.iter().filter(|digest| !self.holds(digest));
            return absent.cloned().collect();
        };
        let named: Vec<BlobDigest> = digests.iter().map(name).collect();
        let absent = packing
            .index
            .find_missing(&named)
            .expect("the index answers");
        let wanted = |digest: &&Digest| absent.contains(&name(digest));
        digests.iter().filter(wanted).cloned().collect()
    }
}

/// A packed plane over a fake plane, with or without packs behind it.
struct Packed {
    packing: bool,
    scratch: PathBuf,
}

impl Packed {
    fn new(packing: bool, name: &str) -> Self {
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        std::fs::create_dir_all(&scratch).expect("scratch directory");
        Self { packing, scratch }
    }
}

impl Subject for Packed {
    type Cas = PackedCas<Shelf>;

    async fn fresh(&self) -> Arc<PackedCas<Shelf>> {
        let caps = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        let store = Arc::new(MemoryStore::new(caps));
        let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
        let shared: Arc<dyn PackIndex> = index.clone();
        let keys = PackKeys::new(WriterKind::Daemon, "node1", "boot1").expect("keys");
        let config = PackerConfig::default();
        let packer = Packer::new(Arc::clone(&store), Arc::clone(&shared), keys, config);
        let packing = Packing {
            packer: packer.expect("packer"),
            index,
        };
        let shelf = Shelf {
            blobs: Arc::default(),
            packing: self.packing.then(|| Arc::new(packing)),
        };
        let reader = Reader::new(store, shared, None, ReaderConfig::default());
        Arc::new(PackedCas::new(Arc::new(shelf), Arc::new(reader)))
    }

    fn scratch(&self) -> &Path {
        &self.scratch
    }
}

/// Every check, with no pack holding any blob.
mod with_no_pack {
    kbf_cas_conformance::tests!(super::Packed::new(false, "packed-conformance-empty"));
}

/// Every check, with the plane's writes packed and its own reads empty.
mod with_every_blob_packed {
    kbf_cas_conformance::tests!(super::Packed::new(true, "packed-conformance-packs"));
}
