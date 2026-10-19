//! How many descriptors the container driver's `collect` holds open while the CAS is
//! slow. Alone in its test binary: it counts this process's descriptors, so
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

/// Catches output files held open while the stores are slow: a file being stored
/// holds its descriptor until it is sent, so a tree of thousands of files would hold
/// thousands open (the lease's `nofile` limit is the daemon's too). The walk closes
/// each file once hashed, and a record names its file without holding it open: a file
/// is opened only to read it, at the moment it is sent. The stores wait on a gate;
/// once `UPLOADS_IN_FLIGHT` wait, the test watches the descriptors for 300 ms before
/// opening it. The slack is `collect`'s own descriptor (the upper directory).
#[tokio::test]
async fn no_output_file_is_open_while_the_stores_are_slow() {
    let upper = kbf_testkit::dir::TestDir::under(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("kbf-driver-container-upload-queue"),
        "upper",
    );
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
    let pool = kbf_daemon::cpu_pool::CpuPool::new(4);
    let walk = collect(
        &cas,
        &upper,
        &pool,
        "",
        &paths,
        OutputLimits::DEFAULT,
        &mut result,
    );
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
        held <= 1 + 2,
        "{held} descriptors held at once while {UPLOADS_IN_FLIGHT} stores wait"
    );
    let stored = cas
        .blobs
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    assert_eq!(stored, 300 + 2, "300 files, the Directory and the Tree");
}
