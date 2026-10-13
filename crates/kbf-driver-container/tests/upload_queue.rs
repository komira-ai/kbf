//! How many output files the container driver's `collect` holds open at once while
//! the CAS is slow. Alone in its test binary: it counts this process's descriptors, so
//! no other test may open files beside it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use futures::future::Shared;
use futures::{FutureExt as _, StreamExt as _};
use kbf_daemon::cas::{Cas, CasError, Chunks, digest_of, label};
use kbf_driver_container::tree::{OutputLimits, collect};
use kbf_proto::reapi::{ActionResult, Digest};

/// Blobs in memory. Every store waits on `gate`, counted in `started` once it waits.
struct GatedCas {
    gate: Shared<futures::channel::oneshot::Receiver<()>>,
    started: AtomicUsize,
    blobs: Mutex<Vec<Digest>>,
}

impl GatedCas {
    async fn keep(&self, digest: Digest) -> Digest {
        self.started.fetch_add(1, Ordering::SeqCst);
        let _ = self.gate.clone().await;
        self.blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(digest.clone());
        digest
    }
}

/// The in-flight limit [`GatedCas`] answers: not the engine's default, so a walk that
/// kept a bound of its own would show.
const UPLOADS_IN_FLIGHT: usize = 16;

impl Cas for GatedCas {
    type Lease = std::sync::Arc<Self>;

    fn lease(self: &std::sync::Arc<Self>, _: &kbf_types::LeaseId) -> std::sync::Arc<Self> {
        std::sync::Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        Err(CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        Ok(self.keep(digest_of(&bytes)).await)
    }

    async fn put_chunks(&self, digest: Digest, mut chunks: Chunks) -> Result<Digest, CasError> {
        // Waits before reading: the file stays open while the store is slow.
        let digest = self.keep(digest).await;
        while let Some(chunk) = chunks.next().await {
            chunk.map_err(|e| CasError::Read(label(&digest), e.to_string()))?;
        }
        Ok(digest)
    }

    fn in_flight(&self) -> usize {
        UPLOADS_IN_FLIGHT
    }
}

/// This process's open descriptors (the listing's own included, the same each time).
fn open_descriptors() -> usize {
    std::fs::read_dir("/dev/fd").expect("list /dev/fd").count()
}

/// Catches a walk that opens every output file while the stores are slow: each queued
/// file holds its descriptor until it is stored, so a tree of thousands of files
/// would hold thousands open (the lease's `nofile` limit is the daemon's too). The
/// stores wait on a gate; once `UPLOADS_IN_FLIGHT` wait, the test watches the
/// descriptors for 300 ms (the walk has time to run ahead) before opening it. The
/// slack is the walk's own descriptors (the upper directory, the directory being
/// read). Mutant seen red: the queue's channel given room for 1_000_000
/// (`kbf-outputs/src/uploads.rs`, `mpsc::channel(0)`).
#[tokio::test]
async fn the_walk_holds_at_most_one_file_past_the_stores_in_flight() {
    let upper = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-driver-container-upload-queue")
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&upper);
    std::fs::create_dir_all(upper.join("out")).expect("mkdir");
    for f in 0..300 {
        std::fs::write(upper.join(format!("out/f{f:03}")), format!("file {f}")).expect("write");
    }
    let (open, gate) = futures::channel::oneshot::channel::<()>();
    let cas = GatedCas {
        gate: gate.shared(),
        started: AtomicUsize::new(0),
        blobs: Mutex::new(Vec::new()),
    };
    let before = open_descriptors();
    let mut result = ActionResult::default();
    let paths = ["out".to_owned()];
    let walk = collect(&cas, &upper, "", &paths, OutputLimits::DEFAULT, &mut result);
    let watch = async {
        // A deadline, so a walk that never starts the stores fails rather than hangs.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while cas.started.load(Ordering::SeqCst) < UPLOADS_IN_FLIGHT {
            assert!(
                std::time::Instant::now() < deadline,
                "{} of {UPLOADS_IN_FLIGHT} stores started in 30 s",
                cas.started.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let mut most = 0;
        for _ in 0..150 {
            most = most.max(open_descriptors());
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let _ = open.send(());
        Ok(most)
    };
    let ((), most) = futures::try_join!(walk, watch).expect("collect");
    let held = most.saturating_sub(before);
    assert!(
        held >= UPLOADS_IN_FLIGHT,
        "{held}: the gate held the stores"
    );
    assert!(
        held <= UPLOADS_IN_FLIGHT + 1 + 4,
        "{held} descriptors held at once, {UPLOADS_IN_FLIGHT} stores in flight"
    );
    let stored = cas
        .blobs
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    assert_eq!(stored, 300 + 2, "300 files, the Directory and the Tree");
}
